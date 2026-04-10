// SPDX-License-Identifier: BSD-3-Clause

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Ok, anyhow};
use async_channel::{Sender, bounded};
use futures::{FutureExt, select};
use narwhal_common::runtime::TcpListener;

use compio_tls::TlsAcceptor;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use rustls::ServerConfig;
use tracing::{info, trace, warn};

use narwhal_common::conn::DispatcherFactory;
use narwhal_common::runtime;
use narwhal_common::service::{C2sService, Service};

use crate::c2s::config::ListenerConfig;
use crate::c2s::conn::{C2sConnRuntime, C2sDispatcherFactory};
use crate::channel::store::{ChannelStore, MessageLogFactory};
use crate::util;
use crate::util::tls::{create_tls_config, generate_self_signed_cert};
use narwhal_common::core_dispatcher::CoreDispatcher;

const LOCALHOST_DOMAIN: &str = "localhost";

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabel {
  result: &'static str,
}

const SUCCESS: ResultLabel = ResultLabel { result: "success" };
const FAILURE: ResultLabel = ResultLabel { result: "failure" };

#[derive(Clone)]
struct Metrics {
  connections_accepted: Counter,
  tls_handshakes: Family<ResultLabel, Counter>,
}

impl Metrics {
  fn register(registry: &mut Registry) -> Self {
    let connections_accepted = Counter::default();
    registry.register("connections_accepted", "TCP connections accepted", connections_accepted.clone());
    let tls_handshakes = Family::<ResultLabel, Counter>::default();
    registry.register("tls_handshakes", "TLS handshake results", tls_handshakes.clone());
    Self { connections_accepted, tls_handshakes }
  }
}

/// A TLS-enabled TCP listener for client-to-server (C2S) connections.
///
/// The `C2sListener` manages incoming client connections, handling TLS negotiation
/// and connection management. Accept loops are dispatched to per-core runtimes
/// via a `CoreDispatcher`.
pub struct C2sListener<CS: ChannelStore, MLF: MessageLogFactory> {
  /// The configuration for the C2S listener.
  config: ListenerConfig,

  /// The connection runtime.
  conn_rt: C2sConnRuntime,

  /// The dispatcher factory.
  dispatcher_factory: C2sDispatcherFactory<CS, MLF>,

  /// The core dispatcher.
  core_dispatcher: CoreDispatcher,

  /// The local address of the listener.
  local_address: Option<SocketAddr>,

  /// Shutdown senders for each accept loop — one per shard.
  shutdown_txs: Vec<Sender<()>>,

  /// Listener metrics.
  metrics: Metrics,
}

// === impl C2sListener ===

impl<CS: ChannelStore, MLF: MessageLogFactory> C2sListener<CS, MLF> {
  /// Creates a new C2S listener with the given configuration and connection manager.
  ///
  /// # Arguments
  ///
  /// * `config` - The configuration for the C2S listener
  /// * `conn_rt` - The connection runtime that will handle established connections
  /// * `dispatcher_factory` - The dispatcher factory that will create new dispatchers
  /// * `core_dispatcher` - The core dispatcher for spawning accept loops on per-core runtimes
  /// * `registry` - The metrics registry used to register listener-level metrics
  ///
  /// # Returns
  ///
  /// Returns a new `C2sListener` instance that is ready to be bootstrapped.
  pub fn new(
    config: ListenerConfig,
    conn_rt: C2sConnRuntime,
    dispatcher_factory: C2sDispatcherFactory<CS, MLF>,
    core_dispatcher: CoreDispatcher,
    registry: &mut Registry,
  ) -> Self {
    let metrics = Metrics::register(registry);

    Self {
      config,
      conn_rt,
      dispatcher_factory,
      core_dispatcher,
      local_address: None,
      shutdown_txs: Vec::new(),
      metrics,
    }
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
    let worker_count = self.core_dispatcher.shard_count();

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

    // Spawn accept loops on each shard's runtime.
    for worker_id in 0..worker_count {
      let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
      let (ready_tx, ready_rx) = bounded::<()>(1);

      let tls_config = tls_config.clone();
      let conn_rt = self.conn_rt.clone();
      let dispatcher_factory = self.dispatcher_factory.clone();
      let worker_listener = if worker_id == 0 { pre_created_listener.take() } else { None };
      let metrics = self.metrics.clone();

      self
        .core_dispatcher
        .dispatch_at_shard(worker_id, move || {
          run_accept_loop(
            worker_id,
            bind_address,
            worker_listener,
            tls_config,
            conn_rt,
            dispatcher_factory,
            ready_tx,
            shutdown_rx,
            metrics,
          )
        })
        .await?;

      // Wait for this worker to be ready before spawning the next.
      let _ = ready_rx.recv().await;

      self.shutdown_txs.push(shutdown_tx);
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

    // Signal all accept loops to stop.
    for tx in &self.shutdown_txs {
      let _ = tx.send(()).await;
    }

    Ok(())
  }

  /// Returns the local address that the listener is bound to.
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
}

#[allow(clippy::too_many_arguments)]
async fn run_accept_loop<CS: ChannelStore, MLF: MessageLogFactory>(
  worker_id: usize,
  bind_address: SocketAddr,
  pre_created_listener: Option<std::net::TcpListener>,
  tls_config: Arc<ServerConfig>,
  conn_rt: C2sConnRuntime,
  dispatcher_factory: C2sDispatcherFactory<CS, MLF>,
  ready_tx: async_channel::Sender<()>,
  shutdown_rx: async_channel::Receiver<()>,
  metrics: Metrics,
) {
  let listener = if let Some(std_listener) = pre_created_listener {
    match TcpListener::from_std(std_listener) {
      std::result::Result::Ok(l) => l,
      Err(e) => {
        warn!(worker_id, error = ?e, service_type = C2sService::NAME, "failed to create listener from pre-created socket");
        return;
      },
    }
  } else {
    match create_reusable_listener(bind_address).await {
      std::result::Result::Ok(l) => l,
      Err(e) => {
        warn!(worker_id, error = ?e, service_type = C2sService::NAME, "failed to create listener");
        return;
      },
    }
  };

  // Signal that this worker is ready to accept.
  let _ = ready_tx.send(()).await;

  let acceptor = TlsAcceptor::from(tls_config);

  // Accept loop
  loop {
    select! {
      result = listener.accept().fuse() => {
        match result {
          std::result::Result::Ok((tcp_stream, remote_addr)) => {
            trace!(worker_id, %remote_addr, service_type = C2sService::NAME, "accepted connection");

            metrics.connections_accepted.inc();

            let acceptor = acceptor.clone();
            let conn_rt = conn_rt.clone();
            let dispatcher_factory = dispatcher_factory.clone();
            let metrics = metrics.clone();

            runtime::spawn_detached(async move {
              match acceptor.accept(tcp_stream).await {
                std::result::Result::Ok(tls_stream) => {
                  trace!(worker_id, %remote_addr, "TLS handshake complete");
                  metrics.tls_handshakes.get_or_create(&SUCCESS).inc();
                  conn_rt.run_connection(tls_stream, dispatcher_factory).await;
                }
                Err(e) => {
                  warn!(worker_id, %remote_addr, error = ?e, service_type = C2sService::NAME, "TLS handshake failed");
                  metrics.tls_handshakes.get_or_create(&FAILURE).inc();
                }
              }
            });
          }
          Err(e) => {
            warn!(worker_id, error = ?e, service_type = C2sService::NAME, "accept error");
          }
        }
      }
      _ = shutdown_rx.recv().fuse() => {
        break;
      }
    }
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
