// SPDX-License-Identifier: BSD-3-Clause

use narwhal_protocol::Message;
use narwhal_util::{pool::PoolBuffer, string_atom::StringAtom};

/// A resource that uniquely identifies a connection within a single domain.
pub struct Resource {
  /// The domain where the connection resides. A value of `None` indicates
  /// that the connection is on the local domain.
  pub domain: Option<StringAtom>,

  /// The handler ID for the connection within the specified domain.
  pub handler: usize,
}

/// Trait for transmitting messages across the network or locally.
///
/// This trait defines the interface for components that can send messages,
/// either with or without additional payload data.
pub trait Transmitter: Send + Sync + 'static {
  /// Send a message without additional payload.
  ///
  /// # Arguments
  /// * `message` - The message to be sent
  fn send_message(&self, message: Message);

  /// Send a message with an optional payload buffer.
  ///
  /// # Arguments
  /// * `message` - The protocol message to be sent
  /// * `payload_opt` - Optional payload data buffer
  fn send_message_with_payload(&self, message: Message, payload_opt: Option<PoolBuffer>);

  /// Returns the resource identifier for this transmitter's connection.
  ///
  /// # Returns
  /// A `Resource` struct containing the domain and handler information for this connection.
  fn resource(&self) -> Resource;
}
