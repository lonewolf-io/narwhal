// SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_broadcast;
use async_channel;
use monoio::net::TcpStream;
use monoio_rustls::{ClientTlsStream, TlsConnector};
use pki_types::ServerName;

use narwhal_client::monoio::s2m::S2mClient;
use narwhal_modulator::{Modulator, OutboundPrivatePayload};
use narwhal_protocol::{
  AclAction, AclType, BroadcastParameters, ConnectParameters, IdentifyParameters, JoinChannelParameters,
  LeaveChannelParameters, Message, SetChannelAclParameters, SetChannelConfigurationParameters,
};
use narwhal_server::c2s;
use narwhal_server::channel::ChannelManager;
use narwhal_server::notifier::Notifier;
use narwhal_server::router::GlobalRouter;
use narwhal_util::string_atom::StringAtom;

use crate::TestConn;

type TlsStream = ClientTlsStream<TcpStream>;

/// A test suite for the c2s server.
pub struct C2sSuite {
  /// The server configuration.
  config: Arc<c2s::Config>,

  /// The server listener.
  ln: c2s::C2sListener,

  /// The local router.
  local_router: c2s::Router,

  /// The M2S payload receiver.
  m2s_payload_rx: Option<async_broadcast::Receiver<OutboundPrivatePayload>>,

  /// The M2S payload router task handle.
  m2s_router_task_handle: Option<(monoio::task::JoinHandle<()>, async_channel::Sender<()>)>,

  /// Authenticated clients by username.
  clients: HashMap<String, TestConn<TlsStream>>,
}

// ===== impl C2sSuite =====

impl C2sSuite {
  pub async fn default() -> anyhow::Result<Self> {
    Self::new(c2s::Config::default()).await
  }

  pub async fn new(config: c2s::Config) -> anyhow::Result<Self> {
    Self::with_modulator(config, None, None).await
  }

  pub async fn with_modulator(
    config: c2s::Config,
    s2m_client: Option<S2mClient>,
    m2s_payload_rx: Option<async_broadcast::Receiver<OutboundPrivatePayload>>,
  ) -> anyhow::Result<Self> {
    let arc_config = Arc::new(config);

    let local_domain = StringAtom::from("localhost");
    let c2s_router = c2s::Router::new(local_domain.clone());

    let global_router = GlobalRouter::new(c2s_router.clone());

    let modulator = s2m_client.clone().map(|s2m_client| Arc::new(s2m_client) as Arc<dyn Modulator>);

    let notifier = Notifier::new(global_router.clone(), modulator.clone());

    let max_channels = arc_config.limits.max_channels;
    let max_clients_per_channel = arc_config.limits.max_clients_per_channel;
    let max_channels_per_client = arc_config.limits.max_channels_per_client;
    let max_payload_size = arc_config.limits.max_payload_size;

    let channel_mng = ChannelManager::new(
      global_router,
      notifier,
      max_channels,
      max_clients_per_channel,
      max_channels_per_client,
      max_payload_size,
    );

    let conn_cfg = narwhal_common::conn::Config {
      max_connections: arc_config.limits.max_connections,
      max_message_size: arc_config.limits.max_message_size,
      max_payload_size: arc_config.limits.max_payload_size,
      payload_pool_memory_budget: 8 * 1024, // 8MB default
      connect_timeout: arc_config.connect_timeout,
      authenticate_timeout: arc_config.authenticate_timeout,
      payload_read_timeout: arc_config.payload_read_timeout,
      outbound_message_queue_size: arc_config.limits.outbound_message_queue_size,
      request_timeout: arc_config.request_timeout,
      max_inflight_requests: arc_config.limits.max_inflight_requests,
      rate_limit: arc_config.limits.rate_limit,
    };

    let conn_rt = c2s::conn::C2sConnRuntime::new(conn_cfg).await;

    let dispatcher_factory =
      c2s::conn::C2sDispatcherFactory::new(arc_config.clone(), channel_mng.clone(), c2s_router.clone(), modulator)
        .await?;

    let ln = c2s::C2sListener::new(arc_config.listener.clone(), conn_rt.clone(), dispatcher_factory);

    Ok(Self {
      config: arc_config,
      ln,
      m2s_payload_rx,
      m2s_router_task_handle: None,
      local_router: c2s_router,
      clients: HashMap::new(),
    })
  }

  pub fn config(&self) -> Arc<c2s::Config> {
    self.config.clone()
  }

  pub async fn setup(&mut self) -> anyhow::Result<()> {
    if let Some(m2s_payload_rx) = self.m2s_payload_rx.take() {
      self.m2s_router_task_handle = Some(c2s::route_m2s_private_payload(m2s_payload_rx, self.local_router.clone()));
    }

    self.ln.bootstrap().await?;
    Ok(())
  }

  pub async fn teardown(&mut self) -> anyhow::Result<()> {
    self.ln.shutdown().await?;

    if let Some((handle, shutdown_tx)) = self.m2s_router_task_handle.take() {
      shutdown_tx.close();
      let _ = handle.await;
    }
    Ok(())
  }

  pub async fn tls_socket_connect(&self) -> anyhow::Result<TestConn<TlsStream>> {
    let domain = self.config.listener.domain.clone();
    assert_eq!(self.config.listener.domain, "localhost", "domain is not localhost");

    let addr = self.ln.local_address().expect("local address not set");

    let tcp_stream = TcpStream::connect(&addr).await?;

    let client_config = crate::tls::make_tls_client_config();
    let connector = TlsConnector::from(client_config);

    let server_name: ServerName<'static> =
      ServerName::try_from(domain).map_err(|e| anyhow::anyhow!("invalid server name: {}", e))?;

    let tls_stream = connector.connect(server_name, tcp_stream).await?;

    let max_message_size = self.config().limits.max_message_size as usize;

    let pool = narwhal_util::pool::Pool::new(1, max_message_size);

    let tls_socket = TestConn::new(tls_stream, pool.acquire_buffer().await, max_message_size);

    Ok(tls_socket)
  }

  #[allow(dead_code)]
  pub async fn identify(&mut self, username: &str) -> anyhow::Result<()> {
    let mut tls_socket = self.tls_socket_connect().await?;

    tls_socket
      .write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 }))
      .await?;

    let client_connected_msg = tls_socket.read_message().await?;
    assert!(
      matches!(client_connected_msg, Message::ConnectAck { .. }),
      "expected ConnectAck, got: {:?}",
      client_connected_msg
    );

    tls_socket.write_message(Message::Identify(IdentifyParameters { username: StringAtom::from(username) })).await?;

    let reply = tls_socket.read_message().await?;
    assert!(matches!(reply, Message::IdentifyAck { .. }), "expected IdentifyAck, got: {:?}", reply);

    self.clients.insert(username.to_string(), tls_socket);

    Ok(())
  }

  pub async fn join_channel(&mut self, username: &str, channel: &str, on_behalf: Option<&str>) -> anyhow::Result<()> {
    let on_behalf = on_behalf.map(StringAtom::from);

    self
      .write_message(
        username,
        Message::JoinChannel(JoinChannelParameters { id: 1234, channel: channel.into(), on_behalf }),
      )
      .await?;

    // Verify that the server sent the proper join channel ack message.
    let reply = self.read_message(username).await?;
    assert!(matches!(reply, Message::JoinChannelAck { .. }), "expected JoinChannelAck, got: {:?}", reply);

    Ok(())
  }

  pub async fn leave_channel(&mut self, username: &str, channel: &str) -> anyhow::Result<()> {
    self
      .write_message(
        username,
        Message::LeaveChannel(LeaveChannelParameters { id: 1234, channel: StringAtom::from(channel), on_behalf: None }),
      )
      .await?;

    // Verify that the server sent the proper leave channel ack message.
    let reply = self.read_message(username).await?;
    assert!(matches!(reply, Message::LeaveChannelAck { .. }));

    Ok(())
  }

  pub async fn configure_channel(
    &mut self,
    username: &str,
    channel: &str,
    max_clients: u32,
    max_payload_size: u32,
  ) -> anyhow::Result<()> {
    self
      .write_message(
        username,
        Message::SetChannelConfiguration(SetChannelConfigurationParameters {
          id: 1234,
          channel: StringAtom::from(channel),
          max_clients,
          max_payload_size,
        }),
      )
      .await?;

    // Verify that the server sent the proper response message.
    let reply = self.read_message(username).await?;
    assert!(matches!(reply, Message::SetChannelConfigurationAck { .. }));

    Ok(())
  }

  pub async fn set_channel_acl(
    &mut self,
    username: &str,
    channel: &str,
    acl_type: AclType,
    acl_action: AclAction,
    nids: Vec<StringAtom>,
  ) -> anyhow::Result<()> {
    self
      .write_message(
        username,
        Message::SetChannelAcl(SetChannelAclParameters {
          id: 1234,
          channel: StringAtom::from(channel),
          r#type: StringAtom::from(acl_type.as_str()),
          action: StringAtom::from(acl_action.as_str()),
          nids,
        }),
      )
      .await?;

    // Verify that the server sent the proper response message.
    let reply = self.read_message(username).await?;
    assert!(matches!(reply, Message::SetChannelAclAck { .. }));

    Ok(())
  }

  pub async fn broadcast(&mut self, username: &str, channel: &str, payload: &str) -> anyhow::Result<()> {
    self
      .write_message(
        username,
        Message::Broadcast(BroadcastParameters {
          id: 1234,
          channel: StringAtom::from(channel),
          qos: None,
          length: payload.len() as u32,
        }),
      )
      .await?;

    // ... along with the payload.
    self.write_raw_bytes(username, payload.as_bytes()).await?;
    self.write_raw_bytes(username, b"\n").await?;

    // Verify that the server sent the proper response message.
    let reply = self.read_message(username).await?;
    assert!(matches!(reply, Message::BroadcastAck { .. }));

    Ok(())
  }

  pub async fn write_raw_bytes(&mut self, username: &str, data: &[u8]) -> anyhow::Result<()> {
    let tls_socket = self.get_tls_socket(username)?;
    tls_socket.write_raw_bytes(data).await
  }

  pub async fn read_raw_bytes(&mut self, username: &str, data: &mut [u8]) -> anyhow::Result<()> {
    let tls_socket = self.get_tls_socket(username)?;
    tls_socket.read_raw_bytes(data).await?;
    Ok(())
  }

  pub async fn write_message(&mut self, username: &str, message: Message) -> anyhow::Result<()> {
    let tls_socket = self.get_tls_socket(username)?;
    tls_socket.write_message(message).await
  }

  pub async fn read_message(&mut self, username: &str) -> anyhow::Result<Message> {
    let tls_socket = self.get_tls_socket(username)?;
    tls_socket.read_message().await
  }

  pub async fn ignore_reply(&mut self, username: &str) -> anyhow::Result<()> {
    let _ = self.read_message(username).await?;
    Ok(())
  }

  pub async fn expect_read_timeout(&mut self, username: &str, timeout: Duration) -> anyhow::Result<()> {
    let tls_socket = self.get_tls_socket(username)?;
    tls_socket.expect_read_timeout(timeout).await
  }

  fn get_tls_socket(&mut self, username: &str) -> anyhow::Result<&mut TestConn<TlsStream>> {
    self.clients.get_mut(username).ok_or_else(|| anyhow!("client not found"))
  }
}

pub fn default_c2s_config() -> c2s::Config {
  c2s::Config {
    listener: c2s::ListenerConfig {
      port: 0, // use a random port
      workers_count: 1,
      ..Default::default()
    },
    limits: c2s::Limits {
      max_connections: 10,
      max_message_size: 256 * 1024,
      payload_pool_memory_budget: 512 * 1024,
      ..Default::default()
    },
    ..Default::default()
  }
}
