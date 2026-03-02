// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;

use async_broadcast;
use monoio::net::TcpStream;

use narwhal_modulator::{M2sListener, OutboundPrivatePayload};
use narwhal_protocol::{M2sConnectParameters, Message};

use crate::TestConn;

/// A test suite for the M2S server.
pub struct M2sSuite {
  /// The server configuration.
  config: Arc<narwhal_modulator::M2sServerConfig>,

  /// The modulator server listener.
  ln: M2sListener,

  /// The broadcast sender for outbound private payloads.
  payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,

  /// The broadcast receiver for outbound private payloads (optional, for testing).
  payload_rx: Option<async_broadcast::Receiver<OutboundPrivatePayload>>,
}

// ===== impl M2sSuite =====

impl M2sSuite {
  /// Creates a new M2sSuite with the given configuration.
  ///
  /// This is `async` because `ConnRuntime::new` (conn) is async.
  pub async fn with_config(config: narwhal_modulator::M2sServerConfig) -> Self {
    let arc_config = Arc::new(config);

    // Create the broadcast channel for outbound payloads
    let (payload_tx, payload_rx) = async_broadcast::broadcast(1024);

    let dispatcher_factory = narwhal_modulator::conn::M2sDispatcherFactory::new(arc_config.clone(), payload_tx.clone());

    let server_config = (*arc_config).clone();

    let conn_rt = narwhal_modulator::conn::M2sConnRuntime::new(&server_config).await;

    let ln = M2sListener::new(server_config.listener.clone(), conn_rt, dispatcher_factory);

    Self { config: arc_config, ln, payload_tx, payload_rx: Some(payload_rx) }
  }

  /// Returns the server configuration.
  pub fn config(&self) -> Arc<narwhal_modulator::M2sServerConfig> {
    self.config.clone()
  }

  /// Returns a clone of the broadcast sender for outbound payloads.
  pub fn payload_sender(&self) -> async_broadcast::Sender<OutboundPrivatePayload> {
    self.payload_tx.clone()
  }

  /// Takes the payload receiver if available (can only be called once).
  pub fn take_payload_receiver(&mut self) -> Option<async_broadcast::Receiver<OutboundPrivatePayload>> {
    self.payload_rx.take()
  }

  /// Sets up the test suite by bootstrapping the listener.
  pub async fn setup(&mut self) -> anyhow::Result<()> {
    self.ln.bootstrap().await?;
    Ok(())
  }

  /// Tears down the test suite by shutting down the listener.
  pub async fn teardown(&mut self) -> anyhow::Result<()> {
    self.ln.shutdown().await?;
    Ok(())
  }

  /// Creates a new socket connection to the server without authentication.
  ///
  /// Uses `monoio::net::TcpStream` so the returned `TestConn` works
  /// natively with monoio's completion-based I/O.
  pub async fn socket_connect(&self) -> anyhow::Result<TestConn<TcpStream>> {
    let addr = self.ln.local_address().expect("local address not set");

    let tcp_stream = TcpStream::connect(addr).await?;

    let max_message_size = self.config().limits.max_message_size as usize;

    let pool = narwhal_util::pool::Pool::new(1, max_message_size);

    let socket = TestConn::new(tcp_stream, pool.acquire_buffer().await, max_message_size);

    Ok(socket)
  }

  /// Creates a new authenticated connection to the server.
  ///
  /// # Arguments
  ///
  /// * `secret` - The secret to use for authentication (can be None if no secret is required)
  pub async fn connect(&mut self, secret: Option<&str>) -> anyhow::Result<TestConn<TcpStream>> {
    let mut socket = self.socket_connect().await?;

    socket
      .write_message(Message::M2sConnect(M2sConnectParameters {
        protocol_version: 1,
        secret: secret.map(Into::into),
        heartbeat_interval: 0,
      }))
      .await?;

    let client_connected_msg = socket.read_message().await?;
    assert!(matches!(client_connected_msg, Message::M2sConnectAck { .. }));

    Ok(socket)
  }

  /// Creates a new authenticated connection to the server with a custom heartbeat interval.
  ///
  /// # Arguments
  ///
  /// * `secret` - The secret to use for authentication (can be None if no secret is required)
  /// * `heartbeat_interval` - The heartbeat interval in milliseconds
  pub async fn connect_with_heartbeat(
    &mut self,
    secret: Option<&str>,
    heartbeat_interval: u32,
  ) -> anyhow::Result<TestConn<TcpStream>> {
    let mut socket = self.socket_connect().await?;

    socket
      .write_message(Message::M2sConnect(M2sConnectParameters {
        protocol_version: 1,
        secret: secret.map(Into::into),
        heartbeat_interval,
      }))
      .await?;

    let client_connected_msg = socket.read_message().await?;
    assert!(matches!(client_connected_msg, Message::M2sConnectAck { .. }));

    Ok(socket)
  }
}

/// Creates a default M2S configuration for testing.
pub fn default_m2s_config() -> narwhal_modulator::M2sServerConfig {
  narwhal_modulator::M2sServerConfig {
    listener: narwhal_modulator::ListenerConfig {
      network: narwhal_modulator::TCP_NETWORK.to_string(),
      bind_address: "127.0.0.1:0".to_string(), // use a random port
      workers_count: 1,
      ..Default::default()
    },
    limits: narwhal_modulator::Limits {
      max_connections: 10,
      max_message_size: 256 * 1024,
      payload_pool_memory_budget: 512 * 1024,
      ..Default::default()
    },
    ..Default::default()
  }
}
