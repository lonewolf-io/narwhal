// SPDX-License-Identifier: BSD-3-Clause

use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use narwhal_common::runtime::AsyncWrite;
use narwhal_protocol::{Message, Nid};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use super::manager::{ChannelAcl, ChannelConfig};

/// Persisted channel metadata, restored on server startup.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PersistedChannel {
  pub handler: StringAtom,
  pub owner: Option<Nid>,
  pub config: ChannelConfig,
  pub acl: ChannelAcl,
  #[serde(with = "rc_slice_serde")]
  pub members: Rc<[Nid]>,
}

mod rc_slice_serde {
  use std::rc::Rc;

  use serde::{Deserialize, Deserializer, Serialize, Serializer};

  pub fn serialize<T: Serialize, S: Serializer>(data: &Rc<[T]>, serializer: S) -> Result<S::Ok, S::Error> {
    data.as_ref().serialize(serializer)
  }

  pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Rc<[T]>, D::Error> {
    let v: Vec<T> = Vec::deserialize(deserializer)?;
    Ok(Rc::from(v))
  }
}

/// Storage backend for persisting channel metadata.
#[async_trait(?Send)]
pub trait ChannelStore: Clone + Send + Sync + 'static {
  /// Persists channel metadata.
  /// Called on channel creation (when persist=true) and on any metadata update.
  /// Returns the storage hash that identifies this channel on disk.
  async fn save_channel(&self, channel: &PersistedChannel) -> anyhow::Result<StringAtom>;

  /// Removes all persisted metadata for the given channel storage hash.
  /// Called when persist is toggled to false or the channel is deleted.
  async fn delete_channel(&self, hash: &StringAtom) -> anyhow::Result<()>;

  /// Returns the storage hashes of all persisted channels.
  async fn load_channel_hashes(&self) -> anyhow::Result<Arc<[StringAtom]>>;

  /// Loads persisted channel metadata by its storage hash.
  async fn load_channel(&self, hash: &StringAtom) -> anyhow::Result<PersistedChannel>;
}

/// Factory for creating per-channel message logs.
pub trait MessageLogFactory: Clone + Send + Sync + 'static {
  /// The message log type produced by this factory.
  type Log: MessageLog;

  /// Creates a message log for the given channel handler.
  fn create(&self, handler: &StringAtom) -> Self::Log;
}

/// Append-only log for persisting broadcast messages for a single channel.
#[async_trait(?Send)]
pub trait MessageLog: 'static {
  /// Appends a message to the log, buffering it in memory without flushing to disk.
  /// When the number of stored messages exceeds `max_messages`, the oldest entries should be evicted.
  /// Call `flush` to persist buffered writes to durable storage.
  async fn append(&self, message: &Message, payload: &PoolBuffer, max_messages: u32) -> anyhow::Result<()>;

  /// Removes all persisted messages.
  async fn delete(&self) -> anyhow::Result<()>;

  /// Flushes any buffered writes to durable storage.
  async fn flush(&self) -> anyhow::Result<()>;

  /// Returns the highest sequence number stored in the log, or 0 if the log is empty.
  /// Used during restore to derive the correct starting seq for new broadcasts.
  async fn last_seq(&self) -> anyhow::Result<u64>;

  /// Streams messages with seq > `from_seq`, ordered by seq ascending, up to `limit` entries,
  /// writing their wire-format representation directly to the given writer.
  /// Returns the number of messages written.
  async fn write_history<W: AsyncWrite>(&self, from_seq: u64, limit: u32, writer: &mut W) -> anyhow::Result<u32>
  where
    Self: Sized;
}

/// A no-op message log that discards all writes and returns empty results.
pub struct NoopMessageLog;

// === impl NoopMessageLog ===

#[async_trait(?Send)]
impl MessageLog for NoopMessageLog {
  async fn append(&self, _message: &Message, _payload: &PoolBuffer, _max_messages: u32) -> anyhow::Result<()> {
    Ok(())
  }

  async fn delete(&self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn flush(&self) -> anyhow::Result<()> {
    Ok(())
  }

  async fn last_seq(&self) -> anyhow::Result<u64> {
    Ok(0)
  }

  async fn write_history<W: AsyncWrite>(&self, _from_seq: u64, _limit: u32, _writer: &mut W) -> anyhow::Result<u32>
  where
    Self: Sized,
  {
    Ok(0)
  }
}

/// A no-op message log factory that produces `NoopMessageLog` instances.
#[derive(Clone)]
pub struct NoopMessageLogFactory;

// === impl NoopMessageLogFactory ===

impl MessageLogFactory for NoopMessageLogFactory {
  type Log = NoopMessageLog;

  fn create(&self, _handler: &StringAtom) -> NoopMessageLog {
    NoopMessageLog
  }
}
