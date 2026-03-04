// SPDX-License-Identifier: BSD-3-Clause

use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::anyhow;
use async_channel::Sender;
use monoio::net::TcpListener;

use futures::{FutureExt, select};
use tracing::{error, info, trace, warn};

use narwhal_common::conn::{ConnRuntime, Dispatcher, DispatcherFactory};
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_common::service::{M2sService, S2mService, Service};

use crate::config::*;
use crate::conn::{M2sDispatcher, M2sDispatcherFactory, S2mDispatcher, S2mDispatcherFactory};

/// Type alias for S2M listener.
pub type S2mListener<M> = Listener<S2mDispatcher<M>, S2mDispatcherFactory<M>, S2mService>;

/// Type alias for M2S listener.
pub type M2sListener = Listener<M2sDispatcher, M2sDispatcherFactory, M2sService>;

/// Modulator server listener.
///
/// Accepts connections on per-core runtimes managed by a `CoreDispatcher`.
/// For TCP the accept loops bind to the same port via `SO_REUSEPORT`, one per
/// shard.  For Unix domain sockets a single accept loop runs on shard 0.
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

  /// The core dispatcher that owns the worker threads.
  core_dispatcher: CoreDispatcher,

  /// The local address of the listener (only for TCP).
  local_address: Option<SocketAddr>,

  /// Channels to signal accept loops to stop.
  shutdown_txs: Vec<Sender<()>>,

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
  pub fn new(
    config: ListenerConfig,
    conn_rt: ConnRuntime<ST>,
    dispatcher_factory: DF,
    core_dispatcher: CoreDispatcher,
  ) -> Self {
    Listener {
      config,
      conn_rt,
      dispatcher_factory,
      core_dispatcher,
      local_address: None,
      shutdown_txs: Vec::new(),
      _dispatcher: std::marker::PhantomData,
    }
  }

  /// Bootstraps the listener, starting to accept incoming connections.
  pub async fn bootstrap(&mut self) -> anyhow::Result<()> {
    assert!(self.shutdown_txs.is_empty(), "listener already bootstrapped");

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
  /// Only meaningful for TCP listeners.
  pub fn local_address(&self) -> Option<SocketAddr> {
    self.local_address
  }

  /// Gracefully shuts down the listener.
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

    // Signal all accept loops to stop.
    for shutdown_tx in self.shutdown_txs.drain(..) {
      let _ = shutdown_tx.send(()).await;
    }

    self.conn_rt.shutdown().await?;
    self.dispatcher_factory.shutdown().await?;

    Ok(())
  }

  async fn bootstrap_tcp(&mut self, conn_rt: ConnRuntime<ST>, dispatcher_factory: DF) -> anyhow::Result<()> {
    let mut bind_address: SocketAddr = self
      .config
      .bind_address
      .parse()
      .map_err(|e| anyhow!("invalid bind address '{}': {}", self.config.bind_address, e))?;

    let shard_count = self.core_dispatcher.shard_count();

    // When port is 0 the OS picks a random port.
    //
    // For a single shard we create a plain listener (no SO_REUSEPORT) to get a
    // stable port.  For multiple shards we discover the port up front so all
    // SO_REUSEPORT listeners bind to the same port.
    let mut pre_created_listener: Option<std::net::TcpListener> = None;

    if bind_address.port() == 0 {
      if shard_count == 1 {
        let listener = create_plain_tcp_listener(bind_address)?;
        bind_address = listener.local_addr().map_err(|e| anyhow!("failed to get local address: {}", e))?;
        pre_created_listener = Some(listener);
      } else {
        let discovered = discover_available_port(bind_address)?;
        bind_address.set_port(discovered);
      }
    }

    self.local_address = Some(bind_address);

    let mut ready_rxs = Vec::with_capacity(shard_count);

    for shard_id in 0..shard_count {
      let (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1);
      let (ready_tx, ready_rx) = async_channel::bounded::<()>(1);

      self.shutdown_txs.push(shutdown_tx);
      ready_rxs.push(ready_rx);

      let conn_rt = conn_rt.clone();
      let dispatcher_factory = dispatcher_factory.clone();
      let worker_listener = pre_created_listener.take();

      self
        .core_dispatcher
        .dispatch_at_shard(shard_id, move || {
          run_tcp_accept_loop(
            shard_id,
            bind_address,
            worker_listener,
            conn_rt,
            dispatcher_factory,
            ready_tx,
            shutdown_rx,
          )
        })
        .await?;
    }

    for rx in ready_rxs {
      let _ = rx.recv().await;
    }

    info!(
      network = TCP_NETWORK,
      address = %bind_address,
      service_type = ST::NAME,
      worker_threads = shard_count,
      "accepting socket connections",
    );

    Ok(())
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

    let (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1);
    let (ready_tx, ready_rx) = async_channel::bounded::<()>(1);

    self.shutdown_txs.push(shutdown_tx);

    let socket_path_str = self.config.socket_path.clone();

    self
      .core_dispatcher
      .dispatch_at_shard(0, move || {
        run_unix_accept_loop(socket_path_str, conn_rt, dispatcher_factory, ready_tx, shutdown_rx)
      })
      .await?;

    let _ = ready_rx.recv().await;

    info!(
      network = UNIX_NETWORK,
      socket_path = self.config.socket_path,
      service_type = ST::NAME,
      "accepting socket connections",
    );

    Ok(())
  }
}

async fn run_tcp_accept_loop<D, DF, ST>(
  worker_id: usize,
  bind_address: SocketAddr,
  pre_created_listener: Option<std::net::TcpListener>,
  conn_rt: ConnRuntime<ST>,
  dispatcher_factory: DF,
  ready_tx: async_channel::Sender<()>,
  shutdown_rx: async_channel::Receiver<()>,
) where
  D: Dispatcher,
  DF: DispatcherFactory<D>,
  ST: Service,
{
  let listener = if let Some(std_listener) = pre_created_listener {
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

  let _ = ready_tx.send(()).await;

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

  trace!(worker_id, service_type = ST::NAME, "TCP accept loop stopped");
}

async fn run_unix_accept_loop<D, DF, ST>(
  socket_path: String,
  conn_rt: ConnRuntime<ST>,
  dispatcher_factory: DF,
  ready_tx: async_channel::Sender<()>,
  shutdown_rx: async_channel::Receiver<()>,
) where
  D: Dispatcher,
  DF: DispatcherFactory<D>,
  ST: Service,
{
  let listener = match create_unix_listener(&socket_path) {
    Ok(l) => l,
    Err(e) => {
      error!(error = ?e, service_type = ST::NAME, "failed to create Unix listener");
      return;
    },
  };

  let _ = ready_tx.send(()).await;

  loop {
    select! {
      result = listener.accept().fuse() => {
        match result {
          Ok((unix_stream, _)) => {
            trace!(service_type = ST::NAME, "accepted Unix connection");

            let conn_rt = conn_rt.clone();
            let dispatcher_factory = dispatcher_factory.clone();

            monoio::spawn(async move {
              conn_rt.run_connection(unix_stream, dispatcher_factory).await;
            });
          }
          Err(e) => {
            warn!(error = ?e, service_type = ST::NAME, "Unix accept error");
          }
        }
      }
      _ = shutdown_rx.recv().fuse() => {
        break;
      }
    }
  }

  trace!(service_type = ST::NAME, "Unix accept loop stopped");
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
/// Used for the single-shard case so that the OS-assigned port is
/// exclusively owned by this listener.
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
