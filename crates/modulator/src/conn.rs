// SPDX-License-Identifier: BSD-3-Clause

use std::fmt::Debug;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Ok;
use async_broadcast;
use async_channel;
use async_lock::RwLock;
use futures::FutureExt;
use tracing::{error, trace, warn};

use narwhal_common::conn::{ConnTx, State};
use narwhal_common::runtime;
use narwhal_common::service::{M2sService, S2mService};
use narwhal_protocol::ErrorReason::{BadRequest, Unauthorized, UnexpectedMessage, UnsupportedProtocolVersion};
use narwhal_protocol::{Event, Nid, S2mModDirectAckParameters};
use narwhal_protocol::{
  M2sConnectAckParameters, M2sModDirectAckParameters, Message, S2mAuthAckParameters, S2mConnectAckParameters,
  S2mForwardBroadcastPayloadAckParameters, S2mForwardEventAckParameters,
};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

use crate::modulator::{
  AuthRequest, AuthResult, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResult, ForwardEventRequest,
  Operation, OutboundPrivatePayload, ReceivePrivatePayloadRequest, SendPrivatePayloadRequest, SendPrivatePayloadResult,
};
use narwhal_client::compio::m2s::M2sClient;

use crate::{M2sServerConfig, Modulator, S2mServerConfig};

/// The S2M connection runtime.
pub type S2mConnRuntime = narwhal_common::conn::ConnRuntime<S2mService>;

/// The M2S connection runtime.
pub type M2sConnRuntime = narwhal_common::conn::ConnRuntime<M2sService>;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabel {
  result: &'static str,
}

const SUCCESS: ResultLabel = ResultLabel { result: "success" };
const FAILURE: ResultLabel = ResultLabel { result: "failure" };

#[derive(Clone)]
struct M2sDispatcherMetrics {
  connect_failures: Counter,
  mod_direct: Counter,
  mod_direct_dropped: Counter,
}

// === impl M2sDispatcherMetrics ===

impl M2sDispatcherMetrics {
  fn register(registry: &mut Registry) -> Self {
    let connect_failures = Counter::default();
    registry.register("connect_failures", "Connection handshake failures", connect_failures.clone());
    let mod_direct = Counter::default();
    registry.register("mod_direct", "M2sModDirect messages routed", mod_direct.clone());
    let mod_direct_dropped = Counter::default();
    registry.register("mod_direct_dropped", "M2sModDirect payloads dropped", mod_direct_dropped.clone());
    Self { connect_failures, mod_direct, mod_direct_dropped }
  }
}

#[derive(Clone)]
struct S2mDispatcherMetrics {
  connect_failures: Counter,
  auth_attempts: Family<ResultLabel, Counter>,
  forward_payload: Family<ResultLabel, Counter>,
  forward_event: Counter,
  mod_direct: Family<ResultLabel, Counter>,
}

// === impl S2mDispatcherMetrics ===

impl S2mDispatcherMetrics {
  fn register(registry: &mut Registry) -> Self {
    let connect_failures = Counter::default();
    registry.register("connect_failures", "Connection handshake failures", connect_failures.clone());
    let auth_attempts = Family::<ResultLabel, Counter>::default();
    registry.register("auth_attempts", "Authentication attempts", auth_attempts.clone());
    let forward_payload = Family::<ResultLabel, Counter>::default();
    registry.register("forward_payload", "Forward broadcast payload results", forward_payload.clone());
    let forward_event = Counter::default();
    registry.register("forward_event", "Forward event messages handled", forward_event.clone());
    let mod_direct = Family::<ResultLabel, Counter>::default();
    registry.register("mod_direct", "S2mModDirect messages handled", mod_direct.clone());
    Self { connect_failures, auth_attempts, forward_payload, forward_event, mod_direct }
  }
}

#[derive(Clone)]
pub struct M2sDispatcherFactory {
  inner: Arc<RwLock<M2sDispatcherFactoryInner>>,
  metrics: M2sDispatcherMetrics,
}

// === impl M2sDispatcherFactory ===

impl M2sDispatcherFactory {
  pub fn new(
    config: Arc<M2sServerConfig>,
    payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,
    registry: &mut Registry,
  ) -> Self {
    let inner = M2sDispatcherFactoryInner { config, payload_tx };

    Self { inner: Arc::new(RwLock::new(inner)), metrics: M2sDispatcherMetrics::register(registry) }
  }
}

#[async_trait::async_trait(?Send)]
impl narwhal_common::conn::DispatcherFactory<M2sDispatcher> for M2sDispatcherFactory {
  async fn create(&mut self, handler: usize, tx: ConnTx) -> M2sDispatcher {
    let inner = self.inner.read().await;

    M2sDispatcher::new(handler, inner.config.clone(), tx, inner.payload_tx.clone(), self.metrics.clone())
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    Ok(())
  }
}

pub struct M2sDispatcherFactoryInner {
  /// The server configuration.
  config: Arc<M2sServerConfig>,

  /// The broadcast sender for outbound private payloads.
  payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,
}

/// A M2S dispatcher that handles incoming messages.
pub struct M2sDispatcher {
  /// The dispatcher handler.
  handler: usize,

  /// The M2S server configuration.
  config: Arc<M2sServerConfig>,

  /// The connection transmitter.
  tx: ConnTx,

  /// The broadcast sender for outbound private payloads.
  payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,

  /// Metric handles.
  metrics: M2sDispatcherMetrics,
}

impl Debug for M2sDispatcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("M2sDispatcher").field("handler", &self.handler).field("config", &self.config).finish()
  }
}

// === impl M2sDispatcher ===

impl M2sDispatcher {
  fn new(
    handler: usize,
    config: Arc<M2sServerConfig>,
    tx: ConnTx,
    payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,
    metrics: M2sDispatcherMetrics,
  ) -> Self {
    Self { handler, config, tx, payload_tx, metrics }
  }
}

#[async_trait::async_trait(?Send)]
impl narwhal_common::conn::Dispatcher for M2sDispatcher {
  async fn dispatch_message(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
    state: State,
  ) -> anyhow::Result<Option<State>> {
    match state {
      State::Connecting => {
        let heartbeat_interval = self.dispatch_message_in_connecting_state(msg).await?;
        Ok(Some(State::Authenticated { heartbeat_interval }))
      },
      State::Authenticated { .. } => {
        self.dispatch_message_in_authenticated_state(msg, payload).await?;
        Ok(None)
      },
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    Ok(())
  }
}

impl M2sDispatcher {
  async fn dispatch_message_in_connecting_state(&mut self, msg: Message) -> anyhow::Result<Duration> {
    match msg {
      Message::M2sConnect(params) => {
        if params.protocol_version != 1 {
          self.metrics.connect_failures.inc();
          return Err(narwhal_protocol::Error::new(UnsupportedProtocolVersion).into());
        }
        let config = self.config.clone();

        // Validate the shared secret
        let shared_secret: Option<StringAtom> =
          { if !config.shared_secret.is_empty() { Some(config.shared_secret.as_str().into()) } else { None } };

        if shared_secret.is_some() && params.secret != shared_secret {
          self.metrics.connect_failures.inc();
          return Err(narwhal_protocol::Error::new(Unauthorized).into());
        }

        let config_keep_alive_interval = config.keep_alive_interval;
        let config_min_keep_alive_interval = config.min_keep_alive_interval;

        let mut heartbeat_interval = Duration::from_millis(params.heartbeat_interval as u64);
        if heartbeat_interval.is_zero() {
          heartbeat_interval = config_keep_alive_interval;
        } else if heartbeat_interval < config_min_keep_alive_interval {
          heartbeat_interval = config_min_keep_alive_interval;
        } else if heartbeat_interval > config_keep_alive_interval {
          heartbeat_interval = config_keep_alive_interval
        }

        // Send the proper reply message informing the client that it is connected.
        let reply_msg = Message::M2sConnectAck(M2sConnectAckParameters {
          heartbeat_interval: heartbeat_interval.as_millis() as u32,
          max_message_size: config.limits.max_message_size,
          max_payload_size: config.limits.max_payload_size,
          max_inflight_requests: config.limits.max_inflight_requests,
        });

        self.tx.send_message(reply_msg);

        Ok(heartbeat_interval)
      },
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  async fn dispatch_message_in_authenticated_state(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
  ) -> anyhow::Result<()> {
    match msg {
      Message::M2sModDirect(_) => match payload {
        Some(p) => self.dispatch_mod_direct_message(msg, p).await,
        None => {
          error!(handler = self.handler, "M2sModDirect message received without payload");
          Err(narwhal_protocol::Error::new(BadRequest).into())
        },
      },
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  async fn dispatch_mod_direct_message(&mut self, msg: Message, payload: PoolBuffer) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::M2sModDirect { .. }));

    // Extract the parameters from the message
    let params = match msg {
      Message::M2sModDirect(params) => params,
      _ => unreachable!(),
    };

    // Create the outbound private payload
    let outbound_payload = OutboundPrivatePayload { payload, targets: params.targets.clone() };

    // Send the payload through the broadcast channel
    // Ignore errors if there are no receivers
    if self.payload_tx.try_broadcast(outbound_payload).is_err() {
      self.metrics.mod_direct_dropped.inc();
    } else {
      self.metrics.mod_direct.inc();
    }

    // Send acknowledgment
    let ack_msg = Message::M2sModDirectAck(M2sModDirectAckParameters { id: params.id });
    self.tx.send_message(ack_msg);

    trace!(handler = self.handler, id = params.id, "M2sModDirect message processed successfully");

    Ok(())
  }
}

pub struct S2mDispatcherFactory<M: Modulator> {
  inner: Arc<RwLock<S2mDispatcherFactoryInner<M>>>,
  metrics: S2mDispatcherMetrics,
}

pub struct S2mDispatcherFactoryInner<M: Modulator> {
  /// The server configuration.
  config: Arc<S2mServerConfig>,

  /// The modulator interface.
  modulator: Arc<M>,

  /// The M2S client, if needed.
  m2s_client: Option<M2sClient>,

  /// Handle to the private payload reader task.
  payload_reader_handle: Option<runtime::JoinHandle<()>>,

  /// Shutdown sender for graceful shutdown.
  shutdown_tx: async_channel::Sender<()>,

  /// Shutdown receiver for graceful shutdown.
  shutdown_rx: async_channel::Receiver<()>,
}

impl<M: Modulator> Debug for S2mDispatcherFactoryInner<M> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("S2mDispatcherFactoryInner")
      .field("config", &self.config)
      .field("m2s_client", &self.m2s_client)
      .finish()
  }
}

// === impl S2mDispatcherFactory ===

impl<M: Modulator> Clone for S2mDispatcherFactory<M> {
  fn clone(&self) -> Self {
    Self { inner: self.inner.clone(), metrics: self.metrics.clone() }
  }
}

impl<M: Modulator> S2mDispatcherFactory<M> {
  pub fn new(config: Arc<S2mServerConfig>, modulator: Arc<M>, registry: &mut Registry) -> Self {
    let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);

    let inner = S2mDispatcherFactoryInner {
      config,
      modulator,
      shutdown_tx,
      shutdown_rx,
      payload_reader_handle: None,
      m2s_client: None,
    };

    Self { inner: Arc::new(RwLock::new(inner)), metrics: S2mDispatcherMetrics::register(registry) }
  }

  async fn payload_reader_loop(
    mut rx: async_broadcast::Receiver<OutboundPrivatePayload>,
    m2s_client: M2sClient,
    shutdown_rx: async_channel::Receiver<()>,
  ) {
    loop {
      let mut recv = std::pin::pin!(rx.recv().fuse());
      let mut shutdown = std::pin::pin!(shutdown_rx.recv().fuse());

      futures::select! {
        result = recv => {
          match result {
            std::result::Result::Ok(outbound) => {
                match m2s_client.route_private_payload(outbound.payload, outbound.targets).await {
                    anyhow::Result::Ok(_) => {}
                    Err(err) => warn!("failed to route private payload: {}", err),
                }
            }
            Err(async_broadcast::RecvError::Overflowed(_)) => {
              continue;
            }
            Err(async_broadcast::RecvError::Closed) => {
              break;
            }
          }
        }
        _ = shutdown => {
          break;
        }
      }
    }
  }
}

#[async_trait::async_trait(?Send)]
impl<M: Modulator> narwhal_common::conn::DispatcherFactory<S2mDispatcher<M>> for S2mDispatcherFactory<M> {
  async fn create(&mut self, handler: usize, tx: ConnTx) -> S2mDispatcher<M> {
    let inner = self.inner.read().await;

    S2mDispatcher::new(handler, inner.config.clone(), inner.modulator.clone(), tx, self.metrics.clone())
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    let mut inner = self.inner.write().await;

    assert!(!inner.shutdown_tx.is_closed());

    if inner.modulator.operations().await?.contains(Operation::ReceivePrivatePayload) {
      // Initialize the M2S client
      let m2s_client = M2sClient::new(inner.config.m2s_client.clone().into())?;
      inner.m2s_client = Some(m2s_client.clone());

      // Spawn the payload reader loop
      let response = inner.modulator.receive_private_payload(ReceivePrivatePayloadRequest {}).await?;
      let rx = response.receiver;
      let shutdown_rx = inner.shutdown_rx.clone();

      let handle = runtime::spawn(Self::payload_reader_loop(rx, m2s_client.clone(), shutdown_rx));
      inner.payload_reader_handle = Some(handle);
    }
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    let mut inner = self.inner.write().await;

    inner.shutdown_tx.close();

    if let Some(handle) = inner.payload_reader_handle.take() {
      let _ = handle.await;
    }

    if let Some(m2s_client) = inner.m2s_client.take() {
      let _ = m2s_client.shutdown().await;
    }
    Ok(())
  }
}

/// A S2M dispatcher that handles incoming messages.
pub struct S2mDispatcher<M: Modulator> {
  /// The dispatcher handler.
  handler: usize,

  /// The S2M server configuration.
  config: Arc<S2mServerConfig>,

  /// The modulator interface.
  modulator: Arc<M>,

  /// The connection transmitter.
  tx: ConnTx,

  /// Metric handles.
  metrics: S2mDispatcherMetrics,
}

impl<M: Modulator> Debug for S2mDispatcher<M> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("S2mDispatcher").field("handler", &self.handler).field("config", &self.config).finish()
  }
}

impl<M: Modulator> S2mDispatcher<M> {
  fn new(
    handler: usize,
    config: Arc<S2mServerConfig>,
    modulator: Arc<M>,
    tx: ConnTx,
    metrics: S2mDispatcherMetrics,
  ) -> Self {
    S2mDispatcher { handler, config, modulator, tx, metrics }
  }
}

#[async_trait::async_trait(?Send)]
impl<M: Modulator> narwhal_common::conn::Dispatcher for S2mDispatcher<M> {
  async fn dispatch_message(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
    state: State,
  ) -> anyhow::Result<Option<State>> {
    match state {
      State::Connecting => {
        let heartbeat_interval = self.dispatch_message_in_connecting_state(msg).await?;
        Ok(Some(State::Authenticated { heartbeat_interval }))
      },
      State::Authenticated { .. } => {
        self.dispatch_message_in_authenticated_state(msg, payload).await?;
        Ok(None)
      },
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    Ok(())
  }
}

impl<M: Modulator> S2mDispatcher<M> {
  async fn dispatch_message_in_connecting_state(&mut self, msg: Message) -> anyhow::Result<Duration> {
    match msg {
      Message::S2mConnect(params) => {
        if params.protocol_version != 1 {
          self.metrics.connect_failures.inc();
          return Err(narwhal_protocol::Error::new(UnsupportedProtocolVersion).into());
        }
        let config = self.config.server.clone();

        // Validate the shared secret
        let shared_secret: Option<StringAtom> =
          { if !config.shared_secret.is_empty() { Some(config.shared_secret.as_str().into()) } else { None } };

        if shared_secret.is_some() && params.secret != shared_secret {
          self.metrics.connect_failures.inc();
          return Err(narwhal_protocol::Error::new(Unauthorized).into());
        }

        let config_keep_alive_interval = config.keep_alive_interval;
        let config_min_keep_alive_interval = config.min_keep_alive_interval;

        let mut heartbeat_interval = Duration::from_millis(params.heartbeat_interval as u64);
        if heartbeat_interval.is_zero() {
          heartbeat_interval = config_keep_alive_interval;
        } else if heartbeat_interval < config_min_keep_alive_interval {
          heartbeat_interval = config_min_keep_alive_interval;
        } else if heartbeat_interval > config_keep_alive_interval {
          heartbeat_interval = config_keep_alive_interval
        }

        // Send the proper reply message informing the client that it is connected.
        let reply_msg = Message::S2mConnectAck(S2mConnectAckParameters {
          application_protocol: self.modulator.protocol_name().await?,
          operations: self.modulator.operations().await?.into(),
          heartbeat_interval: heartbeat_interval.as_millis() as u32,
          max_message_size: config.limits.max_message_size,
          max_payload_size: config.limits.max_payload_size,
          max_inflight_requests: config.limits.max_inflight_requests,
        });

        self.tx.send_message(reply_msg);

        Ok(heartbeat_interval)
      },
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  async fn dispatch_message_in_authenticated_state(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
  ) -> anyhow::Result<()> {
    match msg {
      Message::S2mAuth(_) => self.dispatch_auth_message(msg).await,
      Message::S2mModDirect(_) => self.dispatch_mod_direct_message(msg, payload.unwrap()).await,
      Message::S2mForwardBroadcastPayload(_) => {
        self.dispatch_forward_message_payload_message(msg, payload.unwrap()).await
      },
      Message::S2mForwardEvent(_) => self.dispatch_forward_event_message(msg).await,
      _ => Err(narwhal_protocol::Error::new(UnexpectedMessage).into()),
    }
  }

  async fn dispatch_auth_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::S2mAuth { .. }));

    // Check if the modulator supports the Auth operation.
    if !self.modulator.operations().await?.contains(Operation::Auth) {
      return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
    }

    let params = match msg {
      Message::S2mAuth(params) => params,
      _ => unreachable!(),
    };
    let id = params.id;

    // Authenticate the client.
    let auth_res = self.modulator.authenticate(AuthRequest { token: params.token }).await?;

    let reply_msg = match auth_res.result {
      AuthResult::Success { username } => {
        self.metrics.auth_attempts.get_or_create(&SUCCESS).inc();
        Message::S2mAuthAck(S2mAuthAckParameters { id, challenge: None, username: Some(username), succeeded: true })
      },
      AuthResult::Continue { challenge } => {
        self.metrics.auth_attempts.get_or_create(&ResultLabel { result: "challenge" }).inc();
        Message::S2mAuthAck(S2mAuthAckParameters { id, challenge: Some(challenge), username: None, succeeded: false })
      },
      AuthResult::Failure => {
        self.metrics.auth_attempts.get_or_create(&FAILURE).inc();
        Message::S2mAuthAck(S2mAuthAckParameters { id, challenge: None, username: None, succeeded: false })
      },
    };

    // Send the reply message.
    self.tx.send_message(reply_msg);

    trace!(handler = self.handler, id = id, "handled Auth message");

    Ok(())
  }

  async fn dispatch_mod_direct_message(&mut self, msg: Message, payload: PoolBuffer) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::S2mModDirect { .. }));

    // Check if the modulator supports the SendPrivatePayload operation.
    if !self.modulator.operations().await?.contains(Operation::SendPrivatePayload) {
      return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
    }

    let params = match msg {
      Message::S2mModDirect(params) => params,
      _ => unreachable!(),
    };

    // Pass private payload to the modulator for processing.
    let request = SendPrivatePayloadRequest { payload, from: params.from };
    let response = self.modulator.send_private_payload(request).await?;

    // Send the reply message.
    let valid = matches!(response.result, SendPrivatePayloadResult::Valid);

    if valid {
      self.metrics.mod_direct.get_or_create(&ResultLabel { result: "valid" }).inc();
    } else {
      self.metrics.mod_direct.get_or_create(&ResultLabel { result: "invalid" }).inc();
    }

    self.tx.send_message(Message::S2mModDirectAck(S2mModDirectAckParameters { id: params.id, valid }));

    trace!(handler = self.handler, id = params.id, "handled ModDirect message");

    Ok(())
  }

  async fn dispatch_forward_message_payload_message(
    &mut self,
    msg: Message,
    payload: PoolBuffer,
  ) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::S2mForwardBroadcastPayload { .. }));

    // Check if the modulator supports the ForwardMessagePayload operation.
    if !self.modulator.operations().await?.contains(Operation::ForwardBroadcastPayload) {
      return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
    }

    let params = match msg {
      Message::S2mForwardBroadcastPayload(params) => params,
      _ => unreachable!(),
    };

    let from_nid = match Nid::from_str(params.from.as_ref()) {
      std::result::Result::Ok(nid) => nid,
      std::result::Result::Err(_) => {
        return Err(narwhal_protocol::Error::new(BadRequest).with_id(params.id).into());
      },
    };

    // Forward the payload to the modulator and validate it.
    let forward_req = ForwardBroadcastPayloadRequest { payload, from: from_nid, channel_handler: params.channel };
    let forward_resp = self.modulator.forward_broadcast_payload(forward_req).await?;

    // Send the reply message and log the result.
    match forward_resp.result {
      ForwardBroadcastPayloadResult::Valid => {
        self.metrics.forward_payload.get_or_create(&ResultLabel { result: "valid" }).inc();
        self.tx.send_message(Message::S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters {
          id: params.id,
          valid: true,
          altered_payload: false,
          altered_payload_length: 0,
        }));

        trace!(
          handler = self.handler,
          id = params.id,
          valid = true,
          altered_payload = false,
          "handled ForwardPayload message"
        );
      },
      ForwardBroadcastPayloadResult::ValidWithAlteration { altered_payload } => {
        self.metrics.forward_payload.get_or_create(&ResultLabel { result: "valid_altered" }).inc();
        let length = altered_payload.len() as u32;
        self.tx.send_message_with_payload(
          Message::S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters {
            id: params.id,
            valid: true,
            altered_payload: true,
            altered_payload_length: length,
          }),
          Some(altered_payload),
        );

        trace!(
          handler = self.handler,
          id = params.id,
          valid = true,
          altered_payload = true,
          "handled ForwardPayload message"
        );
      },
      ForwardBroadcastPayloadResult::Invalid => {
        self.metrics.forward_payload.get_or_create(&ResultLabel { result: "invalid" }).inc();
        self.tx.send_message(Message::S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters {
          id: params.id,
          valid: false,
          altered_payload: false,
          altered_payload_length: 0,
        }));

        trace!(
          handler = self.handler,
          id = params.id,
          valid = false,
          altered_payload = false,
          "handled ForwardPayload message"
        );
      },
    }

    Ok(())
  }

  async fn dispatch_forward_event_message(&mut self, msg: Message) -> anyhow::Result<()> {
    assert!(matches!(msg, Message::S2mForwardEvent { .. }));

    // Check if the modulator supports the ForwardEvent operation.
    if !self.modulator.operations().await?.contains(Operation::ForwardEvent) {
      return Err(narwhal_protocol::Error::new(UnexpectedMessage).into());
    }

    let params = match msg {
      Message::S2mForwardEvent(params) => params,
      _ => unreachable!(),
    };

    // Forward the event to the modulator.
    let kind = params.kind.try_into().map_err(|e| {
      error!(handler = self.handler, error = ?e, "failed to parse event kind");
      e
    })?;

    let event = Event { kind, channel: params.channel, nid: params.nid, owner: params.owner };
    self.modulator.forward_event(ForwardEventRequest { event }).await?;

    self.metrics.forward_event.inc();

    self.tx.send_message(Message::S2mForwardEventAck(S2mForwardEventAckParameters { id: params.id }));

    trace!(handler = self.handler, id = params.id, "handled ForwardEvent message");

    Ok(())
  }
}
