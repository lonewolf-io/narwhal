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
use narwhal_common::service::{M2sService, S2mService};
use narwhal_protocol::ErrorReason::{BadRequest, Unauthorized, UnexpectedMessage, UnsupportedProtocolVersion};
use narwhal_protocol::{Event, Nid, S2mModDirectAckParameters};
use narwhal_protocol::{
  M2sConnectAckParameters, M2sModDirectAckParameters, Message, S2mAuthAckParameters, S2mConnectAckParameters,
  S2mForwardBroadcastPayloadAckParameters, S2mForwardEventAckParameters,
};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use crate::modulator::{
  AuthRequest, AuthResult, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResult, ForwardEventRequest,
  Operation, OutboundPrivatePayload, ReceivePrivatePayloadRequest, SendPrivatePayloadRequest, SendPrivatePayloadResult,
};
use narwhal_client::monoio::m2s::M2sClient;

use crate::{M2sServerConfig, Modulator, S2mServerConfig};

/// The S2M connection runtime.
pub type S2mConnRuntime = narwhal_common::conn::ConnRuntime<S2mService>;

/// The M2S connection runtime.
pub type M2sConnRuntime = narwhal_common::conn::ConnRuntime<M2sService>;

#[derive(Clone)]
pub struct M2sDispatcherFactory(Arc<RwLock<M2sDispatcherFactoryInner>>);

// ===== impl M2sDispatcherFactory =====

impl M2sDispatcherFactory {
  pub fn new(config: Arc<M2sServerConfig>, payload_tx: async_broadcast::Sender<OutboundPrivatePayload>) -> Self {
    let inner = M2sDispatcherFactoryInner { config, payload_tx };

    Self(Arc::new(RwLock::new(inner)))
  }
}

#[async_trait::async_trait(?Send)]
impl narwhal_common::conn::DispatcherFactory<M2sDispatcher> for M2sDispatcherFactory {
  async fn create(&mut self, handler: usize, tx: ConnTx) -> M2sDispatcher {
    let inner = self.0.read().await;

    M2sDispatcher::new(handler, inner.config.clone(), tx, inner.payload_tx.clone())
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
}

impl Debug for M2sDispatcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("M2sDispatcher").field("handler", &self.handler).field("config", &self.config).finish()
  }
}

// ===== impl M2sDispatcher =====

impl M2sDispatcher {
  /// Initializes the M2S dispatcher with the given parameters.
  pub fn new(
    handler: usize,
    config: Arc<M2sServerConfig>,
    tx: ConnTx,
    payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,
  ) -> Self {
    Self { handler, config, tx, payload_tx }
  }
}

#[async_trait::async_trait(?Send)]
impl narwhal_common::conn::Dispatcher for M2sDispatcher {
  /// Dispatches incoming messages based on the current connection state.
  ///
  /// This is the main entry point for message handling that:
  /// * Routes connecting state messages to connection establishment
  /// * Routes authenticated state messages to appropriate handlers
  /// * Rejects messages in other states
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to dispatch
  /// * `payload` - Optional payload buffer associated with the message
  /// * `state` - Current connection state
  ///
  /// # Returns
  ///
  /// Returns `Ok(Some(new_state))` if a state transition should occur, or
  /// `Ok(None)` if the state should remain unchanged.
  ///
  /// # Errors
  ///
  /// Returns an `UnexpectedMessage` error if the message cannot be handled
  /// in the current state.
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
          return Err(narwhal_protocol::Error::new(UnsupportedProtocolVersion).into());
        }
        let config = self.config.clone();

        // Validate the shared secret
        let shared_secret: Option<StringAtom> =
          { if !config.shared_secret.is_empty() { Some(config.shared_secret.as_str().into()) } else { None } };

        if shared_secret.is_some() && params.secret != shared_secret {
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

  /// Dispatches messages when the connection is in authenticated state.
  ///
  /// This method handles messages after the connection has been established and
  /// authenticated. Currently, it only supports M2sModDirect messages which are
  /// forwarded to the modulator for processing.
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to dispatch
  /// * `payload` - Optional payload buffer associated with the message
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the message was successfully dispatched.
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

  /// Handles M2sModDirect messages from the modulator for routing to clients.
  ///
  /// This method processes direct messages sent by the modulator that contain
  /// payloads to be routed to specific client connections. The payload and its
  /// target clients are broadcast through a channel for the server to deliver.
  ///
  /// # Arguments
  ///
  /// * `msg` - The M2sModDirect message to process
  /// * `payload` - The payload buffer containing the message data
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` when the message is successfully processed and acknowledged.
  ///
  /// # Errors
  ///
  /// Returns an error if the broadcast channel fails to send the payload.
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
    let _ = self.payload_tx.try_broadcast(outbound_payload);

    // Send acknowledgment
    let ack_msg = Message::M2sModDirectAck(M2sModDirectAckParameters { id: params.id });
    self.tx.send_message(ack_msg);

    trace!(handler = self.handler, id = params.id, "M2sModDirect message processed successfully");

    Ok(())
  }
}

pub struct S2mDispatcherFactory<M: Modulator>(Arc<RwLock<S2mDispatcherFactoryInner<M>>>);

impl<M: Modulator> Clone for S2mDispatcherFactory<M> {
  fn clone(&self) -> Self {
    Self(self.0.clone())
  }
}

pub struct S2mDispatcherFactoryInner<M: Modulator> {
  /// The server configuration.
  config: Arc<S2mServerConfig>,

  /// The modulator interface.
  modulator: Arc<M>,

  /// The M2S client, if needed.
  m2s_client: Option<M2sClient>,

  /// Handle to the private payload reader task.
  payload_reader_handle: Option<monoio::task::JoinHandle<()>>,

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

// ===== impl S2mDispatcherFactory =====

impl<M: Modulator> S2mDispatcherFactory<M> {
  pub fn new(config: Arc<S2mServerConfig>, modulator: Arc<M>) -> Self {
    let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);

    let inner = S2mDispatcherFactoryInner {
      config,
      modulator,
      shutdown_tx,
      shutdown_rx,
      payload_reader_handle: None,
      m2s_client: None,
    };

    Self(Arc::new(RwLock::new(inner)))
  }

  async fn payload_reader_loop(
    mut rx: async_broadcast::Receiver<OutboundPrivatePayload>,
    m2s_client: M2sClient,
    shutdown_rx: async_channel::Receiver<()>,
  ) {
    loop {
      futures::select! {
        result = rx.recv().fuse() => {
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
        _ = shutdown_rx.recv().fuse() => {
          break;
        }
      }
    }
  }
}

#[async_trait::async_trait(?Send)]
impl<M: Modulator> narwhal_common::conn::DispatcherFactory<S2mDispatcher<M>> for S2mDispatcherFactory<M> {
  async fn create(&mut self, handler: usize, tx: ConnTx) -> S2mDispatcher<M> {
    let inner = self.0.read().await;

    S2mDispatcher::new(handler, inner.config.clone(), inner.modulator.clone(), tx)
  }

  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    let mut inner = self.0.write().await;

    assert!(!inner.shutdown_tx.is_closed());

    if inner.modulator.operations().await?.contains(Operation::ReceivePrivatePayload) {
      // Initialize the M2S client
      let m2s_client = M2sClient::new(inner.config.m2s_client.clone().into())?;
      inner.m2s_client = Some(m2s_client.clone());

      // Spawn the payload reader loop
      let response = inner.modulator.receive_private_payload(ReceivePrivatePayloadRequest {}).await?;
      let rx = response.receiver;
      let shutdown_rx = inner.shutdown_rx.clone();

      let handle = monoio::spawn(Self::payload_reader_loop(rx, m2s_client.clone(), shutdown_rx));
      inner.payload_reader_handle = Some(handle);
    }
    Ok(())
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    let mut inner = self.0.write().await;

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
}

impl<M: Modulator> Debug for S2mDispatcher<M> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("S2mDispatcher").field("handler", &self.handler).field("config", &self.config).finish()
  }
}

impl<M: Modulator> S2mDispatcher<M> {
  /// Initializes the dispatcher with the given parameters.
  pub fn new(handler: usize, config: Arc<S2mServerConfig>, modulator: Arc<M>, tx: ConnTx) -> Self {
    S2mDispatcher { handler, config, modulator, tx }
  }
}

#[async_trait::async_trait(?Send)]
impl<M: Modulator> narwhal_common::conn::Dispatcher for S2mDispatcher<M> {
  /// Dispatches incoming messages based on the current connection state.
  ///
  /// This is the main entry point for message handling that:
  /// * Routes connecting state messages to connection establishment
  /// * Routes authenticated state messages to appropriate handlers
  /// * Rejects messages in other states
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to dispatch
  /// * `payload` - Optional payload buffer associated with the message
  /// * `state` - Current connection state
  ///
  /// # Returns
  ///
  /// Returns `Ok(Some(new_state))` if a state transition should occur, or
  /// `Ok(None)` if the state should remain unchanged.
  ///
  /// # Errors
  ///
  /// Returns an `UnexpectedMessage` error if the message cannot be handled
  /// in the current state.
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
  /// Handles the initial connection handshake with a modulator client.
  ///
  /// This method processes the S2M connect message by:
  /// * Validating the protocol version
  /// * Negotiating the heartbeat interval based on client preferences and server limits
  /// * Sending a connection acknowledgment with:
  ///   - The modulator's protocol name (if any)
  ///   - Supported operations
  ///   - Negotiated heartbeat interval
  ///   - Server configuration limits
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to process. Must be a `Message::S2mConnect` variant.
  ///
  /// # Returns
  ///
  /// Returns the negotiated heartbeat interval as a `Duration` if the connection is accepted.
  ///
  /// # Errors
  ///
  /// Returns an error in the following cases:
  /// * If the protocol version is not supported
  /// * If the message is not a connect message
  async fn dispatch_message_in_connecting_state(&mut self, msg: Message) -> anyhow::Result<Duration> {
    match msg {
      Message::S2mConnect(params) => {
        if params.protocol_version != 1 {
          return Err(narwhal_protocol::Error::new(UnsupportedProtocolVersion).into());
        }
        let config = self.config.server.clone();

        // Validate the shared secret
        let shared_secret: Option<StringAtom> =
          { if !config.shared_secret.is_empty() { Some(config.shared_secret.as_str().into()) } else { None } };

        if shared_secret.is_some() && params.secret != shared_secret {
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

  /// Routes messages received in the authenticated state to their appropriate handlers.
  ///
  /// # Arguments
  ///
  /// * `msg` - The message to dispatch
  /// * `payload` - Optional payload buffer associated with the message
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the message was successfully handled by one of the specialized handlers.
  ///
  /// # Errors
  ///
  /// Returns an `UnexpectedMessage` error if the message type is not supported in the authenticated state.
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

  /// Handles an authentication message by delegating to the configured modulator.
  ///
  /// # Arguments
  ///
  /// * `msg` - The authentication message to process. Must be a `Message::S2mAuth` variant.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the authentication message was handled successfully,
  /// regardless of whether the authentication itself succeeded or failed.
  ///
  /// # Errors
  ///
  /// Returns an error in the following cases:
  /// * If the modulator doesn't support authentication operation
  /// * If the modulator's authenticate call fails
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
        Message::S2mAuthAck(S2mAuthAckParameters { id, challenge: None, username: Some(username), succeeded: true })
      },
      AuthResult::Continue { challenge } => {
        Message::S2mAuthAck(S2mAuthAckParameters { id, challenge: Some(challenge), username: None, succeeded: false })
      },
      AuthResult::Failure => {
        Message::S2mAuthAck(S2mAuthAckParameters { id, challenge: None, username: None, succeeded: false })
      },
    };

    // Send the reply message.
    self.tx.send_message(reply_msg);

    trace!(handler = self.handler, id = id, "handled Auth message");

    Ok(())
  }

  /// Handles a direct modulator message by delegating to the configured modulator.
  ///
  /// This method processes `S2mModDirect` messages that enable direct communication
  /// between clients and the modulator. The payload is passed directly to the
  /// modulator's `client_direct` method for custom processing.
  ///
  /// # Errors
  ///
  /// Returns an error in the following cases:
  /// * If the modulator doesn't support the `ClientDirect` operation
  /// * If the modulator's `client_direct` call fails
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
    self.tx.send_message(Message::S2mModDirectAck(S2mModDirectAckParameters { id: params.id, valid }));

    trace!(handler = self.handler, id = params.id, "handled ModDirect message");

    Ok(())
  }

  /// Handles a payload forward message by delegating to the configured modulator.
  ///
  /// # Arguments
  ///
  /// * `msg` - The forwarding message to process. Must be a `Message::ForwardPayload` variant.
  /// * `payload` - The payload buffer to be forwarded/validated.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the validation was performed and the response was sent,
  /// regardless of whether the payload was valid or not.
  ///
  /// # Errors
  ///
  /// Returns an error in the following cases:
  /// * If the modulator doesn't support payload forwarding operation
  /// * If the modulator's forward_payload call fails
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

  /// Handles an incoming `ForwardEvent` message by forwarding the event to the modulator.
  ///
  /// This method checks if the modulator supports the `ForwardEvent` operation. If supported,
  /// it extracts the event parameters from the message and calls the modulator's
  /// `forward_event` method with the constructed [`Event`]. Logs the handling of the event
  /// and returns an error if the operation is not supported or if forwarding fails.
  ///
  /// # Arguments
  ///
  /// * `msg` - The [`Message`] containing the `ModForwardEvent` parameters to process.
  ///
  /// # Returns
  ///
  /// An [`anyhow::Result`] indicating success or failure of the event forwarding operation.
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

    self.tx.send_message(Message::S2mForwardEventAck(S2mForwardEventAckParameters { id: params.id }));

    trace!(handler = self.handler, id = params.id, "handled ForwardEvent message");

    Ok(())
  }
}
