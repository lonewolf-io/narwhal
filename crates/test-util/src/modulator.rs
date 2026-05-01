// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;

use async_broadcast;
use futures::future::BoxFuture;

use narwhal_modulator::Modulator;
use narwhal_modulator::modulator::{
  AuthRequest, AuthResponse, AuthResult, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResponse,
  ForwardBroadcastPayloadResult, ForwardEventRequest, ForwardEventResponse, Operation, Operations,
  OutboundPrivatePayload, ReceivePrivatePayloadRequest, ReceivePrivatePayloadResponse, SendPrivatePayloadRequest,
  SendPrivatePayloadResponse, SendPrivatePayloadResult,
};
use narwhal_protocol::{Event, Nid};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

type AuthHandler = Arc<dyn Fn(AuthRequest) -> BoxFuture<'static, anyhow::Result<AuthResponse>> + Send + Sync>;
type ForwardMessagePayloadHandler = Arc<
  dyn Fn(ForwardBroadcastPayloadRequest) -> BoxFuture<'static, anyhow::Result<ForwardBroadcastPayloadResponse>>
    + Send
    + Sync,
>;
type ForwardEventHandler =
  Arc<dyn Fn(ForwardEventRequest) -> BoxFuture<'static, anyhow::Result<ForwardEventResponse>> + Send + Sync>;
type SendPrivatePayloadHandler = Arc<
  dyn Fn(SendPrivatePayloadRequest) -> BoxFuture<'static, anyhow::Result<SendPrivatePayloadResponse>> + Send + Sync,
>;
type ReceivePrivatePayloadHandler = Arc<
  dyn Fn(ReceivePrivatePayloadRequest) -> BoxFuture<'static, anyhow::Result<ReceivePrivatePayloadResponse>>
    + Send
    + Sync,
>;

#[derive(Clone)]
pub struct TestModulator {
  auth_handler: Option<AuthHandler>,
  forward_message_payload_handler: Option<ForwardMessagePayloadHandler>,
  forward_event_handler: Option<ForwardEventHandler>,
  send_private_payload_handler: Option<SendPrivatePayloadHandler>,
  receive_private_payload_handler: Option<ReceivePrivatePayloadHandler>,
}

impl std::fmt::Debug for TestModulator {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TestModulator")
      .field("auth_handler", &self.auth_handler.is_some())
      .field("forward_message_payload_handler", &self.forward_message_payload_handler.is_some())
      .field("forward_event_handler", &self.forward_event_handler.is_some())
      .field("send_private_payload_handler", &self.send_private_payload_handler.is_some())
      .field("receive_private_payload_handler", &self.receive_private_payload_handler.is_some())
      .finish()
  }
}

// === impl TestModulator ===

impl TestModulator {
  pub fn new() -> Self {
    Self {
      auth_handler: None,
      forward_message_payload_handler: None,
      forward_event_handler: None,
      send_private_payload_handler: None,
      receive_private_payload_handler: None,
    }
  }

  pub fn with_auth_handler<F, Fut>(mut self, handler: F) -> Self
  where
    F: Fn(StringAtom) -> Fut + Send + Sync + 'static,
    Fut: futures::Future<Output = anyhow::Result<AuthResult>> + Send + 'static,
  {
    self.auth_handler = Some(Arc::new(move |request| {
      let fut = handler(request.token);
      Box::pin(async move { fut.await.map(|result| AuthResponse { result }) })
    }));
    self
  }

  pub fn with_forward_message_payload_handler<F, Fut>(mut self, handler: F) -> Self
  where
    F: Fn(PoolBuffer, Nid, StringAtom) -> Fut + Send + Sync + 'static,
    Fut: futures::Future<Output = anyhow::Result<ForwardBroadcastPayloadResult>> + Send + 'static,
  {
    self.forward_message_payload_handler = Some(Arc::new(move |request| {
      let fut = handler(request.payload, request.from, request.channel_handler);
      Box::pin(async move { fut.await.map(|result| ForwardBroadcastPayloadResponse { result }) })
    }));
    self
  }

  pub fn with_forward_event_handler<F, Fut>(mut self, handler: F) -> Self
  where
    F: Fn(Event) -> Fut + Send + Sync + 'static,
    Fut: futures::Future<Output = anyhow::Result<()>> + Send + 'static,
  {
    self.forward_event_handler = Some(Arc::new(move |request| {
      let fut = handler(request.event);
      Box::pin(async move { fut.await.map(|_| ForwardEventResponse {}) })
    }));
    self
  }

  pub fn with_send_private_payload_handler<F, Fut>(mut self, handler: F) -> Self
  where
    F: Fn(PoolBuffer, StringAtom) -> Fut + Send + Sync + 'static,
    Fut: futures::Future<Output = anyhow::Result<SendPrivatePayloadResult>> + Send + 'static,
  {
    self.send_private_payload_handler = Some(Arc::new(move |request| {
      let fut = handler(request.payload, request.from);
      Box::pin(async move { fut.await.map(|result| SendPrivatePayloadResponse { result }) })
    }));
    self
  }

  pub fn with_receive_private_payload_handler<F, Fut>(mut self, handler: F) -> Self
  where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: futures::Future<Output = anyhow::Result<async_broadcast::Receiver<OutboundPrivatePayload>>> + Send + 'static,
  {
    self.receive_private_payload_handler = Some(Arc::new(move |_request| {
      let fut = handler();
      Box::pin(async move { fut.await.map(|receiver| ReceivePrivatePayloadResponse { receiver }) })
    }));
    self
  }
}

impl Default for TestModulator {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait::async_trait(?Send)]
impl Modulator for TestModulator {
  async fn protocol_name(&self) -> anyhow::Result<StringAtom> {
    Ok("test-modulator-proto/1.0".into())
  }

  async fn operations(&self) -> anyhow::Result<Operations> {
    let mut ops = Operations::new();
    if self.auth_handler.is_some() {
      ops = ops.with(Operation::Authenticate);
    }
    if self.forward_message_payload_handler.is_some() {
      ops = ops.with(Operation::ForwardBroadcastPayload);
    }
    if self.forward_event_handler.is_some() {
      ops = ops.with(Operation::ForwardEvent);
    }
    if self.send_private_payload_handler.is_some() {
      ops = ops.with(Operation::SendPrivatePayload);
    }
    if self.receive_private_payload_handler.is_some() {
      ops = ops.with(Operation::ReceivePrivatePayload);
    }
    Ok(ops)
  }

  async fn authenticate(&self, request: AuthRequest) -> anyhow::Result<AuthResponse> {
    match &self.auth_handler {
      Some(handler) => handler(request).await,
      None => Err(anyhow::anyhow!("auth handler not set")),
    }
  }

  async fn forward_broadcast_payload(
    &self,
    request: ForwardBroadcastPayloadRequest,
  ) -> anyhow::Result<ForwardBroadcastPayloadResponse> {
    match &self.forward_message_payload_handler {
      Some(handler) => handler(request).await,
      None => Err(anyhow::anyhow!("forward payload handler not set")),
    }
  }

  async fn forward_event(&self, request: ForwardEventRequest) -> anyhow::Result<ForwardEventResponse> {
    match &self.forward_event_handler {
      Some(handler) => handler(request).await,
      None => Err(anyhow::anyhow!("forward event handler not set")),
    }
  }

  async fn send_private_payload(
    &self,
    request: SendPrivatePayloadRequest,
  ) -> anyhow::Result<SendPrivatePayloadResponse> {
    match &self.send_private_payload_handler {
      Some(handler) => handler(request).await,
      None => Err(anyhow::anyhow!("send private payload handler not set")),
    }
  }

  async fn receive_private_payload(
    &self,
    request: ReceivePrivatePayloadRequest,
  ) -> anyhow::Result<ReceivePrivatePayloadResponse> {
    match &self.receive_private_payload_handler {
      Some(handler) => handler(request).await,
      None => Err(anyhow::anyhow!("send public payload handler not set")),
    }
  }
}

pub fn default_s2m_config(shared_secret: &str) -> narwhal_modulator::S2mServerConfig {
  narwhal_modulator::S2mServerConfig {
    server: narwhal_modulator::ServerConfig {
      listener: narwhal_modulator::ListenerConfig {
        network: narwhal_modulator::TCP_NETWORK.to_string(),
        bind_address: "127.0.0.1:0".to_string(), // use a random port
        ..Default::default()
      },
      shared_secret: shared_secret.to_string(),
      limits: narwhal_modulator::Limits { max_connections: 50, max_message_size: 256 * 1024, ..Default::default() },
      ..Default::default()
    },
    m2s_client: narwhal_modulator::M2sClientConfig::default(),
  }
}
