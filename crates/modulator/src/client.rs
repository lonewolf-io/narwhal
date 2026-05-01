// SPDX-License-Identifier: BSD-3-Clause

use narwhal_client::compio::s2m::S2mClient;

use narwhal_protocol::{
  Message, S2mAuthParameters, S2mForwardBroadcastPayloadParameters, S2mForwardEventParameters, S2mModDirectParameters,
};
use narwhal_util::string_atom::StringAtom;

use crate::modulator::{
  AuthRequest, AuthResponse, AuthResult, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResponse,
  ForwardBroadcastPayloadResult, ForwardEventRequest, ForwardEventResponse, Operation, Operations,
  ReceivePrivatePayloadRequest, ReceivePrivatePayloadResponse, SendPrivatePayloadRequest, SendPrivatePayloadResponse,
  SendPrivatePayloadResult,
};

#[async_trait::async_trait(?Send)]
impl crate::Modulator for S2mClient {
  /// Returns the modulator's protocol identifier.
  ///
  /// # Returns
  ///
  /// A [`StringAtom`] containing the modulator's protocol identifier.
  async fn protocol_name(&self) -> anyhow::Result<StringAtom> {
    let (_, extra_session_info) = self.session_info().await?;
    Ok(extra_session_info.protocol_name)
  }

  /// Returns modulator's supported operations.
  ///
  /// # Returns
  ///
  /// An [`Operations`] value containing the supported operations.
  async fn operations(&self) -> anyhow::Result<Operations> {
    let (_, extra_session_info) = self.session_info().await?;
    Ok(Operations::from(extra_session_info.operation_mask))
  }

  /// Authenticates with the modulator server using the provided token.
  ///
  /// This method sends an authentication request to the modulator server and waits
  /// for the response. The authentication process may result in success, failure,
  /// or a continuation request for multi-step authentication.
  ///
  /// # Arguments
  ///
  /// * `request` - The authentication request containing the token to send to the server.
  ///
  /// # Returns
  ///
  /// Returns a `Result` containing an `AuthResponse` with a result that can be:
  /// - `AuthResult::Success { username }` - Authentication succeeded with the given username
  /// - `AuthResult::Continue { challenge }` - Server requires additional authentication steps
  /// - `AuthResult::Failure` - Authentication failed
  async fn authenticate(&self, request: AuthRequest) -> anyhow::Result<AuthResponse> {
    if !self.operations().await?.contains(Operation::Authenticate) {
      return Err(anyhow::anyhow!("authentication operation not supported"));
    }

    let correlation_id = self.next_id().await;

    let handle =
      self.send_message(Message::S2mAuth(S2mAuthParameters { id: correlation_id, token: request.token }), None).await?;

    match handle.response().await {
      Ok((msg, _)) => match msg {
        Message::S2mAuthAck(params) => {
          if params.succeeded {
            Ok(AuthResponse { result: AuthResult::Success { username: params.username.unwrap() } })
          } else {
            match params.challenge {
              Some(challenge) => Ok(AuthResponse { result: AuthResult::Continue { challenge } }),
              None => Ok(AuthResponse { result: AuthResult::Failure }),
            }
          }
        },
        Message::Error(e) => Err(anyhow::anyhow!("authentication error: {}", e.reason)),
        _ => Err(anyhow::anyhow!("unexpected message type during authentication")),
      },
      Err(e) => Err(anyhow::anyhow!("authentication failed: {}", e)),
    }
  }

  /// Forwards a message payload to the modulator server for validation and processing.
  ///
  /// This method sends a message payload along with its metadata to the modulator server
  /// for validation. The server will check whether the payload meets the required criteria
  /// and may optionally return a modified version of the payload if transformations are needed.
  ///
  /// # Arguments
  ///
  /// * `request` - A [`ForwardBroadcastPayloadRequest`] containing the payload to validate,
  ///   the sender's ID, and the target channel handler
  ///
  /// # Returns
  ///
  /// Returns a `Result` containing a [`ForwardBroadcastPayloadResponse`] which includes
  /// the validation result.
  async fn forward_broadcast_payload(
    &self,
    request: ForwardBroadcastPayloadRequest,
  ) -> anyhow::Result<ForwardBroadcastPayloadResponse> {
    if !self.operations().await?.contains(Operation::ForwardBroadcastPayload) {
      return Err(anyhow::anyhow!("payload forwarding operation not supported"));
    }

    let params = S2mForwardBroadcastPayloadParameters {
      id: self.next_id().await,
      from: request.from.into(),
      channel: request.channel_handler,
      length: request.payload.len() as u32,
    };

    let handle = self.send_message(Message::S2mForwardBroadcastPayload(params), Some(request.payload)).await?;

    match handle.response().await {
      Ok((msg, payload_opt)) => match msg {
        Message::S2mForwardBroadcastPayloadAck(params) => {
          let result = if params.valid {
            match payload_opt {
              Some(altered) => ForwardBroadcastPayloadResult::ValidWithAlteration { altered_payload: altered },
              None => ForwardBroadcastPayloadResult::Valid,
            }
          } else {
            ForwardBroadcastPayloadResult::Invalid
          };
          Ok(ForwardBroadcastPayloadResponse { result })
        },
        _ => Err(anyhow::anyhow!("unexpected message type during payload forwarding")),
      },
      Err(e) => anyhow::bail!(e),
    }
  }

  /// Forwards an event to the modulator server for processing.
  ///
  /// # Arguments
  ///
  /// * `request` - A `ForwardEventRequest` containing the event information to be forwarded,
  ///   including the event kind, channel, nid and owner.
  ///
  /// # Returns
  ///
  /// Returns a `Result<ForwardEventResponse>` indicating success or failure.
  async fn forward_event(&self, request: ForwardEventRequest) -> anyhow::Result<ForwardEventResponse> {
    if !self.operations().await?.contains(Operation::ForwardEvent) {
      return Err(anyhow::anyhow!("event forwarding operation not supported"));
    }

    let params = S2mForwardEventParameters {
      id: self.next_id().await,
      kind: request.event.kind.into(),
      channel: request.event.channel,
      nid: request.event.nid,
      owner: request.event.owner,
    };

    let handle = self.send_message(Message::S2mForwardEvent(params), None).await?;

    match handle.response().await {
      Ok((msg, _)) => match msg {
        Message::S2mForwardEventAck { .. } => Ok(ForwardEventResponse {}),
        _ => Err(anyhow::anyhow!("unexpected message type during event forwarding")),
      },
      Err(e) => Err(anyhow::anyhow!("forwarding event failed: {}", e)),
    }
  }

  /// Sends a private payload directly from the client to the modulator.
  ///
  /// # Arguments
  ///
  /// * `request` - A [`SendPrivatePayloadRequest`] containing the payload and sender information
  ///
  /// # Returns
  ///
  /// Returns a `Result` containing a [`SendPrivatePayloadResponse`] which indicates
  /// whether the private payload was successfully processed by the modulator.
  async fn send_private_payload(
    &self,
    request: SendPrivatePayloadRequest,
  ) -> anyhow::Result<SendPrivatePayloadResponse> {
    if !self.operations().await?.contains(Operation::SendPrivatePayload) {
      return Err(anyhow::anyhow!("send private payload operation not supported"));
    }

    let correlation_id = self.next_id().await;

    let handle = self
      .send_message(
        Message::S2mModDirect(S2mModDirectParameters {
          id: correlation_id,
          from: request.from,
          length: request.payload.len() as u32,
        }),
        Some(request.payload),
      )
      .await?;

    match handle.response().await {
      Ok((msg, _)) => match msg {
        Message::S2mModDirectAck(params) => Ok(SendPrivatePayloadResponse {
          result: if params.valid { SendPrivatePayloadResult::Valid } else { SendPrivatePayloadResult::Invalid },
        }),
        _ => Err(anyhow::anyhow!("unexpected message type during client direct operation")),
      },
      Err(e) => anyhow::bail!(e),
    }
  }

  /// Not supported by S2mClient.
  ///
  /// This method is part of the [`Modulator`] trait but is not implemented for S2mClient.
  /// S2mClient does not support receiving private payloads directly from the modulator.
  /// This functionality should be handled via the M2S service instead.
  ///
  /// # Panics
  ///
  /// Always panics with `unreachable!()` since this operation should never be called
  /// on S2mClient.
  async fn receive_private_payload(
    &self,
    _request: ReceivePrivatePayloadRequest,
  ) -> anyhow::Result<ReceivePrivatePayloadResponse> {
    unreachable!("receive_private_payload is not supported by S2mClient");
  }
}
