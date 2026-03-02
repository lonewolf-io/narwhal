// SPDX-License-Identifier: BSD-3-Clause

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::thread;

use anyhow::anyhow;
use async_channel::{Sender, bounded};
use monoio::net::TcpListener;

use futures::{FutureExt, select};
use libc::{
  SIG_BLOCK, SIG_SETMASK, SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, pthread_sigmask, sigaddset,
  sigemptyset, sigset_t,
};
use tracing::{error, info, trace, warn};

use narwhal_common::conn::{ConnRuntime, Dispatcher, DispatcherFactory};
use narwhal_common::service::{M2sService, S2mService, Service};

use crate::config::*;
use crate::conn::{M2sDispatcher, M2sDispatcherFactory, S2mDispatcher, S2mDispatcherFactory};

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

/// Type alias for S2M listener.
pub type S2mListener<M> = Listener<S2mDispatcher<M>, S2mDispatcherFactory<M>, S2mService>;

/// Type alias for M2S listener.
pub type M2sListener = Listener<M2sDispatcher, M2sDispatcherFactory, M2sService>;

/// Modulator server listener.
///
/// Spawns one or more worker threads, each with its own monoio event loop.
/// For TCP the workers bind to the same port via `SO_REUSEPORT`; for Unix
/// domain sockets a single worker is used.
pub struct Listener<D, DF, ST>
where
  D: Dispatcher,
  DF: DispatcherFactory<D>,
  ST: Service,
{
  /// The listener configuration.
  config: ListenerConfig,

  /// The connection runtime.
  conn_rt: ConnRuntime<ST>,

  /// The dispatcher factory.
  dispatcher_factory: DF,

  /// Worker thread handles.
  worker_handles: Vec<WorkerHandle>,

  /// The local address of the listener (only for TCP).
  local_address: Option<SocketAddr>,

  /// Phantom data for the dispatcher type.
  _dispatcher: std::marker::PhantomData<D>,
}

// === impl Listener ===

impl<D, DF, ST> Listener<D, DF, ST>
where
  D: Dispatcher,
  DF: DispatcherFactory<D>,
  ST: Service,
{
  /// Creates a new listener.
  ///
  /// # Arguments
  ///
  /// * `config` - The configuration for the listener
  /// * `conn_rt` - The connection runtime that will handle established connections
  /// * `dispatcher_factory` - Factory for creating dispatchers for new connections
  ///
  /// # Returns
  ///
  /// Returns a new `Listener` instance that is ready to be bootstrapped.
  pub fn new(mut config: ListenerConfig, conn_rt: ConnRuntime<ST>, dispatcher_factory: DF) -> Self {
    if config.workers_count == 0 {
      config.workers_count = core_affinity::get_core_ids().map(|c| c.len()).unwrap_or(1).max(1);
    }

    let workers_count = config.workers_count;

    Listener {
      config,
      conn_rt,
      dispatcher_factory,
      worker_handles: Vec::with_capacity(workers_count),
      local_address: None,
      _dispatcher: std::marker::PhantomData,
    }
  }

  /// Bootstraps the listener, starting to accept incoming connections.
  ///
  /// Spawns worker threads with dedicated monoio runtimes, each pinned to
  /// a CPU core.  For TCP, every worker creates its own listener via
  /// `SO_REUSEPORT`.  For Unix domain sockets a single worker is used.
  pub async fn bootstrap(&mut self) -> anyhow::Result<()> {
    assert!(self.worker_handles.is_empty(), "listener already bootstrapped");

    // Bootstrap the dispatcher factory and connection runtime.
    let mut dispatcher_factory = self.dispatcher_factory.clone();
    dispatcher_factory.bootstrap().await?;

    let conn_rt = self.conn_rt.clone();
    conn_rt.bootstrap().await?;

    match self.config.network.as_str() {
      TCP_NETWORK => {
        self.bootstrap_tcp(conn_rt, dispatcher_factory).await?;
      },
      UNIX_NETWORK => {
        self.bootstrap_unix(conn_rt, dispatcher_factory).await?;
      },
      _ => {
        anyhow::bail!("unsupported network type: {}", self.config.network);
      },
    }

    Ok(())
  }

  /// Returns the local address that the listener is bound to.
  ///
  /// Only meaningful for TCP listeners.  Returns `None` before
  /// `bootstrap()` or for Unix domain sockets.
  pub fn local_address(&self) -> Option<SocketAddr> {
    self.local_address
  }

  /// Gracefully shuts down the listener and all worker threads.
  pub async fn shutdown(&mut self) -> anyhow::Result<()> {
    match self.config.network.as_str() {
      TCP_NETWORK => {
        info!(
          network = TCP_NETWORK,
          address = self.config.bind_address,
          service_type = ST::NAME,
          "stopped accepting socket connections",
        );
      },
      UNIX_NETWORK => {
        // Remove the socket file.
        let socket_path = Path::new(&self.config.socket_path);
        if socket_path.exists() {
          fs::remove_file(socket_path)
            .map_err(|e| anyhow!("failed to remove socket file {}: {}", self.config.socket_path, e))?;
        }
        info!(
          network = UNIX_NETWORK,
          socket_path = self.config.socket_path,
          service_type = ST::NAME,
          "stopped accepting socket connections",
        );
      },
      _ => unreachable!("unsupported network type: {}", self.config.network),
    }

    // Shutdown workers, then the connection runtime and dispatcher factory.
    self.conn_rt.shutdown().await?;
    self.dispatcher_factory.shutdown().await?;

    self.shutdown_workers().await?;

    Ok(())
  }

  async fn bootstrap_tcp(&mut self, conn_rt: ConnRuntime<ST>, dispatcher_factory: DF) -> anyhow::Result<()> {
    let mut bind_address: SocketAddr = self
      .config
      .bind_address
      .parse()
      .map_err(|e| anyhow!("invalid bind address '{}': {}", self.config.bind_address, e))?;

    // When port is 0 the OS picks a random port.
    //
    // For multiple workers we discover the port up front so that every
    // SO_REUSEPORT worker binds to the same port.
    //
    // For a single worker we create a plain listener (no SO_REUSEPORT).
    let mut pre_created_listener: Option<std::net::TcpListener> = None;

    if bind_address.port() == 0 {
      if self.config.workers_count == 1 {
        let listener = create_plain_tcp_listener(bind_address)?;
        bind_address = listener.local_addr().map_err(|e| anyhow!("failed to get local address: {}", e))?;
        pre_created_listener = Some(listener);
      } else {
        let discovered = discover_available_port(bind_address)?;
        bind_address.set_port(discovered);
      }
    }

    self.local_address = Some(bind_address);

    let ready_rxs = self.spawn_tcp_workers(bind_address, conn_rt, dispatcher_factory, pre_created_listener)?;

    // Wait for every worker to signal that it is ready to accept.
    for rx in ready_rxs {
      let _ = rx.recv().await;
    }

    info!(
      network = TCP_NETWORK,
      address = %bind_address,
      service_type = ST::NAME,
      worker_threads = self.config.workers_count,
      "accepting socket connections",
    );

    Ok(())
  }

  fn spawn_tcp_workers(
    &mut self,
    bind_address: SocketAddr,
    conn_rt: ConnRuntime<ST>,
    dispatcher_factory: DF,
    mut pre_created_listener: Option<std::net::TcpListener>,
  ) -> anyhow::Result<Vec<async_channel::Receiver<()>>> {
    let mut ready_rxs = Vec::with_capacity(self.config.workers_count);

    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    if core_ids.is_empty() {
      warn!(service_type = ST::NAME, "no CPU cores detected, workers will run without core affinity");
    } else {
      trace!(
        worker_count = self.config.workers_count,
        available_cores = core_ids.len(),
        service_type = ST::NAME,
        "spawning TCP worker threads with CPU core affinity",
      );
    }

    // Block signals before spawning workers
    let old_mask = block_signals();

    for worker_id in 0..self.config.workers_count {
      let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
      let (ready_tx, ready_rx) = bounded::<()>(1);
      ready_rxs.push(ready_rx);

      let core_id = if !core_ids.is_empty() { Some(core_ids[worker_id % core_ids.len()]) } else { None };

      let conn_rt = conn_rt.clone();
      let dispatcher_factory = dispatcher_factory.clone();
      let worker_listener = pre_created_listener.take();

      let handle = thread::Builder::new().name(format!("{}-tcp-worker-{}", ST::NAME, worker_id)).spawn(
        move || -> anyhow::Result<()> {
          trace!(worker_id, service_type = ST::NAME, "worker thread started");

          if let Some(core_id) = core_id {
            let _ = core_affinity::set_for_current(core_id);
          }

          let mut rt =
            monoio::RuntimeBuilder::<monoio::FusionDriver>::new().enable_all().build().map_err(|e| anyhow!("failed to create monoio runtime for worker {}: {}", worker_id, e))?;

          rt.block_on(async move {
            let listener = if let Some(std_listener) = worker_listener {
              match TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                  error!(worker_id, error = ?e, service_type = ST::NAME, "failed to create listener from pre-created socket");
                  return;
                },
              }
            } else {
              match create_reusable_tcp_listener(bind_address) {
                Ok(l) => l,
                Err(e) => {
                  error!(worker_id, error = ?e, service_type = ST::NAME, "failed to create TCP listener");
                  return;
                },
              }
            };

            // Signal that this worker is ready to accept.
            let _ = ready_tx.send(()).await;

            // Accept loop.
            loop {
              select! {
                result = listener.accept().fuse() => {
                  match result {
                    Ok((tcp_stream, remote_addr)) => {
                      trace!(worker_id, %remote_addr, service_type = ST::NAME, "accepted TCP connection");

                      let conn_rt = conn_rt.clone();
                      let dispatcher_factory = dispatcher_factory.clone();

                      monoio::spawn(async move {
                        conn_rt.run_connection(tcp_stream, dispatcher_factory).await;
                      });
                    }
                    Err(e) => {
                      warn!(worker_id, error = ?e, service_type = ST::NAME, "TCP accept error");
                    }
                  }
                }
                _ = shutdown_rx.recv().fuse() => {
                  break;
                }
              }
            }
          });

          trace!(worker_id, service_type = ST::NAME, "worker thread stopped");
          Ok(())
        },
      )?;

      self.worker_handles.push(WorkerHandle { thread_handle: handle, worker_id, shutdown_tx });
    }

    // Restore the original signal mask.
    restore_signals(old_mask);

    Ok(ready_rxs)
  }

  async fn bootstrap_unix(&mut self, conn_rt: ConnRuntime<ST>, dispatcher_factory: DF) -> anyhow::Result<()> {
    if self.config.socket_path.is_empty() {
      anyhow::bail!("unix network selected but socket_path is not set");
    }

    // Remove stale socket file if it exists.
    let socket_path = Path::new(&self.config.socket_path);
    if socket_path.exists() {
      fs::remove_file(socket_path)
        .map_err(|e| anyhow!("failed to remove socket file {}: {}", self.config.socket_path, e))?;
    }

    if self.config.workers_count > 1 {
      warn!(
        requested = self.config.workers_count,
        actual = 1,
        service_type = ST::NAME,
        "unix sockets use a single worker thread; ignoring requested worker count",
      );
    }

    let ready_rxs = self.spawn_unix_worker(conn_rt, dispatcher_factory)?;

    for rx in ready_rxs {
      let _ = rx.recv().await;
    }

    info!(
      network = UNIX_NETWORK,
      socket_path = self.config.socket_path,
      service_type = ST::NAME,
      "accepting socket connections",
    );

    Ok(())
  }

  fn spawn_unix_worker(
    &mut self,
    conn_rt: ConnRuntime<ST>,
    dispatcher_factory: DF,
  ) -> anyhow::Result<Vec<async_channel::Receiver<()>>> {
    let socket_path = self.config.socket_path.clone();

    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    let core_id = core_ids.first().copied();

    let old_mask = block_signals();

    let worker_id: usize = 0;
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
    let (ready_tx, ready_rx) = bounded::<()>(1);

    let handle =
      thread::Builder::new().name(format!("{}-unix-worker-0", ST::NAME)).spawn(move || -> anyhow::Result<()> {
        trace!(worker_id, service_type = ST::NAME, "unix worker thread started");

        if let Some(core_id) = core_id {
          let _ = core_affinity::set_for_current(core_id);
        }

        let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
          .enable_all()
          .build()
          .map_err(|e| anyhow!("failed to create monoio runtime for unix worker: {}", e))?;

        rt.block_on(async move {
          let listener = match create_unix_listener(&socket_path) {
            Ok(l) => l,
            Err(e) => {
              error!(worker_id, error = ?e, service_type = ST::NAME, "failed to create Unix listener");
              return;
            },
          };

          // Signal that this worker is ready to accept.
          let _ = ready_tx.send(()).await;

          // Accept loop.
          loop {
            select! {
              result = listener.accept().fuse() => {
                match result {
                  Ok((unix_stream, _)) => {
                    trace!(worker_id, service_type = ST::NAME, "accepted Unix connection");

                    let conn_rt = conn_rt.clone();
                    let dispatcher_factory = dispatcher_factory.clone();

                    monoio::spawn(async move {
                      conn_rt.run_connection(unix_stream, dispatcher_factory).await;
                    });
                  }
                  Err(e) => {
                    warn!(worker_id, error = ?e, service_type = ST::NAME, "Unix accept error");
                  }
                }
              }
              _ = shutdown_rx.recv().fuse() => {
                break;
              }
            }
          }
        });

        trace!(worker_id, service_type = ST::NAME, "unix worker thread stopped");
        Ok(())
      })?;

    self.worker_handles.push(WorkerHandle { thread_handle: handle, worker_id, shutdown_tx });

    restore_signals(old_mask);

    Ok(vec![ready_rx])
  }

  async fn shutdown_workers(&mut self) -> anyhow::Result<()> {
    if self.worker_handles.is_empty() {
      return Ok(());
    }

    trace!(worker_count = self.worker_handles.len(), service_type = ST::NAME, "shutting down worker threads",);

    // Signal all workers to stop.
    for worker in &self.worker_handles {
      if let Err(e) = worker.shutdown_tx.send(()).await {
        warn!(
          worker_id = worker.worker_id,
          error = ?e,
          service_type = ST::NAME,
          "failed to send shutdown signal to worker",
        );
      }
    }

    // Join all worker threads.
    let handles = std::mem::take(&mut self.worker_handles);

    for worker in handles {
      match worker.thread_handle.join() {
        Ok(Ok(())) => {
          trace!(worker_id = worker.worker_id, service_type = ST::NAME, "worker thread joined successfully");
        },
        Ok(Err(e)) => {
          error!(
            worker_id = worker.worker_id,
            error = ?e,
            service_type = ST::NAME,
            "worker thread returned error",
          );
        },
        Err(e) => {
          warn!(
            worker_id = worker.worker_id,
            error = ?e,
            service_type = ST::NAME,
            "worker thread panicked",
          );
        },
      }
    }

    trace!(service_type = ST::NAME, "all worker threads stopped");

    Ok(())
  }
}

/// Blocks delivery of common signals on the calling thread and returns
/// the previous signal mask so it can be restored afterwards.
///
/// This is called before spawning worker threads so that they inherit
/// the blocked mask and do not intercept SIGINT / SIGTERM etc.
fn block_signals() -> sigset_t {
  unsafe {
    let mut new_mask: sigset_t = std::mem::zeroed();
    sigemptyset(&mut new_mask);
    sigaddset(&mut new_mask, SIGINT);
    sigaddset(&mut new_mask, SIGTERM);
    sigaddset(&mut new_mask, SIGQUIT);
    sigaddset(&mut new_mask, SIGHUP);
    sigaddset(&mut new_mask, SIGUSR1);
    sigaddset(&mut new_mask, SIGUSR2);
    sigaddset(&mut new_mask, SIGPIPE);

    let mut old_mask: sigset_t = std::mem::zeroed();
    pthread_sigmask(SIG_BLOCK, &new_mask, &mut old_mask);

    old_mask
  }
}

/// Restores the signal mask that was saved by [`block_signals`].
fn restore_signals(old_mask: sigset_t) {
  unsafe {
    pthread_sigmask(SIG_SETMASK, &old_mask, std::ptr::null_mut());
  }
}

/// Discovers an available port by temporarily binding to `addr` with
/// `SO_REUSEPORT` enabled.  Used when the configured port is `0`.
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

/// Creates a `SO_REUSEPORT`-enabled TCP listener suitable for the
/// thread-per-core pattern.
fn create_reusable_tcp_listener(addr: SocketAddr) -> anyhow::Result<TcpListener> {
  use socket2::{Domain, Protocol, Socket, Type};

  let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };

  let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
  socket.set_reuse_address(true)?;
  socket.set_reuse_port(true)?;
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
fn create_plain_tcp_listener(addr: SocketAddr) -> anyhow::Result<std::net::TcpListener> {
  let listener = std::net::TcpListener::bind(addr)?;
  listener.set_nonblocking(true)?;
  Ok(listener)
}

fn create_unix_listener(socket_path: &str) -> anyhow::Result<monoio::net::UnixListener> {
  let std_listener = std::os::unix::net::UnixListener::bind(socket_path)
    .map_err(|e| anyhow!("failed to bind to Unix socket {}: {}", socket_path, e))?;
  std_listener.set_nonblocking(true)?;

  let listener =
    monoio::net::UnixListener::from_std(std_listener).map_err(|e| anyhow!("failed to wrap Unix listener: {}", e))?;

  Ok(listener)
}
