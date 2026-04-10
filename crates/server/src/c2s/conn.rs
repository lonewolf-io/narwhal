// SPDX-License-Identifier: BSD-3-Clause

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_lock::RwLock;
use async_trait::async_trait;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use tracing::{error, trace};

use narwhal_common::conn::{ConnTx, State};
use narwhal_common::service::C2sService;
use narwhal_modulator::modulator::{
  AuthRequest, AuthResult, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResult, Modulator, Operation,
  SendPrivatePayloadRequest, SendPrivatePayloadResult,
};
use narwhal_protocol::ErrorReason::{
  BadRequest, Forbidden, InternalServerError, UnexpectedMessage, UnsupportedProtocolVersion, UsernameInUse,
};
use narwhal_protocol::{
  AclAction, AclType, AuthAckParameters, ConnectAckParameters, IdentifyAckParameters, Message, ModDirectAckParameters,
};
use narwhal_protocol::{ChannelId, Nid};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use crate::c2s::{self, Config};
use crate::channel::store::{ChannelStore, MessageLogFactory};
use crate::channel::{ChannelConfig, ChannelManager};
use crate::transmitter::{Resource, Transmitter};

/// The C2S connection runtime.
pub type C2sConnRuntime = narwhal_common::conn::ConnRuntime<C2sService>;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabel {
  result: &'static str,
}

const SUCCESS: ResultLabel = ResultLabel { result: "success" };
const FAILURE: ResultLabel = ResultLabel { result: "failure" };

/// Metric handles for a C2sDispatcher instance.
#[derive(Clone)]
struct C2sDispatcherMetrics {
  auth_attempts: Family<ResultLabel, Counter>,
  identify_attempts: Family<ResultLabel, Counter>,
  messages_received: Counter,
  broadcast_payload_bytes: Histogram,
  modulator_forward_payload: Family<ResultLabel, Counter>,
}

impl C2sDispatcherMetrics {
  fn register(registry: &mut Registry) -> Self {
    let auth_attempts = Family::default();
    registry.register("auth_attempts", "Authentication attempts", auth_attempts.clone());
    let identify_attempts = Family::default();
    registry.register("identify_attempts", "Identification attempts", identify_attempts.clone());
    let messages_received = Counter::default();
    registry.register("messages_received", "Messages received from clients", messages_received.clone());
    let broadcast_payload_bytes =
      Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(64.0, 2.0, 12));
    registry.register("broadcast_payload_bytes", "Broadcast payload sizes in bytes", broadcast_payload_bytes.clone());
    let modulator_forward_payload = Family::default();
    registry.register(
      "modulator_forward_payload",
      "Modulator payload forward results",
      modulator_forward_payload.clone(),
    );
    Self { auth_attempts, identify_attempts, messages_received, broadcast_payload_bytes, modulator_forward_payload }
  }
}

impl std::fmt::Debug for C2sDispatcherMetrics {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("C2sDispatcherMetrics").finish_non_exhaustive()
  }
}

#[derive(Clone)]
/// A transmitter implementation for C2S connections.
///
/// This struct wraps a connection transmitter (`ConnTx`) along with the handler
/// identifier to provide a `Transmitter` interface for C2S connections.
struct C2sTransmitter {
  /// The local handler identifier for this connection
  handler: usize,

  /// The underlying connection transmitter
  conn_tx: ConnTx,
}

impl C2sTransmitter {
  fn new(handler: usize, conn_tx: ConnTx) -> Self {
    Self { handler, conn_tx }
  }
}

impl Transmitter for C2sTransmitter {
  fn send_message(&self, message: Message) {
    self.conn_tx.send_message(message);
  }

  fn send_message_with_payload(&self, message: Message, payload_opt: Option<PoolBuffer>) {
    self.conn_tx.send_message_with_payload(message, payload_opt);
  }

  fn resource(&self) -> Resource {
    Resource { domain: None, handler: self.handler }
  }
}

impl std::fmt::Debug for C2sTransmitter {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("C2sTransmitter").field("local_handler", &self.handler).finish()
  }
}

pub struct C2sDispatcherFactory<CS: ChannelStore, MLF: MessageLogFactory> {
  inner: Arc<RwLock<C2sDispatcherFactoryInner<CS, MLF>>>,
  metrics: C2sDispatcherMetrics,
}

impl<CS: ChannelStore, MLF: MessageLogFactory> Clone for C2sDispatcherFactory<CS, MLF> {
  fn clone(&self) -> Self {
    Self { inner: self.inner.clone(), metrics: self.metrics.clone() }
  }
}

impl<CS: ChannelStore, MLF: MessageLogFactory> std::fmt::Debug for C2sDispatcherFactory<CS, MLF> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("C2sDispatcherFactory").finish_non_exhaustive()
  }
}

// === impl C2sDispatcherFactory ===

impl<CS: ChannelStore, MLF: MessageLogFactory> C2sDispatcherFactory<CS, MLF> {
  /// Creates a new C2S `C2sDispatcherFactory`.
  pub fn new(
    config: Arc<Config>,
    channel_manager: ChannelManager<CS, MLF>,
    c2s_router: c2s::Router,
    modulator: Option<Arc<dyn Modulator>>,
    auth_required: bool,
    registry: &mut Registry,
  ) -> Self {
    let inner = C2sDispatcherFactoryInner { config, channel_manager, c2s_router, modulator, auth_required };

    Self { inner: Arc::new(RwLock::new(inner)), metrics: C2sDispatcherMetrics::register(registry) }
  }
}

#[async_trait(?Send)]
impl<CS: ChannelStore, MLF: MessageLogFactory> narwhal_common::conn::DispatcherFactory<C2sDispatcher<CS, MLF>>
  for C2sDispatcherFactory<CS, MLF>
{
  async fn create(&mut self, handler: usize, tx: ConnTx) -> C2sDispatcher<CS, MLF> {
    let inner = self.inner.read().await;

    C2sDispatcher::new(
      handler,
      inner.config.clone(),
      inner.auth_required,
      inner.modulator.clone(),
      inner.channel_manager.clone(),
      inner.c2s_router.clone(),
      tx,
      self.metrics.clone(),
    )
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    Ok(())
  }
}

pub struct C2sDispatcherFactoryInner<CS: ChannelStore, MLF: MessageLogFactory> {
  /// The C2S configuration.
  config: Arc<Config>,

  /// The channel manager.
  channel_manager: ChannelManager<CS, MLF>,

  /// The C2S router.
  c2s_router: c2s::Router,

  /// The modulator, if any.
  modulator: Option<Arc<dyn Modulator>>,

  /// Whether authentication is required.
  auth_required: bool,
}

pub struct C2sDispatcher<CS: ChannelStore, MLF: MessageLogFactory>(Option<C2sDispatcherInner<CS, MLF>>);

impl<CS: ChannelStore, MLF: MessageLogFactory> Default for C2sDispatcher<CS, MLF> {
  fn default() -> Self {
    Self(None)
  }
}

impl<CS: ChannelStore, MLF: MessageLogFactory> std::fmt::Debug for C2sDispatcher<CS, MLF> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("C2sDispatcher").finish_non_exhaustive()
  }
}

// === impl C2sDispatcher ===

impl<CS: ChannelStore, MLF: MessageLogFactory> C2sDispatcher<CS, MLF> {
  /// Initializes the dispatcher with the given parameters.
  #[allow(clippy::too_many_arguments)]
  fn new(
    handler: usize,
    config: Arc<Config>,
    auth_required: bool,
    modulator: Option<Arc<dyn Modulator>>,
    channel_manager: ChannelManager<CS, MLF>,
    c2s_router: c2s::Router,
    conn_tx: ConnTx,
    metrics: C2sDispatcherMetrics,
  ) -> Self {
    let inner = C2sDispatcherInner {
      config,
      channel_manager,
      c2s_router,
      heartbeat_interval: Default::default(),
      nid: None,
      transmitter: Arc::new(C2sTransmitter::new(handler, conn_tx)),
      modulator,
      auth_required,
      metrics,
    };

    Self(Some(inner))
  }
}

struct C2sDispatcherInner<CS: ChannelStore, MLF: MessageLogFactory> {
  /// C2S configuration.
  config: Arc<Config>,

  /// Whether authentication is required.
  auth_required: bool,

  /// The transmitter registry.
  c2s_router: c2s::Router,

  /// The connection transmitter.
  transmitter: Arc<C2sTransmitter>,

  /// The modulator, if any.
  modulator: Option<Arc<dyn Modulator>>,

  /// The channel manager.
  channel_manager: ChannelManager<CS, MLF>,

  /// The NID assigned to the connection.
  nid: Option<Nid>,

  /// The negotiated heartbeat interval.
  heartbeat_interval: Duration,

  /// Metric handles for this dispatcher.
  metrics: C2sDispatcherMetrics,
}

// === impl C2sDispatcherInner ===

impl<CS: ChannelStore, MLF: MessageLogFactory> C2sDispatcherInner<CS, MLF> {
  /// Handles the initial connection handshake with a client.
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to process. Must be a `Message::C2sConnect` variant.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the connection is accepted.
  ///
  /// # Errors
  ///
  /// Returns an error if the protocol version is not supported or if an unexpected
  /// message type is received.
  async fn dispatch_message_in_connecting_state(&mut self, msg: Message) -> anyhow::Result<()> {
    let config = self.config.clone();
    let config_keep_alive_interval = config.keep_alive_interval;
    let config_min_keep_alive_interval = config.min_keep_alive_interval;

    match msg {
      Message::Connect(params) => {
        if params.protocol_version != 1 {
          return Err(narwhal_protocol::Error::new(UnsupportedProtocolVersion).into());
        }

        let mut heartbeat_interval = Duration::from_millis(params.heartbeat_interval as u64);
        if heartbeat_interval.is_zero() {
          heartbeat_interval = config_keep_alive_interval;
        } else if heartbeat_interval < config_min_keep_alive_interval {
          heartbeat_interval = config_min_keep_alive_interval;
        } else if heartbeat_interval > config_keep_alive_interval {
          heartbeat_interval = config_keep_alive_interval
        }
        self.heartbeat_interval = heartbeat_interval;

        // Send the proper reply message informing the client that it is connected.
        let application_protocol =
          if let Some(modulator) = self.modulator.as_ref() { Some(modulator.protocol_name().await?) } else { None };

        let reply_msg = Message::ConnectAck(ConnectAckParameters {
          auth_required: self.auth_required,
          application_protocol,
          heartbeat_interval: heartbeat_interval.as_millis() as u32,
          max_subscriptions: config.limits.max_channels_per_client,
          max_message_size: config.limits.max_message_size,
          max_payload_size: config.limits.max_payload_size,
          max_inflight_requests: config.limits.max_inflight_requests,
          max_persist_messages: match config.limits.max_persist_messages {
            0 => None,
            v => Some(v),
          },
        });

        self.transmitter.send_message(reply_msg);

        trace!(
          handler = self.transmitter.handler,
          auth_required = self.auth_required,
          max_channels_per_client = config.limits.max_channels_per_client,
          max_message_size = config.limits.max_message_size,
          max_payload_size = config.limits.max_payload_size,
          max_inflight_requests = config.limits.max_inflight_requests,
          "handshake completed"
        );
      },
      _ => {
        return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
      },
    }
    Ok(())
  }

  /// Handles client identification in the connected state.
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to process. Must be a `Message::Identify` or `Message::Auth` variant.
  ///
  /// # Returns
  ///
  /// Returns `Ok(true)` if identification/authentication succeeds and the connection should
  /// transition to the authenticated state. Returns `Ok(false)` if authentication is still
  /// in progress (multi-step authentication).
  ///
  /// # Errors
  ///
  /// Returns an error in the following cases:
  /// * If the username is invalid
  /// * If the username is already in use when attempting to identify (authentication disabled)
  /// * If an unexpected message type is received
  async fn dispatch_message_in_connected_state(&mut self, msg: Message) -> anyhow::Result<bool> {
    match msg {
      Message::Auth(params) => {
        // If authentication is not required, reject the message.
        if !self.auth_required {
          return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
        }

        match self.modulator.as_ref().unwrap().authenticate(AuthRequest { token: params.token }).await {
          Ok(auth_res) => match auth_res.result {
            AuthResult::Success { username } => {
              let nid = {
                match self.make_local_nid(username.clone()) {
                  Ok(nid) => nid,
                  Err(e) => {
                    return Err(narwhal_protocol::Error::new(InternalServerError).with_detail(e.to_string()).into());
                  },
                }
              };

              // Register the connection non-exclusively.
              let _ = self
                .c2s_router
                .register_connection(nid.username.clone(), self.transmitter.clone(), self.transmitter.handler, false)
                .await;

              self.nid = Some(nid.clone());

              self.transmitter.send_message(Message::AuthAck(AuthAckParameters {
                challenge: None,
                succeeded: Some(true),
                nid: Some(nid.clone().into()),
              }));

              self.metrics.auth_attempts.get_or_create(&SUCCESS).inc();
              trace!(handler = self.transmitter.handler, nid = nid.to_string(), "user authenticated");

              Ok(true)
            },
            AuthResult::Continue { challenge } => {
              self.transmitter.send_message(Message::AuthAck(AuthAckParameters {
                challenge: Some(challenge),
                succeeded: None,
                nid: None,
              }));

              self.metrics.auth_attempts.get_or_create(&ResultLabel { result: "challenge" }).inc();
              Ok(false)
            },
            AuthResult::Failure => {
              self.transmitter.send_message(Message::AuthAck(AuthAckParameters {
                challenge: None,
                succeeded: Some(false),
                nid: None,
              }));

              self.metrics.auth_attempts.get_or_create(&FAILURE).inc();
              Ok(false)
            },
          },
          Err(e) => Err(narwhal_protocol::Error::new(InternalServerError).with_detail(e.to_string()).into()),
        }
      },
      Message::Identify(params) => {
        // If authentication is required, reject the message.
        if self.auth_required {
          return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
        }

        // Check if the username is already in use.
        let username = params.username.trim();

        let nid = {
          match self.make_local_nid(username.into()) {
            Ok(nid) => nid,
            Err(e) => {
              return Err(narwhal_protocol::Error::new(BadRequest).with_detail(e.to_string()).into());
            },
          }
        };

        // Register the connection exclusively.
        if self
          .c2s_router
          .register_connection(nid.username.clone(), self.transmitter.clone(), self.transmitter.handler, true)
          .await
        {
          self.nid = Some(nid);
        } else {
          self.metrics.identify_attempts.get_or_create(&FAILURE).inc();
          return Err(narwhal_protocol::Error::new(UsernameInUse).into());
        }

        let nid = self.nid.as_ref().unwrap();

        self.transmitter.send_message(Message::IdentifyAck(IdentifyAckParameters { nid: StringAtom::from(nid) }));

        self.metrics.identify_attempts.get_or_create(&SUCCESS).inc();
        trace!(handler = self.transmitter.handler, nid = nid.to_string(), "user identified");

        Ok(true)
      },
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  /// Routes messages received in the authenticated state to their appropriate handlers.
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to dispatch
  /// * `payload` - Optional payload buffer associated with the message
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the message was successfully handled.
  ///
  /// # Errors
  ///
  /// Returns an `UnexpectedMessage` error if the message type is not supported
  /// in the authenticated state.
  async fn dispatch_message_in_authenticated_state(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
  ) -> anyhow::Result<()> {
    self.metrics.messages_received.inc();

    match msg {
      Message::Broadcast { .. } => {
        self.dispatch_broadcast_message(msg, payload.unwrap()).await?;
      },
      Message::DeleteChannel { .. } => {
        self.dispatch_delete_channel_message(msg).await?;
      },
      Message::GetChannelAcl { .. } => {
        self.dispatch_get_channel_acl_message(msg).await?;
      },
      Message::GetChannelConfiguration { .. } => {
        self.dispatch_get_channel_configuration_message(msg).await?;
      },
      Message::JoinChannel { .. } => {
        self.dispatch_join_message(msg).await?;
      },
      Message::LeaveChannel { .. } => {
        self.dispatch_leave_message(msg).await?;
      },
      Message::ListChannels { .. } => {
        self.dispatch_list_channels_message(msg).await?;
      },
      Message::ListMembers { .. } => {
        self.dispatch_list_members_message(msg).await?;
      },
      Message::ModDirect { .. } => {
        self.dispatch_mod_direct_message(msg, payload.unwrap()).await?;
      },
      Message::SetChannelAcl { .. } => {
        self.dispatch_set_channel_acl_message(msg).await?;
      },
      Message::SetChannelConfiguration { .. } => {
        self.dispatch_set_channel_configuration_message(msg).await?;
      },
      Message::History { .. } => {
        self.dispatch_history_message(msg).await?;
      },
      Message::ChannelSeq { .. } => {
        self.dispatch_channel_seq_message(msg).await?;
      },
      _ => {
        return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
      },
    }
    Ok(())
  }

  /// Handles broadcasting a payload to a channel.
  ///
  /// # Arguments
  ///
  /// * `msg` - The broadcast message containing channel and correlation ID
  /// * `payload` - The payload to broadcast to channel members
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the payload was successfully broadcast.
  ///
  /// # Errors
  ///
  /// Returns an error if the channel ID is invalid or if the broadcast operation fails.
  async fn dispatch_broadcast_message(&mut self, msg: Message, payload: PoolBuffer) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::Broadcast { .. }));

    let mut correlation_id: u32 = 0;
    let mut channel_id: Option<ChannelId> = None;
    let mut qos: Option<u8> = None;

    if let Message::Broadcast(params) = msg {
      correlation_id = params.id;
      channel_id = Some(Self::parse_channel_id(&params.channel)?);
      qos = params.qos;
    }
    let channel_id = channel_id.unwrap();

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    // Forward the payload to the modulator (if available) for validation and alteration.
    let mut altered_payload = payload;

    if let Some(modulator) = self.modulator.as_ref() {
      let request = ForwardBroadcastPayloadRequest {
        payload: altered_payload.clone(),
        from: nid.clone(),
        channel_handler: channel_id.handler.clone(),
      };
      match modulator.forward_broadcast_payload(request).await {
        Ok(forward_res) => match forward_res.result {
          ForwardBroadcastPayloadResult::Valid => {
            self.metrics.modulator_forward_payload.get_or_create(&ResultLabel { result: "valid" }).inc();
          },
          ForwardBroadcastPayloadResult::ValidWithAlteration { altered_payload: modified_payload } => {
            self.metrics.modulator_forward_payload.get_or_create(&ResultLabel { result: "valid_altered" }).inc();
            altered_payload = modified_payload;
          },
          ForwardBroadcastPayloadResult::Invalid => {
            self.metrics.modulator_forward_payload.get_or_create(&ResultLabel { result: "invalid" }).inc();
            return Err(narwhal_protocol::Error::new(BadRequest).with_id(correlation_id).into());
          },
        },
        Err(e) => {
          self.metrics.modulator_forward_payload.get_or_create(&ResultLabel { result: "error" }).inc();
          error!(
            handler = self.transmitter.handler,
            nid = nid.to_string(),
            channel = channel_id.to_string(),
            error = e.to_string(),
            "payload validation failed"
          );
          return Err(narwhal_protocol::Error::new(InternalServerError).with_id(correlation_id).into());
        },
      }
    }

    // Submit the request to broadcast the payload.
    let payload_length = altered_payload.as_slice().len() as u32;

    self
      .channel_manager
      .broadcast_payload(altered_payload, channel_id.clone(), nid.clone(), transmitter, qos, correlation_id)
      .await?;

    self.metrics.broadcast_payload_bytes.observe(payload_length as f64);

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      content_length = payload_length,
      "broadcasted payload"
    );

    Ok(())
  }

  /// Handles requests to retrieve a channel's access control list (ACL).
  ///
  /// # Arguments
  ///
  /// * `msg` - The get ACL message containing channel and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the ACL was successfully retrieved and sent.
  ///
  /// # Errors
  ///
  /// Returns an error if the channel ID is invalid or if the ACL retrieval fails.
  async fn dispatch_get_channel_acl_message(&mut self, msg: Message) -> anyhow::Result<()> {
    let Message::GetChannelAcl(params) = msg else { unreachable!() };

    let correlation_id = params.id;
    let channel_id = Self::parse_channel_id(&params.channel)?;
    let acl_type = AclType::from_str(params.r#type.as_ref())?;
    let page = params.page;
    let page_size = params.page_size;

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    // Submit the request to get the channel ACL.
    self
      .channel_manager
      .get_channel_acl(channel_id.clone(), nid.clone(), acl_type, page, page_size, transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      acl = acl_type.to_string(),
      "got channel ACL"
    );

    Ok(())
  }

  /// Handles requests to retrieve a channel's configuration.
  ///
  /// # Arguments
  ///
  /// * `msg` - The get configuration message containing channel and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the configuration was successfully retrieved and sent.
  ///
  /// # Errors
  ///
  /// Returns an error if the channel ID is invalid or if the configuration retrieval fails.
  async fn dispatch_get_channel_configuration_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::GetChannelConfiguration { .. }));

    let mut channel_id: Option<ChannelId> = None;
    let mut correlation_id: u32 = 0;

    if let Message::GetChannelConfiguration(params) = msg {
      correlation_id = params.id;
      channel_id = Some(Self::parse_channel_id(&params.channel)?);
    }
    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    let channel_id = channel_id.unwrap();

    // Submit the request to get the channel configuration.
    self
      .channel_manager
      .get_channel_configuration(channel_id.clone(), nid.clone(), transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      "got channel configuration"
    );

    Ok(())
  }

  /// Handles requests to update a channel's access control list (ACL).
  ///
  /// # Arguments
  ///
  /// * `msg` - The set ACL message containing channel, new ACL settings, and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the ACL was successfully updated.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The channel ID is invalid
  /// * The user lacks permission to modify the ACL
  /// * The ACL update operation fails
  async fn dispatch_set_channel_acl_message(&mut self, msg: Message) -> anyhow::Result<()> {
    let Message::SetChannelAcl(params) = msg else { unreachable!() };

    let correlation_id = params.id;
    let channel_id = Self::parse_channel_id(&params.channel)?;
    let nids: Vec<Nid> = params.nids.into_iter().map(|s| Self::parse_nid(&s)).collect::<Result<_, _>>()?;
    let acl_type = AclType::from_str(params.r#type.as_ref())?;
    let acl_action = AclAction::from_str(params.action.as_ref())?;

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    // Submit the request to set the channel ACL.
    self
      .channel_manager
      .set_channel_acl(channel_id.clone(), nid.clone(), nids, acl_type, acl_action, transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      acl = acl_type.to_string(),
      "set channel ACL"
    );

    Ok(())
  }

  /// Handles requests to update a channel's configuration.
  ///
  /// # Arguments
  ///
  /// * `msg` - The set configuration message containing channel, new settings, and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the configuration was successfully updated.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The channel ID is invalid
  /// * The user lacks permission to modify the configuration
  /// * The configuration update operation fails
  async fn dispatch_set_channel_configuration_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::SetChannelConfiguration { .. }));

    let mut correlation_id: u32 = 0;
    let mut channel_id: Option<ChannelId> = None;

    let mut channel_config = ChannelConfig::default();

    if let Message::SetChannelConfiguration(params) = msg {
      correlation_id = params.id;

      channel_id = Some(Self::parse_channel_id(&params.channel)?);

      channel_config = ChannelConfig {
        max_clients: params.max_clients,
        max_payload_size: params.max_payload_size,
        max_persist_messages: params.max_persist_messages,
        persist: params.persist,
        message_flush_interval: params.message_flush_interval,
      };
    }

    if channel_config.persist == Some(true) && !self.auth_required {
      return Err(
        narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).with_detail("persistence requires auth").into(),
      );
    }

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    let channel_id = channel_id.unwrap();

    // Submit the request to set the channel configuration.
    self
      .channel_manager
      .set_channel_configuration(channel_config, channel_id.clone(), nid.clone(), transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      "set channel configuration"
    );

    Ok(())
  }

  /// Handles channel join requests.
  ///
  /// This method supports both joining existing channels and creating new ones:
  /// * If a channel ID is provided, attempts to join that channel
  /// * If no channel ID is provided, creates a new channel and joins it
  /// * Supports joining on behalf of another user if authorized
  ///
  /// # Arguments
  ///
  /// * `msg` - The join message containing optional channel ID and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the join operation succeeds.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The channel ID is invalid
  /// * The user lacks permission to join
  /// * The channel is full
  async fn dispatch_join_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::JoinChannel { .. }));

    let mut channel_id: Option<ChannelId> = None;
    let mut correlation_id: u32 = 0;
    let mut on_behalf_nid: Option<Nid> = None;

    if let Message::JoinChannel(params) = msg {
      correlation_id = params.id;

      channel_id = Some(Self::parse_channel_id(&params.channel)?);

      if let Some(nid_str) = params.on_behalf {
        on_behalf_nid = Some(Self::parse_nid(&nid_str)?);
      }
    }
    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    // Submit the request to join the channel.
    let as_owner = self
      .channel_manager
      .join_channel(channel_id.as_ref().unwrap().clone(), nid.clone(), on_behalf_nid, transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.unwrap().to_string(),
      as_owner = as_owner,
      "joined channel"
    );

    Ok(())
  }

  /// Handles channel deletion requests.
  ///
  /// Only the channel owner is allowed to delete the channel. All members are
  /// removed and notified with a `ChannelDeleted` event, and the channel is
  /// destroyed.
  ///
  /// # Arguments
  ///
  /// * `msg` - The delete channel message containing channel ID and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the deletion succeeds.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The channel ID is invalid
  /// * The user is not the channel owner
  /// * The channel does not exist
  async fn dispatch_delete_channel_message(&mut self, msg: Message) -> anyhow::Result<()> {
    let Message::DeleteChannel(params) = msg else { unreachable!() };

    let correlation_id = params.id;
    let channel_id = Self::parse_channel_id(&params.channel)?;

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    self.channel_manager.delete_channel(channel_id.clone(), nid.clone(), transmitter, correlation_id).await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      "deleted channel"
    );

    Ok(())
  }

  /// Handles channel leave requests.
  ///
  /// Supports leaving a channel directly or on behalf of another user if authorized.
  ///
  /// # Arguments
  ///
  /// * `msg` - The leave message containing channel ID and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the leave operation succeeds.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The channel ID is invalid
  /// * The user is not in the channel
  /// * The user lacks permission to remove others
  async fn dispatch_leave_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::LeaveChannel { .. }));

    let mut channel_id: Option<ChannelId> = None;
    let mut correlation_id: u32 = 0;
    let mut on_behalf_nid: Option<Nid> = None;

    if let Message::LeaveChannel(params) = msg {
      correlation_id = params.id;
      channel_id = Some(Self::parse_channel_id(&params.channel)?);

      if let Some(nid_str) = params.on_behalf {
        on_behalf_nid = Some(Self::parse_nid(&nid_str)?);
      }
    }
    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    let channel_id = channel_id.unwrap();

    // Submit the request to leave the channel.
    self
      .channel_manager
      .leave_channel(channel_id.clone(), nid.clone(), on_behalf_nid, Some(transmitter), correlation_id)
      .await?;

    trace!(handler = self.transmitter.handler, nid = nid.to_string(), channel = channel_id.to_string(), "left channel");

    Ok(())
  }

  /// Handles requests to list available channels.
  ///
  /// Can list either all accessible channels or only owned channels.
  ///
  /// # Arguments
  ///
  /// * `msg` - The list channels message containing correlation ID and owner filter
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the channel list was successfully retrieved and sent.
  async fn dispatch_list_channels_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::ListChannels { .. }));

    let mut correlation_id: u32 = 0;
    let mut as_owner: bool = false;
    let mut page: Option<u32> = None;
    let mut count: Option<u32> = None;

    if let Message::ListChannels(params) = msg {
      correlation_id = params.id;
      page = params.page;
      count = params.page_size;
      as_owner = params.owner;
    }
    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    // Submit the request to list the channels.
    self.channel_manager.list_channels(nid.clone(), page, count, as_owner, transmitter, correlation_id).await?;

    trace!(handler = self.transmitter.handler, nid = nid.to_string(), as_owner = as_owner, "listed channels");

    Ok(())
  }

  /// Handles requests to list channel members.
  ///
  /// # Arguments
  ///
  /// * `msg` - The list members message containing channel and correlation ID
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the member list was successfully retrieved and sent.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The channel ID is invalid
  /// * The user lacks permission to list members
  async fn dispatch_list_members_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::ListMembers { .. }));

    let mut correlation_id: u32 = 0;
    let mut channel_id: Option<ChannelId> = None;
    let mut page: Option<u32> = None;
    let mut count: Option<u32> = None;

    if let Message::ListMembers(params) = msg {
      correlation_id = params.id;
      channel_id = Some(Self::parse_channel_id(&params.channel)?);
      page = params.page;
      count = params.page_size;
    }
    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    let channel_id = channel_id.unwrap();

    // Submit the request to list the members.
    self
      .channel_manager
      .list_members(channel_id.clone(), nid.clone(), page, count, transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      "listed members"
    );

    Ok(())
  }

  /// Dispatches a private payload directly to the modulator for processing.
  ///
  /// # Arguments
  ///
  /// * `msg` - The `MOD_DIRECT` message containing the correlation ID and payload length
  /// * `payload` - The raw payload data to be forwarded to the modulator
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` on successful processing and acknowledgment.
  async fn dispatch_mod_direct_message(&mut self, msg: Message, payload: PoolBuffer) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::ModDirect { .. }));

    let modulator = {
      match self.modulator.as_ref() {
        Some(modulator) => modulator,
        None => return Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
      }
    };

    // Check if direct forwarding is supported.
    if !modulator.operations().await?.contains(Operation::SendPrivatePayload) {
      return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
    }

    let params = match msg {
      Message::ModDirect(params) => params,
      _ => unreachable!(),
    };

    let correlation_id: u32 = {
      match params.id {
        Some(id) => id,
        None => return Err(narwhal_protocol::Error::new(BadRequest).into()),
      }
    };

    let nid = self.nid.as_ref().unwrap().clone();

    let request = SendPrivatePayloadRequest { payload, from: nid.username.clone() };
    let response = modulator.send_private_payload(request).await?;
    if matches!(response.result, SendPrivatePayloadResult::Invalid) {
      return Err(narwhal_protocol::Error::new(BadRequest).with_id(correlation_id).into());
    }
    let transmitter = self.transmitter.clone();

    // Send the response back to the client.
    transmitter.send_message(Message::ModDirectAck(ModDirectAckParameters { id: correlation_id }));

    trace!(handler = transmitter.handler, nid = nid.to_string(), "modulator payload forwarded");

    Ok(())
  }

  async fn dispatch_history_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::History { .. }));

    let params = match msg {
      Message::History(params) => params,
      _ => unreachable!(),
    };

    let correlation_id = params.id;
    let channel_id = Self::parse_channel_id(&params.channel)?;
    let history_id = params.history_id;
    let from_seq = params.from_seq;
    let limit = params.limit;
    let direction = params.direction;

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    self
      .channel_manager
      .history(channel_id.clone(), nid.clone(), history_id, from_seq, limit, direction, transmitter, correlation_id)
      .await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      "history requested"
    );

    Ok(())
  }

  async fn dispatch_channel_seq_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::ChannelSeq { .. }));

    let params = match msg {
      Message::ChannelSeq(params) => params,
      _ => unreachable!(),
    };

    let correlation_id = params.id;
    let channel_id = Self::parse_channel_id(&params.channel)?;

    let nid = self.nid.as_ref().unwrap().clone();
    let transmitter = self.transmitter.clone();

    self.channel_manager.channel_seq(channel_id.clone(), nid.clone(), transmitter, correlation_id).await?;

    trace!(
      handler = self.transmitter.handler,
      nid = nid.to_string(),
      channel = channel_id.to_string(),
      "channel seq requested"
    );

    Ok(())
  }

  fn make_local_nid(&self, username: StringAtom) -> anyhow::Result<Nid> {
    match Nid::new(username, StringAtom::from(self.config.listener.domain.as_str())) {
      Ok(nid) => Ok(nid),
      Err(e) => Err(anyhow::Error::new(e)),
    }
  }

  fn parse_channel_id(s: &str) -> anyhow::Result<ChannelId> {
    match ChannelId::from_str(s) {
      Ok(id) => Ok(id),
      Err(e) => Err(narwhal_protocol::Error::new(BadRequest).with_detail(e.to_string()).into()),
    }
  }

  fn parse_nid(s: &str) -> anyhow::Result<Nid> {
    match Nid::from_str(s) {
      Ok(nid) => Ok(nid),
      Err(e) => Err(narwhal_protocol::Error::new(BadRequest).with_detail(e.to_string()).into()),
    }
  }
}

#[async_trait(?Send)]
impl<CS: ChannelStore, MLF: MessageLogFactory> narwhal_common::conn::Dispatcher for C2sDispatcher<CS, MLF> {
  async fn dispatch_message(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
    state: State,
  ) -> anyhow::Result<Option<State>> {
    let inner = self.0.as_mut().unwrap();

    match state {
      State::Connecting => {
        inner.dispatch_message_in_connecting_state(msg).await?;
        Ok(Some(State::Connected))
      },
      State::Connected => {
        if inner.dispatch_message_in_connected_state(msg).await? {
          return Ok(Some(State::Authenticated { heartbeat_interval: inner.heartbeat_interval }));
        }
        Ok(None)
      },
      State::Authenticated { .. } => {
        inner.dispatch_message_in_authenticated_state(msg, payload).await?;
        Ok(None)
      },
    }
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    let inner = self.0.as_mut().unwrap();

    if let Some(nid) = inner.nid.take() {
      let channel_mng = inner.channel_manager.clone();

      // Unregister the username.
      let should_cleanup = inner.c2s_router.unregister_connection(&nid.username, inner.transmitter.handler).await;

      if should_cleanup {
        // Leave from all channels when last connection is closed.
        // The channel manager decides which channels to actually leave
        // based on persistence.
        channel_mng.leave_all_channels(nid.clone()).await?;
      }
    }

    Ok(())
  }
}
