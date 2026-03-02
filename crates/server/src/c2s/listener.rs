// SPDX-License-Identifier: BSD-3-Clause

use std::net::SocketAddr;

use std::sync::Arc;
use std::thread;

use anyhow::{Ok, anyhow};
use async_channel::{Sender, bounded};
use futures::{FutureExt, select};
use libc::{
  SIG_BLOCK, SIG_SETMASK, SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, pthread_sigmask, sigaddset,
  sigemptyset, sigset_t,
};
use monoio::net::TcpListener;
use monoio_rustls::TlsAcceptor;
use rustls::ServerConfig;
use tracing::{error, info, trace, warn};

use narwhal_common::conn::DispatcherFactory;
use narwhal_common::service::{C2sService, Service};

use crate::c2s::config::ListenerConfig;
use crate::c2s::conn::{C2sConnRuntime, C2sDispatcherFactory};
use crate::util;
use crate::util::tls::{create_tls_config, generate_self_signed_cert};

const LOCALHOST_DOMAIN: &str = "localhost";

/// Handle for a worker thread.
///
/// Tracks the thread join handle and provides a channel to signal
/// the worker to shut down gracefully.
struct WorkerHandle {
  /// The thread join handle.
  thread_handle: thread::JoinHandle<anyhow::Result<()>>,

  /// The worker identifier.
  worker_id: usize,

  /// Channel to signal the worker to shutdown.
  shutdown_tx: Sender<()>,
}

/// A TLS-enabled TCP listener for client-to-server (C2S) connections.
///
/// The `Listener` manages incoming client connections, handling TLS negotiation
/// and connection management. It supports both self-signed certificates for
/// localhost development and proper TLS certificates for production use.
///
/// The listener works in conjunction with a connection runtime to handle
/// individual client connections after they are established.
pub struct C2sListener {
  /// The configuration for the C2S listener.
  config: ListenerConfig,

  /// The connection runtime.
  conn_rt: C2sConnRuntime,

  /// The dispatcher factory.
  dispatcher_factory: C2sDispatcherFactory,

  /// The local address of the listener.
  local_address: Option<SocketAddr>,

  /// Worker thread handles.
  worker_handles: Vec<WorkerHandle>,
}

// ===== impl C2sListener =====

impl C2sListener {
  /// Creates a new C2S listener with the given configuration and connection manager.
  ///
  /// # Arguments
  ///
  /// * `config` - The configuration for the C2S listener
  /// * `conn_rt` - The connection runtime that will handle established connections
  /// * `dispatcher_factory` - The dispatcher factory that will create new dispatchers
  ///
  /// # Returns
  ///
  /// Returns a new `C2sListener` instance that is ready to be bootstrapped.
  pub fn new(config: ListenerConfig, conn_rt: C2sConnRuntime, dispatcher_factory: C2sDispatcherFactory) -> Self {
    let workers_count = if config.workers_count == 0 {
      // Default to number of available CPU cores
      core_affinity::get_core_ids().map(|cores| cores.len()).unwrap_or(1).max(1)
    } else {
      config.workers_count
    };

    Self { config, conn_rt, dispatcher_factory, local_address: None, worker_handles: Vec::with_capacity(workers_count) }
  }

  /// Bootstraps the listener, starting to accept incoming connections.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the listener was successfully started.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The connection manager fails to bootstrap
  /// * TLS configuration fails (invalid certificates or keys)
  /// * Unable to bind to the configured address and port
  pub async fn bootstrap(&mut self) -> anyhow::Result<()> {
    let worker_count = self.worker_handles.capacity();
    assert!(worker_count > 0, "worker_count must be greater than 0");

    // Load TLS config
    let tls_config = self.load_tls_config()?;

    // Parse bind address for workers
    let mut bind_address: SocketAddr = self.get_address().parse()?;

    // When port is 0 the OS picks a random port.
    //
    // For multiple workers we discover the port up front so that every
    // SO_REUSEPORT worker binds to the same port.
    //
    // For a single worker we create a plain listener (no SO_REUSEPORT).
    let mut pre_created_listener: Option<std::net::TcpListener> = None;

    if bind_address.port() == 0 {
      if worker_count == 1 {
        let listener = create_plain_listener(bind_address)?;
        bind_address = listener.local_addr()?;
        pre_created_listener = Some(listener);
      } else {
        let discovered = discover_available_port(bind_address)?;
        bind_address.set_port(discovered);
      }
    }

    self.local_address = Some(bind_address);

    // Bootstrap the connection runtime.
    self.dispatcher_factory.bootstrap().await?;
    self.conn_rt.bootstrap().await?;

    // Spawn worker threads and wait for them to be ready.
    let ready_rxs = self.spawn_workers(
      worker_count,
      bind_address,
      tls_config,
      self.conn_rt.clone(),
      self.dispatcher_factory.clone(),
      pre_created_listener,
    )?;

    for rx in ready_rxs {
      let _ = rx.recv().await;
    }

    info!(
      address = self.get_address(),
      domain = self.config.domain,
      service_type = C2sService::NAME,
      worker_threads = worker_count,
      "accepting socket connections",
    );

    Ok(())
  }

  /// Gracefully shuts down the listener.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the shutdown was successful.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * Unable to send the shutdown signal
  /// * The connection manager fails to shut down properly
  pub async fn shutdown(&mut self) -> anyhow::Result<()> {
    info!(
      address = self.get_address(),
      domain = self.config.domain,
      service_type = C2sService::NAME,
      "stopped accepting socket connections"
    );

    // Wait for the connection runtime to stop.
    self.conn_rt.shutdown().await?;
    self.dispatcher_factory.shutdown().await?;

    // Shutdown worker threads.
    self.shutdown_workers().await?;

    Ok(())
  }

  /// Returns the local address that the listener is bound to.
  ///
  /// This method will return `None` if called before `bootstrap()` or if
  /// the listener failed to bind to an address.
  ///
  /// # Returns
  ///
  /// Returns the socket address that the listener is bound to, if available.
  pub fn local_address(&self) -> Option<SocketAddr> {
    self.local_address
  }

  fn load_tls_config(&self) -> anyhow::Result<Arc<ServerConfig>> {
    let is_localhost = self.config.domain == LOCALHOST_DOMAIN;

    if self.config.cert_file.is_empty() || self.config.key_file.is_empty() {
      if !is_localhost {
        return Err(anyhow!("certificate and key files must be specified for non-localhost domains"));
      }
      warn!(domain = "localhost", service_type = C2sService::NAME, "using self-signed certificate");

      let (certs, key) = generate_self_signed_cert(vec![LOCALHOST_DOMAIN.to_string()])?;

      return create_tls_config(certs, key);
    }
    info!(
      domain = self.config.domain,
      cert_file = self.config.cert_file,
      key_file = self.config.key_file,
      service_type = C2sService::NAME,
      "loading certificate and key files"
    );

    let certs = util::tls::load_certs(&self.config.cert_file)?;
    let key = util::tls::load_private_key(&self.config.key_file)?;

    create_tls_config(certs, key)
  }

  fn get_address(&self) -> String {
    format!("{}:{}", self.config.bind_address, self.config.port)
  }

  /// Spawns worker threads with dedicated runtime and CPU core affinity.
  ///
  /// Each worker thread is pinned to a specific CPU core for better cache locality and
  /// reduced context switching. Workers are distributed across available cores using
  /// round-robin if there are more workers than cores.
  ///
  /// # Arguments
  ///
  /// * `worker_count` - Number of worker threads to spawn
  /// * `bind_address` - The socket address for workers to bind to
  /// * `tls_config` - Shared TLS configuration
  /// * `conn_rt` - Connection runtime for registering connections
  /// * `dispatcher_factory` - Factory for creating connection dispatchers
  fn spawn_workers(
    &mut self,
    worker_count: usize,
    bind_address: SocketAddr,
    tls_config: Arc<ServerConfig>,
    conn_rt: C2sConnRuntime,
    dispatcher_factory: C2sDispatcherFactory,
    mut pre_created_listener: Option<std::net::TcpListener>,
  ) -> anyhow::Result<Vec<async_channel::Receiver<()>>> {
    let mut ready_rxs = Vec::with_capacity(worker_count);

    // Get available CPU cores for affinity
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    let available_cores = core_ids.len();

    if available_cores == 0 {
      warn!(service_type = C2sService::NAME, "no CPU cores detected, workers will run without core affinity");
    } else {
      trace!(
        worker_count = worker_count,
        available_cores = available_cores,
        service_type = C2sService::NAME,
        "spawning worker threads with CPU core affinity"
      );
    }

    // Block signals before creating worker threads so they don't capture SIGINT/SIGTERM
    let old_mask = {
      let mut new_mask: sigset_t = unsafe { std::mem::zeroed() };
      unsafe {
        sigemptyset(&mut new_mask);
        sigaddset(&mut new_mask, SIGINT);
        sigaddset(&mut new_mask, SIGTERM);
        sigaddset(&mut new_mask, SIGQUIT);
        sigaddset(&mut new_mask, SIGHUP);
        sigaddset(&mut new_mask, SIGUSR1);
        sigaddset(&mut new_mask, SIGUSR2);
        sigaddset(&mut new_mask, SIGPIPE);
      }

      let mut old_mask: sigset_t = unsafe { std::mem::zeroed() };
      unsafe {
        pthread_sigmask(SIG_BLOCK, &new_mask, &mut old_mask);
      }

      old_mask
    };

    for worker_id in 0..worker_count {
      let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

      let (ready_tx, ready_rx) = bounded::<()>(1);
      ready_rxs.push(ready_rx);

      // Determine which core to pin this worker to (round-robin if more workers than cores)
      let core_id = if !core_ids.is_empty() { Some(core_ids[worker_id % core_ids.len()]) } else { None };

      // Clone shared data for this worker
      let tls_config = tls_config.clone();
      let conn_rt = conn_rt.clone();
      let dispatcher_factory = dispatcher_factory.clone();
      let worker_listener = pre_created_listener.take();

      let handle =
        thread::Builder::new().name(format!("c2s-worker-{}", worker_id)).spawn(move || -> anyhow::Result<()> {
          trace!(worker_id = worker_id, service_type = C2sService::NAME, "worker thread started");

          // Pin this thread to a specific CPU core for better cache locality
          if let Some(core_id) = core_id {
            let _ = core_affinity::set_for_current(core_id);
          }

          // Create a thread-local monoio runtime
          let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
            .enable_all()
            .build()
            .map_err(|e| anyhow!("failed to create runtime for c2s worker {}: {}", worker_id, e))?;

          rt.block_on(async move {
            let listener = if let Some(std_listener) = worker_listener {
              match TcpListener::from_std(std_listener) {
                std::result::Result::Ok(l) => l,
                Err(e) => {
                  warn!(worker_id = worker_id, error = ?e, service_type = C2sService::NAME, "failed to create listener from pre-created socket");
                  return;
                },
              }
            } else {
              match create_reusable_listener(bind_address).await {
                std::result::Result::Ok(l) => l,
                Err(e) => {
                  warn!(worker_id = worker_id, error = ?e, service_type = C2sService::NAME, "failed to create listener");
                  return;
                },
              }
            };

            // Signal that this worker's is ready to accept.
            let _ = ready_tx.send(()).await;

            let acceptor = TlsAcceptor::from(tls_config.clone());

            // Accept loop
            loop {
              select! {
                result = listener.accept().fuse() => {
                  match result {
                    std::result::Result::Ok((tcp_stream, remote_addr)) => {
                      trace!(worker_id = worker_id, %remote_addr, service_type = C2sService::NAME, "accepted connection");

                      let acceptor = acceptor.clone();

                      let conn_rt = conn_rt.clone();
                      let dispatcher_factory = dispatcher_factory.clone();

                      monoio::spawn(async move {
                        // Perform TLS handshake
                        match acceptor.accept(tcp_stream).await {
                          std::result::Result::Ok(tls_stream) => {
                            trace!(worker_id = worker_id, %remote_addr, "TLS handshake complete");

                            conn_rt.run_connection(tls_stream, dispatcher_factory).await;
                          }
                          Err(e) => {
                            warn!(worker_id = worker_id, %remote_addr, error = ?e, service_type = C2sService::NAME, "TLS handshake failed");
                          }
                        }
                      });
                    }
                    Err(e) => {
                      warn!(worker_id = worker_id, error = ?e, service_type = C2sService::NAME, "accept error");
                    }
                  }
                }
                _ = shutdown_rx.recv().fuse() => {
                  break;
                }
              }
            }
          });

          trace!(worker_id = worker_id, service_type = C2sService::NAME, "worker thread stopped");

          std::result::Result::Ok(())
        })?;

      self.worker_handles.push(WorkerHandle { thread_handle: handle, worker_id, shutdown_tx });
    }

    // Restore the original signal mask for the main thread
    unsafe {
      pthread_sigmask(SIG_SETMASK, &old_mask, std::ptr::null_mut());
    }

    std::result::Result::Ok(ready_rxs)
  }

  /// Shuts down all worker threads gracefully.
  ///
  /// Signals each worker to stop and waits for all threads to complete.
  /// Handles thread panics gracefully without propagating them.
  async fn shutdown_workers(&mut self) -> anyhow::Result<()> {
    if self.worker_handles.is_empty() {
      return std::result::Result::Ok(());
    }

    trace!(worker_count = self.worker_handles.len(), service_type = C2sService::NAME, "shutting down worker threads");

    // Signal all workers to shutdown
    for worker in &self.worker_handles {
      if let Err(e) = worker.shutdown_tx.send(()).await {
        warn!(
          worker_id = worker.worker_id,
          error = ?e,
          service_type = C2sService::NAME,
          "failed to send shutdown signal to worker"
        );
      }
    }

    // Join all worker threads
    let handles = std::mem::take(&mut self.worker_handles);

    for worker in handles {
      match worker.thread_handle.join() {
        std::result::Result::Ok(std::result::Result::Ok(())) => {
          trace!(worker_id = worker.worker_id, service_type = C2sService::NAME, "worker thread joined successfully");
        },
        std::result::Result::Ok(Err(e)) => {
          error!(
            worker_id = worker.worker_id,
            error = ?e,
            service_type = C2sService::NAME,
            "worker thread returned error"
          );
        },
        Err(e) => {
          warn!(
            worker_id = worker.worker_id,
            error = ?e,
            service_type = C2sService::NAME,
            "worker thread panicked"
          );
        },
      }
    }

    trace!(service_type = C2sService::NAME, "all worker threads stopped");

    std::result::Result::Ok(())
  }
}

fn discover_available_port(addr: SocketAddr) -> anyhow::Result<u16> {
  use socket2::{Domain, Protocol, Socket, Type};

  let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
  let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
  socket.set_reuse_address(true)?;
  socket.set_reuse_port(true)?;
  socket.bind(&addr.into())?;
  socket.listen(1)?;

  let local_addr = socket.local_addr()?;
  let local_addr: SocketAddr = local_addr.as_socket().ok_or_else(|| anyhow!("failed to resolve socket address"))?;

  Ok(local_addr.port())
}

async fn create_reusable_listener(addr: SocketAddr) -> anyhow::Result<TcpListener> {
  use socket2::{Domain, Protocol, Socket, Type};

  let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };

  let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

  socket.set_reuse_address(true)?; // allows binding to recently-used addresses
  socket.set_reuse_port(true)?; // allows multiple listeners on same port

  socket.set_nonblocking(true)?;
  socket.bind(&addr.into())?;
  socket.listen(1024)?;

  let std_listener: std::net::TcpListener = socket.into();
  std_listener.set_nonblocking(true)?;

  let listener = TcpListener::from_std(std_listener)?;

  Ok(listener)
}

/// Creates a plain TCP listener **without** `SO_REUSEPORT`.
///
/// Used for the single-worker case so that the OS-assigned port is
/// exclusively owned by this listener, preventing collisions when
/// multiple listeners bind to port 0 concurrently.
fn create_plain_listener(addr: SocketAddr) -> anyhow::Result<std::net::TcpListener> {
  let listener = std::net::TcpListener::bind(addr)?;
  listener.set_nonblocking(true)?;
  Ok(listener)
}
