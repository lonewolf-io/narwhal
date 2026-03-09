// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use async_channel::{Receiver, Sender};

use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_util::string_atom::StringAtom;

const DEFAULT_MAILBOX_CAPACITY: usize = 16384;

/// Deterministically maps a username to a shard index.
fn shard_for_user(username: &StringAtom, shard_count: usize) -> usize {
  let mut hasher = DefaultHasher::new();
  username.hash(&mut hasher);
  (hasher.finish() as usize) % shard_count
}

enum Command {
  ReserveSlot { username: StringAtom, handler: StringAtom, max_per_client: u32, reply_tx: Sender<bool> },

  ReleaseSlot { username: StringAtom, handler: StringAtom, reply_tx: Sender<()> },

  ReleaseAllSlots { username: StringAtom, reply_tx: Sender<Arc<[StringAtom]>> },

  GetChannels { username: StringAtom, reply_tx: Sender<Arc<[StringAtom]>> },
}

struct MembershipShard {
  user_channels: HashMap<StringAtom, HashSet<StringAtom>>,
  mailbox: Receiver<Command>,
}

// === impl MembershipShard ===

impl MembershipShard {
  fn new(mailbox: Receiver<Command>) -> Self {
    Self { user_channels: HashMap::new(), mailbox }
  }

  async fn run(mut self) {
    while let Ok(cmd) = self.mailbox.recv().await {
      self.handle(cmd).await;
    }
  }

  async fn handle(&mut self, cmd: Command) {
    match cmd {
      Command::ReserveSlot { username, handler, max_per_client, reply_tx } => {
        let ok = self.reserve_slot(username, handler, max_per_client);
        let _ = reply_tx.send(ok).await;
      },

      Command::ReleaseSlot { username, handler, reply_tx } => {
        self.release_slot(&username, &handler);
        let _ = reply_tx.send(()).await;
      },

      Command::ReleaseAllSlots { username, reply_tx } => {
        let handlers = self.release_all_slots(&username);
        let _ = reply_tx.send(handlers).await;
      },

      Command::GetChannels { username, reply_tx } => {
        let channels = self.get_channels(&username);
        let _ = reply_tx.send(channels).await;
      },
    }
  }

  fn reserve_slot(&mut self, username: StringAtom, handler: StringAtom, max_per_client: u32) -> bool {
    let set = self.user_channels.entry(username).or_default();

    if set.len() >= max_per_client as usize {
      return false;
    }

    set.insert(handler);
    true
  }

  fn release_slot(&mut self, username: &StringAtom, handler: &StringAtom) {
    if let Some(set) = self.user_channels.get_mut(username) {
      set.remove(handler);
      if set.is_empty() {
        self.user_channels.remove(username);
      }
    }
  }

  fn release_all_slots(&mut self, username: &StringAtom) -> Arc<[StringAtom]> {
    self.user_channels.remove(username).map(Arc::from_iter).unwrap_or_default()
  }

  fn get_channels(&self, username: &StringAtom) -> Arc<[StringAtom]> {
    self.user_channels.get(username).map(|set| Arc::from_iter(set.iter().cloned())).unwrap_or_default()
  }
}

/// Per-user membership tracking (sharded by username).
#[derive(Clone)]
pub(crate) struct Membership {
  mailboxes: Arc<[Sender<Command>]>,
  mailbox_capacity: usize,
}

// === impl Membership ===

impl Membership {
  pub(crate) fn new() -> Self {
    Self { mailboxes: Arc::from([]), mailbox_capacity: DEFAULT_MAILBOX_CAPACITY }
  }

  pub(crate) async fn bootstrap(&mut self, core_dispatcher: &CoreDispatcher) -> anyhow::Result<()> {
    let shard_count = core_dispatcher.shard_count();
    let mut mailboxes = Vec::with_capacity(shard_count);

    for shard_id in 0..shard_count {
      let (tx, rx) = async_channel::bounded(self.mailbox_capacity);
      mailboxes.push(tx);

      core_dispatcher
        .dispatch_at_shard(shard_id, move || async move {
          MembershipShard::new(rx).run().await;
        })
        .await?;
    }

    self.mailboxes = Arc::from(mailboxes);

    Ok(())
  }

  pub(crate) fn shutdown(&self) {
    for tx in self.mailboxes.iter() {
      tx.close();
    }
  }

  pub(crate) async fn reserve_slot(&self, username: &StringAtom, handler: &StringAtom, max_per_client: u32) -> bool {
    self.assert_bootstrapped();

    let shard = shard_for_user(username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    let cmd = Command::ReserveSlot { username: username.clone(), handler: handler.clone(), max_per_client, reply_tx };

    if self.mailboxes[shard].send(cmd).await.is_err() {
      return false;
    }

    reply_rx.recv().await.unwrap_or(false)
  }

  pub(crate) async fn release_slot(&self, username: &StringAtom, handler: &StringAtom) {
    self.assert_bootstrapped();

    let shard = shard_for_user(username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    let cmd = Command::ReleaseSlot { username: username.clone(), handler: handler.clone(), reply_tx };

    if self.mailboxes[shard].send(cmd).await.is_err() {
      return;
    }

    let _ = reply_rx.recv().await;
  }

  pub(crate) async fn release_all_slots(&self, username: &StringAtom) -> Arc<[StringAtom]> {
    self.assert_bootstrapped();

    let shard = shard_for_user(username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    let cmd = Command::ReleaseAllSlots { username: username.clone(), reply_tx };

    if self.mailboxes[shard].send(cmd).await.is_err() {
      return Arc::from([]);
    }

    reply_rx.recv().await.unwrap_or_default()
  }

  pub(crate) async fn get_channels(&self, username: &StringAtom) -> Arc<[StringAtom]> {
    self.assert_bootstrapped();

    let shard = shard_for_user(username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    let cmd = Command::GetChannels { username: username.clone(), reply_tx };

    if self.mailboxes[shard].send(cmd).await.is_err() {
      return Arc::from([]);
    }

    reply_rx.recv().await.unwrap_or_default()
  }

  fn assert_bootstrapped(&self) {
    debug_assert!(!self.mailboxes.is_empty(), "Membership::bootstrap() must be called before use");
  }
}
