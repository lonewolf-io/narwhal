// SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_channel::{Receiver, Sender};

use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_protocol::Message;
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use crate::transmitter::Transmitter;

const DEFAULT_MAILBOX_CAPACITY: usize = 16384;

/// Deterministically maps a username to a shard index.
fn shard_for(username: &StringAtom, shard_count: usize) -> usize {
  let mut hasher = DefaultHasher::new();
  username.hash(&mut hasher);
  (hasher.finish() as usize) % shard_count
}

enum Command {
  RouteTo {
    msg: Message,
    payload_opt: Option<PoolBuffer>,
    target: StringAtom,
    excluding_handler: Option<usize>,
  },
  Register {
    username: StringAtom,
    transmitter: Arc<dyn Transmitter>,
    handler: usize,
    exclusive: bool,
    reply_tx: Sender<bool>,
  },
  Unregister {
    username: StringAtom,
    handler: usize,
    reply_tx: Sender<bool>,
  },
  HasConnection {
    username: StringAtom,
    reply_tx: Sender<bool>,
  },
}

struct Entry {
  handler: usize,
  transmitter: Arc<dyn Transmitter>,
}

struct RouterShard {
  connections: HashMap<StringAtom, Vec<Entry>>,
  mailbox: Receiver<Command>,
  total_connections: Arc<AtomicUsize>,
}

// === impl RouterShard ===

impl RouterShard {
  fn new(mailbox: Receiver<Command>, total_connections: Arc<AtomicUsize>) -> Self {
    Self { connections: HashMap::new(), mailbox, total_connections }
  }

  async fn run(mut self) {
    while let Ok(cmd) = self.mailbox.recv().await {
      self.handle(cmd).await;
    }
  }

  async fn handle(&mut self, cmd: Command) {
    match cmd {
      Command::RouteTo { msg, payload_opt, target, excluding_handler } => {
        self.route_to(msg, payload_opt, &target, excluding_handler);
      },
      Command::Register { username, transmitter, handler, exclusive, reply_tx } => {
        let ok = self.register(username, transmitter, handler, exclusive);
        let _ = reply_tx.send(ok).await;
      },
      Command::Unregister { username, handler, reply_tx } => {
        let should_cleanup = self.unregister(&username, handler);
        let _ = reply_tx.send(should_cleanup).await;
      },
      Command::HasConnection { username, reply_tx } => {
        let exists = self.has_connection(&username);
        let _ = reply_tx.send(exists).await;
      },
    }
  }

  fn route_to(
    &self,
    msg: Message,
    payload_opt: Option<PoolBuffer>,
    target: &StringAtom,
    excluding_handler: Option<usize>,
  ) {
    if let Some(entries) = self.connections.get(target) {
      for entry in entries {
        if Some(entry.handler) == excluding_handler {
          continue;
        }
        entry.transmitter.send_message_with_payload(msg.clone(), payload_opt.clone());
      }
    }
  }

  fn register(
    &mut self,
    username: StringAtom,
    transmitter: Arc<dyn Transmitter>,
    handler: usize,
    exclusive: bool,
  ) -> bool {
    let entries = self.connections.entry(username).or_default();
    if exclusive && !entries.is_empty() {
      return false;
    }
    entries.push(Entry { handler, transmitter });
    self.total_connections.fetch_add(1, Ordering::Relaxed);
    true
  }

  fn unregister(&mut self, username: &StringAtom, handler: usize) -> bool {
    let Some(entries) = self.connections.get_mut(username) else {
      return false;
    };
    let before = entries.len();
    entries.retain(|e| e.handler != handler);
    let removed = before - entries.len();
    if removed > 0 {
      self.total_connections.fetch_sub(removed, Ordering::Relaxed);
    }
    if entries.is_empty() {
      self.connections.remove(username);
      true
    } else {
      false
    }
  }

  fn has_connection(&self, username: &StringAtom) -> bool {
    self.connections.get(username).is_some_and(|entries| !entries.is_empty())
  }
}

#[derive(Clone)]
pub struct Router {
  local_domain: StringAtom,
  mailboxes: Arc<[Sender<Command>]>,
  total_connections: Arc<AtomicUsize>,
  mailbox_capacity: usize,
}

// === Router ===

impl Router {
  /// Creates a new `Router`.
  pub fn new(local_domain: StringAtom) -> Self {
    Self::with_mailbox_capacity(local_domain, DEFAULT_MAILBOX_CAPACITY)
  }

  /// Creates a new `Router` with a custom mailbox capacity.
  pub fn with_mailbox_capacity(local_domain: StringAtom, mailbox_capacity: usize) -> Self {
    Self { local_domain, mailboxes: Arc::from([]), total_connections: Arc::new(AtomicUsize::new(0)), mailbox_capacity }
  }

  /// Spawns one shard actor per core on the given dispatcher.
  pub async fn bootstrap(&mut self, core_dispatcher: &CoreDispatcher) -> anyhow::Result<()> {
    let shard_count = core_dispatcher.shard_count();
    let mut mailboxes = Vec::with_capacity(shard_count);

    for shard_id in 0..shard_count {
      let (tx, rx) = async_channel::bounded(self.mailbox_capacity);
      mailboxes.push(tx);

      let tc = self.total_connections.clone();
      core_dispatcher
        .dispatch_at_shard(shard_id, move || async move {
          RouterShard::new(rx, tc).run().await;
        })
        .await?;
    }

    self.mailboxes = Arc::from(mailboxes);
    Ok(())
  }

  /// Shuts down the router by closing all mailbox channels.
  pub fn shutdown(&self) {
    for tx in self.mailboxes.iter() {
      tx.close();
    }
  }

  /// Routes a message to a single target.
  pub async fn route_to(
    &self,
    msg: Message,
    payload_opt: Option<PoolBuffer>,
    target: StringAtom,
    excluding_local_handler: Option<usize>,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&target, self.mailboxes.len());
    let cmd = Command::RouteTo { msg, payload_opt, target, excluding_handler: excluding_local_handler };
    self.mailboxes[shard].send(cmd).await.map_err(|_| anyhow::anyhow!("router shard closed"))?;
    Ok(())
  }

  /// Registers a connection for a specific username.
  /// Returns `true` if successfully registered, `false` if exclusive was requested and connections already exist.
  pub async fn register_connection(
    &self,
    username: StringAtom,
    transmitter: Arc<dyn Transmitter>,
    handler: usize,
    exclusive: bool,
  ) -> bool {
    self.assert_bootstrapped();

    let shard = shard_for(&username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let cmd = Command::Register { username, transmitter, handler, exclusive, reply_tx };
    if self.mailboxes[shard].send(cmd).await.is_err() {
      return false;
    }
    reply_rx.recv().await.unwrap_or(false)
  }

  /// Unregisters a connection for a specific username and handler.
  /// Returns `true` if the last connection for this username was removed.
  pub async fn unregister_connection(&self, username: &StringAtom, handler: usize) -> bool {
    self.assert_bootstrapped();

    let shard = shard_for(username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let cmd = Command::Unregister { username: username.clone(), handler, reply_tx };
    if self.mailboxes[shard].send(cmd).await.is_err() {
      return false;
    }
    reply_rx.recv().await.unwrap_or(false)
  }

  /// Checks if there are any connections registered for a given username.
  pub async fn has_connection(&self, username: &StringAtom) -> bool {
    self.assert_bootstrapped();

    let shard = shard_for(username, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let cmd = Command::HasConnection { username: username.clone(), reply_tx };
    if self.mailboxes[shard].send(cmd).await.is_err() {
      return false;
    }
    reply_rx.recv().await.unwrap_or(false)
  }

  /// Returns the total number of connections across all shards.
  #[allow(clippy::len_without_is_empty)]
  pub fn len(&self) -> usize {
    self.total_connections.load(Ordering::Relaxed)
  }

  /// Returns the local domain of this router.
  pub fn local_domain(&self) -> StringAtom {
    self.local_domain.clone()
  }

  /// Asserts that `bootstrap()` has been called.
  fn assert_bootstrapped(&self) {
    debug_assert!(!self.mailboxes.is_empty(), "Router::bootstrap() must be called before use");
  }
}

impl std::fmt::Debug for Router {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Router").field("local_domain", &self.local_domain).field("total_connections", &self.len()).finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;

  use narwhal_protocol::Message;
  use narwhal_util::pool::PoolBuffer;

  use crate::transmitter::Resource;
  use narwhal_common::runtime;

  /// A mock transmitter that records whether a message was received.
  #[derive(Clone)]
  struct MockTransmitter {
    handler: usize,
    received: Arc<AtomicBool>,
  }

  impl MockTransmitter {
    fn new(handler: usize) -> Self {
      Self { handler, received: Arc::new(AtomicBool::new(false)) }
    }

    fn was_called(&self) -> bool {
      self.received.load(Ordering::SeqCst)
    }
  }

  impl Transmitter for MockTransmitter {
    fn send_message(&self, _message: Message) {
      self.received.store(true, Ordering::SeqCst);
    }

    fn send_message_with_payload(&self, _message: Message, _payload_opt: Option<PoolBuffer>) {
      self.received.store(true, Ordering::SeqCst);
    }

    fn resource(&self) -> Resource {
      Resource { domain: None, handler: self.handler }
    }
  }

  /// Creates a test router with a single shard actor on the current runtime.
  fn create_test_router() -> Router {
    let total_connections = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = async_channel::bounded(64);

    let tc = total_connections.clone();
    runtime::spawn_detached(async move {
      RouterShard::new(rx, tc).run().await;
    });

    Router {
      local_domain: StringAtom::from("localhost"),
      mailboxes: Arc::from([tx]),
      total_connections,
      mailbox_capacity: 64,
    }
  }

  #[compio::test]
  async fn register_and_has_connection() {
    let router = create_test_router();
    let username = StringAtom::from("alice");
    let tx = MockTransmitter::new(1);

    assert!(!router.has_connection(&username).await);
    assert!(router.register_connection(username.clone(), Arc::new(tx), 1, false).await);
    assert!(router.has_connection(&username).await);
  }

  #[compio::test]
  async fn exclusive_registration_fails_when_exists() {
    let router = create_test_router();
    let username = StringAtom::from("bob");

    let tx1 = MockTransmitter::new(1);
    assert!(router.register_connection(username.clone(), Arc::new(tx1), 1, false).await);

    let tx2 = MockTransmitter::new(2);
    assert!(!router.register_connection(username.clone(), Arc::new(tx2), 2, true).await);
  }

  #[compio::test]
  async fn exclusive_registration_succeeds_when_empty() {
    let router = create_test_router();
    let username = StringAtom::from("carol");

    let tx = MockTransmitter::new(1);
    assert!(router.register_connection(username.clone(), Arc::new(tx), 1, true).await);
  }

  #[compio::test]
  async fn unregister_last_connection_returns_true() {
    let router = create_test_router();
    let username = StringAtom::from("dave");

    let tx = MockTransmitter::new(1);
    router.register_connection(username.clone(), Arc::new(tx), 1, false).await;

    assert!(router.unregister_connection(&username, 1).await);
    assert!(!router.has_connection(&username).await);
  }

  #[compio::test]
  async fn unregister_non_last_connection_returns_false() {
    let router = create_test_router();
    let username = StringAtom::from("eve");

    let tx1 = MockTransmitter::new(1);
    let tx2 = MockTransmitter::new(2);
    router.register_connection(username.clone(), Arc::new(tx1), 1, false).await;
    router.register_connection(username.clone(), Arc::new(tx2), 2, false).await;

    assert!(!router.unregister_connection(&username, 1).await);
    assert!(router.has_connection(&username).await);
  }

  #[compio::test]
  async fn route_to_delivers_message() {
    let router = create_test_router();
    let username = StringAtom::from("frank");
    let tx = MockTransmitter::new(1);
    let tx_ref = tx.clone();

    router.register_connection(username.clone(), Arc::new(tx), 1, false).await;

    let msg = Message::Ping(Default::default());
    router.route_to(msg, None, username.clone(), None).await.unwrap();

    // Use has_connection as a barrier — it round-trips through the actor,
    // ensuring the prior RouteTo command has been processed.
    let _ = router.has_connection(&username).await;

    assert!(tx_ref.was_called());
  }

  #[compio::test]
  async fn route_to_excludes_handler() {
    let router = create_test_router();
    let username = StringAtom::from("grace");

    let tx1 = MockTransmitter::new(1);
    let tx2 = MockTransmitter::new(2);
    let tx1_ref = tx1.clone();
    let tx2_ref = tx2.clone();

    router.register_connection(username.clone(), Arc::new(tx1), 1, false).await;
    router.register_connection(username.clone(), Arc::new(tx2), 2, false).await;

    let msg = Message::Ping(Default::default());
    router.route_to(msg, None, username.clone(), Some(1)).await.unwrap();

    // Use has_connection as a barrier.
    let _ = router.has_connection(&username).await;

    assert!(!tx1_ref.was_called());
    assert!(tx2_ref.was_called());
  }

  #[compio::test]
  async fn len_tracks_connections() {
    let router = create_test_router();

    assert_eq!(router.len(), 0);

    let tx1 = MockTransmitter::new(1);
    router.register_connection(StringAtom::from("user1"), Arc::new(tx1), 1, false).await;
    assert_eq!(router.len(), 1);

    let tx2 = MockTransmitter::new(2);
    router.register_connection(StringAtom::from("user2"), Arc::new(tx2), 2, false).await;
    assert_eq!(router.len(), 2);

    router.unregister_connection(&StringAtom::from("user1"), 1).await;
    assert_eq!(router.len(), 1);
  }

  #[test]
  fn shard_assignment_is_deterministic() {
    let username = StringAtom::from("test_user");
    let shard_count = 8;

    let s1 = shard_for(&username, shard_count);
    let s2 = shard_for(&username, shard_count);
    assert_eq!(s1, s2);
  }
}
