// SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use narwhal_common::runtime::AsyncWrite;
use narwhal_protocol::{Message, Nid};
use narwhal_server::channel::store::{ChannelStore, MessageLog, MessageLogFactory, PersistedChannel};
use narwhal_server::channel::{ChannelAcl, ChannelConfig};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

/// A channel store that can be toggled to fail on `save_channel`.
#[derive(Clone, Default)]
pub struct FailingChannelStore {
  should_fail: Arc<AtomicBool>,
}

impl FailingChannelStore {
  pub fn new() -> Self {
    Self { should_fail: Arc::new(AtomicBool::new(false)) }
  }

  /// Sets whether `save_channel` should return an error.
  pub fn set_fail(&self, fail: bool) {
    self.should_fail.store(fail, Ordering::SeqCst);
  }
}

#[async_trait(?Send)]
impl ChannelStore for FailingChannelStore {
  async fn save_channel(&self, channel: &PersistedChannel) -> anyhow::Result<StringAtom> {
    if self.should_fail.load(Ordering::SeqCst) {
      return Err(anyhow::anyhow!("injected store failure"));
    }
    Ok(channel.handler.clone())
  }

  async fn delete_channel(&self, _hash: &StringAtom) -> anyhow::Result<()> {
    Ok(())
  }

  async fn load_channel_hashes(&self) -> anyhow::Result<Arc<[StringAtom]>> {
    Ok(Arc::from([]))
  }

  async fn load_channel(&self, _hash: &StringAtom) -> anyhow::Result<PersistedChannel> {
    unimplemented!()
  }
}

/// A message log that can be toggled to fail on `append`.
pub struct FailingMessageLog {
  should_fail: Arc<AtomicBool>,
}

#[async_trait(?Send)]
impl MessageLog for FailingMessageLog {
  async fn append(&self, _message: &Message, _payload: &PoolBuffer, _max_messages: u32) -> anyhow::Result<()> {
    if self.should_fail.load(Ordering::SeqCst) {
      return Err(anyhow::anyhow!("injected message log failure"));
    }
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

/// A message log factory that produces `FailingMessageLog` instances sharing a single failure flag.
#[derive(Clone, Default)]
pub struct FailingMessageLogFactory {
  should_fail: Arc<AtomicBool>,
}

impl FailingMessageLogFactory {
  pub fn new() -> Self {
    Self { should_fail: Arc::new(AtomicBool::new(false)) }
  }

  /// Sets whether `append` on produced logs should return an error.
  pub fn set_fail(&self, fail: bool) {
    self.should_fail.store(fail, Ordering::SeqCst);
  }
}

impl MessageLogFactory for FailingMessageLogFactory {
  type Log = FailingMessageLog;

  fn create(&self, _handler: &StringAtom) -> FailingMessageLog {
    FailingMessageLog { should_fail: self.should_fail.clone() }
  }
}

/// Stored snapshot of a persisted channel (owned, thread-safe data).
struct StoredChannel {
  handler: StringAtom,
  owner: Option<Nid>,
  config: ChannelConfig,
  acl: ChannelAcl,
  members: Vec<Nid>,
}

/// An in-memory channel store for integration tests.
#[derive(Clone, Default)]
pub struct InMemoryChannelStore {
  channels: Arc<async_lock::Mutex<HashMap<StringAtom, StoredChannel>>>,
}

impl InMemoryChannelStore {
  pub fn new() -> Self {
    Self { channels: Arc::new(async_lock::Mutex::new(HashMap::new())) }
  }
}

#[async_trait(?Send)]
impl ChannelStore for InMemoryChannelStore {
  async fn save_channel(&self, channel: &PersistedChannel) -> anyhow::Result<StringAtom> {
    let handler = channel.handler.clone();
    let stored = StoredChannel {
      handler: handler.clone(),
      owner: channel.owner.clone(),
      config: channel.config.clone(),
      acl: channel.acl.clone(),
      members: channel.members.iter().cloned().collect(),
    };
    self.channels.lock().await.insert(handler.clone(), stored);
    Ok(handler)
  }

  async fn delete_channel(&self, hash: &StringAtom) -> anyhow::Result<()> {
    self.channels.lock().await.remove(hash);
    Ok(())
  }

  async fn load_channel_hashes(&self) -> anyhow::Result<Arc<[StringAtom]>> {
    let guard = self.channels.lock().await;
    Ok(guard.keys().cloned().collect::<Vec<_>>().into())
  }

  async fn load_channel(&self, hash: &StringAtom) -> anyhow::Result<PersistedChannel> {
    let guard = self.channels.lock().await;
    let stored = guard.get(hash).ok_or_else(|| anyhow::anyhow!("channel not found: {}", hash))?;
    Ok(PersistedChannel {
      handler: stored.handler.clone(),
      owner: stored.owner.clone(),
      config: stored.config.clone(),
      acl: stored.acl.clone(),
      members: Rc::from(stored.members.clone()),
    })
  }
}
