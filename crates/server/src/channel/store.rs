// SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use monoio::io::AsyncWriteRent;
use narwhal_protocol::{Message, Nid};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use super::manager::{ChannelAcl, ChannelConfig};

/// Persisted channel metadata, restored on server startup.
pub struct PersistedChannel {
  pub handler: StringAtom,
  pub owner: Option<Nid>,
  pub config: ChannelConfig,
  pub acl: ChannelAcl,
  pub members: HashSet<Nid>,
  pub seq: u64,
}

/// Storage backend for persisting channel metadata.
#[async_trait(?Send)]
pub trait ChannelStore: Send + Sync + 'static {
  /// Persists channel metadata.
  /// Called on channel creation (when persist=true) and on any metadata update.
  async fn save_channel(&self, channel: &PersistedChannel) -> anyhow::Result<()>;

  /// Removes all persisted metadata for the given channel handler.
  /// Called when persist is toggled to false or the channel is deleted.
  async fn delete_channel(&self, handler: &StringAtom) -> anyhow::Result<()>;

  /// Returns all persisted channel handler IDs.
  async fn load_channel_handlers(&self) -> anyhow::Result<Arc<[StringAtom]>>;

  /// Loads persisted channel metadata.
  async fn load_channel(&self, handler: &StringAtom) -> anyhow::Result<PersistedChannel>;
}

/// Append-only log for persisting broadcast messages for a single channel.
#[async_trait(?Send)]
pub trait MessageLog {
  /// Appends a message to the log, buffering it in memory without flushing to disk.
  /// When the number of stored messages exceeds `max_messages`, the oldest entries should be evicted.
  /// Call `flush` to persist buffered writes to durable storage.
  async fn append(&self, message: &Message, payload: &PoolBuffer, max_messages: u32) -> anyhow::Result<()>;

  /// Removes all persisted messages.
  async fn delete(&self) -> anyhow::Result<()>;

  /// Flushes any buffered writes to durable storage.
  async fn flush(&self) -> anyhow::Result<()>;

  /// Streams messages with seq > `from_seq`, ordered by seq ascending, up to `limit` entries,
  /// writing their wire-format representation directly to the given writer.
  /// Returns the number of messages written.
  async fn write_history<W: AsyncWriteRent>(&self, from_seq: u64, limit: u32, writer: &mut W) -> anyhow::Result<u32>
  where
    Self: Sized;
}
