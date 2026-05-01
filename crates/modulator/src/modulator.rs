// SPDX-License-Identifier: BSD-3-Clause

use std::fmt::Debug;

use async_broadcast;
use async_trait::async_trait;

use narwhal_protocol::{Event, Nid};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

/// Represents individual operations that a modulator can perform.
///
/// Each variant corresponds to a specific capability that can be supported
/// by a [`Modulator`]. The enum is represented as a `u64` internally to
/// allow for efficient bitwise operations when used with [`Operations`].
///
/// Bit positions are wire contract: see the "Operation Bit Assignments"
/// table in `docs/PROTOCOL.md`. Existing values must not be reassigned.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operation {
  /// Authentication operation (wire name: `authenticate`).
  ///
  /// Enables validation of authentication tokens through the
  /// [`authenticate`](Modulator::authenticate) method.
  Authenticate = 1 << 0,

  /// Message payload forwarding operation (wire name: `forward-broadcast-payload`).
  ///
  /// Enables validation of broadcast message payloads received from clients through the
  /// [`forward_broadcast_payload`](Modulator::forward_broadcast_payload) method.
  ForwardBroadcastPayload = 1 << 1,

  /// Event forwarding operation (wire name: `forward-event`).
  ///
  /// Enables processing of system events and state changes through the
  /// [`forward_event`](Modulator::forward_event) method.
  ForwardEvent = 1 << 2,

  /// Send private payload operation (wire name: `send-private-payload`).
  ///
  /// Enables direct communication between clients and the modulator through the
  /// [`send_private_payload`](Modulator::send_private_payload) method.
  SendPrivatePayload = 1 << 3,

  /// Receive private payload operation (wire name: `receive-private-payload`).
  ///
  /// Enables receiving outbound private payloads from the modulator to clients through the
  /// [`receive_private_payload`](Modulator::receive_private_payload) method.
  ReceivePrivatePayload = 1 << 4,
}

/// A bitset representing a collection of supported [`Operation`]s.
///
/// This type provides a memory-efficient way to store and query which operations
/// a modulator supports.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Operations(u64);

impl Operations {
  /// Returns all available operations as a slice.
  ///
  /// This constant provides access to all possible operations that can be
  /// supported by a modulator.
  pub const ALL: &'static [Operation] = &[
    Operation::Authenticate,
    Operation::ForwardBroadcastPayload,
    Operation::ForwardEvent,
    Operation::SendPrivatePayload,
    Operation::ReceivePrivatePayload,
  ];

  /// Creates a new empty operations set.
  ///
  /// The returned set contains no operations initially. Use [`with`](Operations::with)
  /// to add operations to the set.
  #[inline(always)]
  pub fn new() -> Self {
    Self(0)
  }

  /// Adds an operation to the operation set.
  ///
  /// Returns a new operations set that includes the specified operation.
  ///
  /// # Arguments
  ///
  /// * `op` - The operation to add to the set
  ///
  /// # Returns
  ///
  /// A new [`Operations`] instance with the specified operation included
  #[inline(always)]
  pub const fn with(self, op: Operation) -> Self {
    Self(self.0 | op as u64)
  }

  /// Removes an operation from the operation set.
  ///
  /// Returns a new operations set that excludes the specified operation.
  ///
  /// # Arguments
  ///
  /// * `op` - The operation to remove from the set
  ///
  /// # Returns
  ///
  /// A new [`Operations`] instance with the specified operation excluded
  #[inline(always)]
  pub const fn remove(self, op: Operation) -> Self {
    Self(self.0 & !(op as u64))
  }

  /// Checks if the operation set contains a specific operation.
  ///
  /// # Arguments
  ///
  /// * `op` - The operation to check for
  ///
  /// # Returns
  ///
  /// `true` if the operation is present in the set, `false` otherwise
  #[inline(always)]
  pub const fn contains(self, op: Operation) -> bool {
    self.0 & op as u64 != 0
  }

  /// Returns the raw bitmask backing this operations set.
  #[inline(always)]
  pub const fn bits(self) -> u64 {
    self.0
  }
}

impl From<u64> for Operations {
  fn from(bits: u64) -> Self {
    Self(bits)
  }
}

/// Request for authentication.
#[derive(Debug)]
pub struct AuthRequest {
  /// The authentication token to validate
  pub token: StringAtom,
}

/// Result of an authentication attempt.
#[derive(Debug, PartialEq)]
pub enum AuthResult {
  /// Authentication was successful.
  ///
  /// Contains the authenticated username as a [`StringAtom`].
  Success { username: StringAtom },

  /// Authentication requires additional steps.
  ///
  /// Contains a challenge as a [`StringAtom`] that should be used for the next
  /// authentication attempt. This is useful for multi-step authentication
  /// protocols like SASL.
  Continue { challenge: StringAtom },

  /// Authentication failed.
  Failure,
}

/// Response from an authentication attempt.
#[derive(Debug, PartialEq)]
pub struct AuthResponse {
  /// The result of the authentication attempt
  pub result: AuthResult,
}

/// Request for forwarding a broadcast payload.
#[derive(Debug)]
pub struct ForwardBroadcastPayloadRequest {
  /// The payload buffer to validate
  pub payload: PoolBuffer,
  /// The sender's user identifier
  pub from: Nid,
  /// The target channel handler
  pub channel_handler: StringAtom,
}

/// Result of validating a broadcast message payload.
#[derive(Debug)]
pub enum ForwardBroadcastPayloadResult {
  /// The payload is valid and can be forwarded as-is.
  Valid,

  /// The payload is valid but has been altered by the modulator.
  ValidWithAlteration { altered_payload: PoolBuffer },

  /// The payload is invalid and should not be forwarded.
  Invalid,
}

/// Response from validating a broadcast message payload.
#[derive(Debug)]
pub struct ForwardBroadcastPayloadResponse {
  /// The result of the validation
  pub result: ForwardBroadcastPayloadResult,
}

/// Request for forwarding an event.
#[derive(Debug)]
pub struct ForwardEventRequest {
  /// The event to forward
  pub event: Event,
}

/// Response from forwarding an event.
#[derive(Debug)]
pub struct ForwardEventResponse {}

/// Request for sending a private payload.
#[derive(Debug)]
pub struct SendPrivatePayloadRequest {
  /// The payload buffer to send
  pub payload: PoolBuffer,
  /// The sender's username
  pub from: StringAtom,
}

/// Result of a client direct operation.
#[derive(Debug)]
pub enum SendPrivatePayloadResult {
  /// The payload was successfully processed.
  Valid,

  /// The payload was invalid and was not processed.
  Invalid,
}

/// Response from sending a private payload.
#[derive(Debug)]
pub struct SendPrivatePayloadResponse {
  /// The result of the operation
  pub result: SendPrivatePayloadResult,
}

/// Request for receiving private payloads.
#[derive(Debug)]
pub struct ReceivePrivatePayloadRequest {}

/// Response for receiving private payloads.
#[derive(Debug)]
pub struct ReceivePrivatePayloadResponse {
  /// The broadcast receiver for outbound private payloads
  pub receiver: async_broadcast::Receiver<OutboundPrivatePayload>,
}

/// Represents an outbound private payload from the modulator to one or more clients.
#[derive(Clone, Debug)]
pub struct OutboundPrivatePayload {
  /// the payload buffer containing the data to send to the clients.
  pub payload: PoolBuffer,

  /// the set of client identifiers to send the payload to.
  pub targets: Vec<StringAtom>,
}

/// A trait that defines a modulator component capable of performing various operations on data streams.
///
/// A modulator is a component that can modify, validate, or authenticate data streams.
/// It implements specific operations defined in the [`Operation`] enum and can be used
/// to extend the functionality of a server.
///
/// # Thread Safety
///
/// This trait requires its implementors to be [`Send`] and [`Sync`], making it safe to use
/// across thread boundaries. It also requires a `'static` lifetime.
#[async_trait(?Send)]
pub trait Modulator: Debug + Send + Sync + 'static {
  /// Returns the modulator's protocol identifier.
  ///
  /// # Returns
  ///
  /// A `Result` containing a [`StringAtom`] with the modulator's protocol identifier.
  ///
  /// # Errors
  ///
  /// Returns an error if the protocol name cannot be retrieved or if there's
  /// a communication failure with the modulator.
  async fn protocol_name(&self) -> anyhow::Result<StringAtom>;

  /// Returns the set of operations supported by this modulator.
  ///
  /// The returned [`Operations`] bitset indicates which operations from the
  /// [`Operation`] enum this modulator supports. This allows the system to
  /// determine the capabilities of the modulator at runtime.
  ///
  /// # Returns
  ///
  /// A `Result` containing an [`Operations`] value with the supported operations.
  ///
  /// # Errors
  ///
  /// Returns an error if the operations cannot be retrieved or if there's
  /// a communication failure with the modulator.
  async fn operations(&self) -> anyhow::Result<Operations>;

  /// Authenticates a request and returns the result of the authentication.
  ///
  /// This method is called when the modulator supports the [`Operation::Authenticate`]
  /// operation. It processes the provided authentication request and returns
  /// an appropriate [`AuthResponse`].
  ///
  /// # Arguments
  ///
  /// * `request` - An [`AuthRequest`] containing the authentication token to validate.
  ///
  /// # Returns
  ///
  /// A `Result` containing either an [`AuthResponse`] indicating the authentication
  /// outcome, or an error if the authentication process failed.
  async fn authenticate(&self, request: AuthRequest) -> anyhow::Result<AuthResponse>;

  /// Validates a broadcast payload from a client.
  ///
  /// This method is called when the modulator supports the [`Operation::ForwardBroadcastPayload`]
  /// operation. It validates broadcast message payloads received from clients,
  /// checking whether they meet the required criteria or constraints.
  ///
  /// # Arguments
  ///
  /// * `request`: A [`ForwardBroadcastPayloadRequest`] containing the payload to validate,
  ///   the sender's ID, and the target channel handler
  ///
  /// # Returns
  ///
  /// A `Result` containing a [`ForwardBroadcastPayloadResponse`] indicating the outcome of the validation.
  async fn forward_broadcast_payload(
    &self,
    request: ForwardBroadcastPayloadRequest,
  ) -> anyhow::Result<ForwardBroadcastPayloadResponse>;

  /// Forwards an event to the modulator for processing.
  ///
  /// This method is called when the modulator needs to handle an [`Event`] that represents
  /// a state change or activity within the system. Implementors can use this method to
  /// react to events, update internal state, or trigger side effects based on the event's
  /// metadata and context.
  ///
  /// # Arguments
  ///
  /// * `request` - A [`ForwardEventRequest`] containing the event to process.
  ///
  /// # Returns
  ///
  /// A `Result` containing a [`ForwardEventResponse`] indicating whether the event was successfully processed.
  async fn forward_event(&self, request: ForwardEventRequest) -> anyhow::Result<ForwardEventResponse>;

  /// Handles direct communication from a client to the modulator.
  ///
  /// This method is called when the modulator supports the [`Operation::SendPrivatePayload`]
  /// operation. It enables clients to communicate directly with the modulator,
  /// allowing for custom protocols and interactions.
  ///
  /// # Arguments
  ///
  /// * `request` - A [`SendPrivatePayloadRequest`] containing the payload and sender information
  ///
  /// # Returns
  ///
  /// A `Result` containing a [`SendPrivatePayloadResponse`] indicating the outcome of the communication.
  async fn send_private_payload(
    &self,
    request: SendPrivatePayloadRequest,
  ) -> anyhow::Result<SendPrivatePayloadResponse>;

  /// Establishes a receiver channel for outbound private payloads from the modulator.
  ///
  /// This method is called when the modulator supports the [`Operation::ReceivePrivatePayload`]
  /// operation. It returns a broadcast receiver that allows listening for direct
  /// modulator-to-client communications that bypass the host.
  ///
  /// # Arguments
  ///
  /// * `request` - A [`ReceivePrivatePayloadRequest`] for receiving private payloads
  ///
  /// # Returns
  ///
  /// A `Result` containing a [`ReceivePrivatePayloadResponse`] that includes a
  /// [`async_broadcast::Receiver`] for receiving [`OutboundPrivatePayload`] messages
  /// representing direct communications from the modulator to specified client targets.
  async fn receive_private_payload(
    &self,
    request: ReceivePrivatePayloadRequest,
  ) -> anyhow::Result<ReceivePrivatePayloadResponse>;
}
