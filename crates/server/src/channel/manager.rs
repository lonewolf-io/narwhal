// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_channel::{Receiver, Sender};

use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_protocol::ErrorReason::{
  BadRequest, ChannelIsFull, ChannelNotFound, Forbidden, NotAllowed, NotImplemented, PersistenceNotEnabled,
  PolicyViolation, ResourceLimitReached, UserInChannel, UserNotInChannel, UserNotRegistered,
};
use narwhal_protocol::{
  AclAction, AclType, BroadcastAckParameters, ChannelAclParameters, ChannelConfigurationParameters,
  ChannelSeqAckParameters, DeleteChannelAckParameters, HistoryAckParameters, JoinChannelAckParameters,
  LeaveChannelAckParameters, ListChannelsAckParameters, ListMembersAckParameters, Message, MessageParameters, QoS,
  SetChannelAclAckParameters, SetChannelConfigurationAckParameters,
};
use narwhal_protocol::{ChannelId, Nid};
use narwhal_protocol::{Event, EventKind};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use tracing::warn;

use crate::notifier::Notifier;
use crate::router::GlobalRouter;
use crate::transmitter::{Resource, Transmitter};

use super::membership::Membership;
use super::store::{ChannelStore, MessageLog, MessageLogFactory, PersistedChannel};

const DEFAULT_MAILBOX_CAPACITY: usize = 16384;

const MAX_CHANNELS_PAGE_SIZE: u32 = 50;

const MAX_MEMBERS_PAGE_SIZE: u32 = 100;

/// Deterministically maps a channel handler to a shard index.
fn shard_for(handler: &StringAtom, shard_count: usize) -> usize {
  let mut hasher = DefaultHasher::new();
  handler.hash(&mut hasher);
  (hasher.finish() as usize) % shard_count
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabel {
  result: &'static str,
}

const SUCCESS: ResultLabel = ResultLabel { result: "success" };
const FAILURE: ResultLabel = ResultLabel { result: "failure" };

/// Metric handles for `ChannelManager`.
#[derive(Clone)]
struct ChannelManagerMetrics {
  channels_active: Gauge,
  channel_joins: Family<ResultLabel, Counter>,
  channel_leaves: Counter,
  channels_deleted: Counter,
  store_saves: Family<ResultLabel, Counter>,
  store_save_duration_seconds: Histogram,
  store_deletes: Family<ResultLabel, Counter>,
  message_log_flushes: Family<ResultLabel, Counter>,
  message_log_flush_duration_seconds: Histogram,
}

impl std::fmt::Debug for ChannelManagerMetrics {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChannelManagerMetrics").finish_non_exhaustive()
  }
}

impl ChannelManagerMetrics {
  fn register(registry: &mut Registry) -> Self {
    let channels_active = Gauge::default();
    registry.register("channels_active", "Number of currently active channels", channels_active.clone());
    let channel_joins = Family::default();
    registry.register("channel_joins", "Channel join results", channel_joins.clone());
    let channel_leaves = Counter::default();
    registry.register("channel_leaves", "Total channel leave operations", channel_leaves.clone());
    let channels_deleted = Counter::default();
    registry.register("channels_deleted", "Total channels deleted", channels_deleted.clone());

    let store_saves = Family::default();
    registry.register("store_saves", "Channel store save operations", store_saves.clone());
    let store_save_duration_seconds =
      Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(0.0001, 2.0, 16));
    registry.register(
      "store_save_duration_seconds",
      "Duration of channel store save operations in seconds",
      store_save_duration_seconds.clone(),
    );
    let store_deletes = Family::default();
    registry.register("store_deletes", "Channel store delete operations", store_deletes.clone());
    let message_log_flushes = Family::default();
    registry.register("message_log_flushes", "Message log flush operations", message_log_flushes.clone());
    let message_log_flush_duration_seconds =
      Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(0.0001, 2.0, 16));
    registry.register(
      "message_log_flush_duration_seconds",
      "Duration of message log flush operations in seconds",
      message_log_flush_duration_seconds.clone(),
    );

    Self {
      channels_active,
      channel_joins,
      channel_leaves,
      channels_deleted,
      store_saves,
      store_save_duration_seconds,
      store_deletes,
      message_log_flushes,
      message_log_flush_duration_seconds,
    }
  }

  /// Records the outcome of a message log flush: observes duration and increments the
  /// success/failure counter.
  fn record_flush(&self, start: Instant, result: &anyhow::Result<()>) {
    self.message_log_flush_duration_seconds.observe(start.elapsed().as_secs_f64());
    match result {
      Ok(()) => self.message_log_flushes.get_or_create(&SUCCESS).inc(),
      Err(_) => self.message_log_flushes.get_or_create(&FAILURE).inc(),
    };
  }
}

enum Command {
  JoinChannel {
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<bool>>,
  },
  LeaveChannel {
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Option<Arc<dyn Transmitter>>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  LeaveChannels {
    nid: Nid,
    handlers: Vec<StringAtom>,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  DeleteChannel {
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  BroadcastPayload {
    payload: PoolBuffer,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    qos: Option<u8>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  GetChannelAcl {
    channel_id: ChannelId,
    nid: Nid,
    acl_type: AclType,
    page: Option<u32>,
    page_size: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  SetChannelAcl {
    channel_id: ChannelId,
    nid: Nid,
    nids: Vec<Nid>,
    acl_type: AclType,
    acl_action: AclAction,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  GetChannelConfiguration {
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  SetChannelConfiguration {
    config: ChannelConfig,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  FilterOwnedChannels {
    nid: Nid,
    handlers: Vec<StringAtom>,
    reply_tx: Sender<Vec<StringAtom>>,
  },
  ListMembers {
    channel_id: ChannelId,
    nid: Nid,
    page: Option<u32>,
    count: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  History {
    channel_id: ChannelId,
    nid: Nid,
    history_id: StringAtom,
    from_seq: u64,
    limit: u32,
    direction: Option<StringAtom>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
  ChannelSeq {
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
    reply_tx: Sender<anyhow::Result<()>>,
  },
}

/// A channel.
struct Channel<ML: MessageLog> {
  handler: StringAtom,
  owner: Option<Nid>,
  config: ChannelConfig,
  acl: ChannelAcl,
  members: Rc<[Nid]>,
  allowed_targets: Rc<[Nid]>,
  notifier: Notifier,
  seq: u64,
  message_log: Rc<ML>,
  flush_cancel_tx: Option<Sender<()>>,
  /// The storage hash for this channel. `Some` for persistent channels, `None` for transient.
  store_hash: Option<StringAtom>,
}

// === impl Channel ===

impl<ML: MessageLog> Channel<ML> {
  fn new(handler: StringAtom, config: ChannelConfig, notifier: Notifier, message_log: ML) -> Self {
    Self {
      handler,
      owner: None,
      config,
      acl: ChannelAcl::default(),
      members: Rc::from([]),
      allowed_targets: Rc::from([]),
      notifier,
      seq: 1,
      message_log: Rc::new(message_log),
      flush_cancel_tx: None,
      store_hash: None,
    }
  }

  fn is_empty(&self) -> bool {
    self.members.is_empty()
  }

  fn is_persistent(&self) -> bool {
    self.config.persist == Some(true)
  }

  fn is_owner(&self, nid: &Nid) -> bool {
    self.owner == Some(nid.clone())
  }

  fn is_member(&self, nid: &Nid) -> bool {
    self.members.binary_search(nid).is_ok()
  }

  fn member_count(&self) -> usize {
    self.members.len()
  }

  fn remove_member(&mut self, nid: &Nid) -> bool {
    if self.owner.as_ref() == Some(nid) {
      self.owner = None;
    }
    match self.members.binary_search(nid) {
      Ok(pos) => {
        let mut v: Vec<Nid> = self.members.iter().cloned().collect();
        v.remove(pos);
        self.members = Rc::from(v);
        self.update_allowed_targets();
        true
      },
      Err(_) => false,
    }
  }

  fn set_acl(&mut self, acl: Acl, acl_type: AclType) {
    match acl_type {
      AclType::Join => self.acl.join_acl = acl,
      AclType::Publish => self.acl.publish_acl = acl,
      AclType::Read => self.acl.read_acl = acl,
    }
    self.update_allowed_targets();
  }

  fn update_allowed_targets(&mut self) {
    let acl = &self.acl;
    if self.members.iter().all(|m| acl.is_read_allowed(m)) {
      self.allowed_targets = self.members.clone();
      return;
    }
    self.allowed_targets = self.members.iter().filter(|m| acl.is_read_allowed(m)).cloned().collect::<Vec<_>>().into();
  }

  async fn notify_member_joined(
    &self,
    nid: &Nid,
    excluding_resource: Option<Resource>,
    as_owner: bool,
    local_domain: StringAtom,
  ) -> anyhow::Result<()> {
    let channel_id = ChannelId::new_unchecked(self.handler.clone(), local_domain);
    let event =
      Event::new(EventKind::MemberJoined).with_channel(channel_id.into()).with_nid(nid.into()).with_owner(as_owner);
    self.notifier.notify(event, self.members.iter(), excluding_resource).await
  }

  async fn notify_member_left(
    &self,
    nid: &Nid,
    excluding_resource: Option<Resource>,
    as_owner: bool,
    local_domain: StringAtom,
  ) -> anyhow::Result<()> {
    let channel_id = ChannelId::new_unchecked(self.handler.clone(), local_domain);
    let event =
      Event::new(EventKind::MemberLeft).with_channel(channel_id.into()).with_nid(nid.into()).with_owner(as_owner);
    self.notifier.notify(event, self.members.iter(), excluding_resource).await
  }

  async fn notify_channel_deleted(
    &self,
    excluding_resource: Option<Resource>,
    local_domain: StringAtom,
  ) -> anyhow::Result<()> {
    let channel_id = ChannelId::new_unchecked(self.handler.clone(), local_domain);
    let event = Event::new(EventKind::ChannelDeleted).with_channel(channel_id.into());
    self.notifier.notify(event, self.members.iter(), excluding_resource).await
  }

  fn next_seq(&mut self) -> u64 {
    let seq = self.seq;
    self.seq += 1;
    seq
  }

  fn ensure_flush_task(&mut self, interval_ms: u32, metrics: &ChannelManagerMetrics) {
    if self.flush_cancel_tx.is_some() {
      return;
    }
    let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
    let message_log = self.message_log.clone();
    let handler = self.handler.clone();
    let interval = std::time::Duration::from_millis(interval_ms as u64);
    let metrics = metrics.clone();

    narwhal_common::runtime::spawn_detached(async move {
      use futures::FutureExt;

      loop {
        futures::select! {
          _ = narwhal_common::runtime::sleep(interval).fuse() => {},
          _ = cancel_rx.recv().fuse() => break,
        }
        let start = Instant::now();
        let result = message_log.flush().await;
        metrics.record_flush(start, &result);
        if let Err(e) = result {
          warn!(channel = %handler, error = %e, "periodic message log flush failed");
        }
      }
    });

    self.flush_cancel_tx = Some(cancel_tx);
  }

  fn cancel_flush_task(&mut self) {
    if let Some(tx) = self.flush_cancel_tx.take() {
      // Best-effort send; if the receiver is already gone, that's fine.
      let _ = tx.try_send(());
    }
  }

  fn to_persisted(&self) -> PersistedChannel {
    PersistedChannel {
      handler: self.handler.clone(),
      owner: self.owner.clone(),
      config: self.config.clone(),
      acl: self.acl.clone(),
      members: self.members.clone(),
    }
  }
}

struct ChannelShard<CS: ChannelStore, MLF: MessageLogFactory> {
  channels: HashMap<StringAtom, Channel<MLF::Log>>,
  #[allow(dead_code)]
  store: CS,
  message_log_factory: MLF,
  membership: Membership,
  mailbox: Receiver<Command>,
  router: GlobalRouter,
  notifier: Notifier,
  local_domain: StringAtom,
  total_channels: Arc<AtomicUsize>,
  limits: ChannelManagerLimits,
  metrics: ChannelManagerMetrics,
  auth_enabled: bool,
}

// === impl ChannelShard ===

impl<CS: ChannelStore, MLF: MessageLogFactory> ChannelShard<CS, MLF> {
  async fn restore_and_run(mut self, hashes: Vec<StringAtom>) {
    self.restore(hashes).await;
    while let Ok(cmd) = self.mailbox.recv().await {
      self.handle(cmd).await;
    }

    // Shutdown: cancel all flush tasks and perform final flush on each channel.
    for (handler, channel) in self.channels.iter_mut() {
      channel.cancel_flush_task();
      if channel.config.persist == Some(true) {
        let start = Instant::now();
        let result = channel.message_log.flush().await;
        self.metrics.record_flush(start, &result);
        if let Err(e) = result {
          warn!(channel = %handler, error = %e, "final flush on shutdown failed");
        }
      }
    }
  }

  async fn restore(&mut self, hashes: Vec<StringAtom>) {
    for hash in &hashes {
      let persisted = match self.store.load_channel(hash).await {
        Ok(p) => p,
        Err(e) => {
          warn!(hash = %hash, error = %e, "skipping channel restore: failed to load persisted channel");
          continue;
        },
      };

      let handler = persisted.handler.clone();
      let message_log = self.message_log_factory.create(&handler);

      let mut channel = Channel::new(handler.clone(), persisted.config, self.notifier.clone(), message_log);
      channel.owner = persisted.owner;
      channel.acl = persisted.acl;
      channel.members = persisted.members;
      channel.store_hash = Some(hash.clone());
      channel.seq = match channel.message_log.last_seq().await {
        Ok(seq) => seq + 1,
        Err(e) => {
          warn!(channel = %handler, error = %e, "skipping channel restore: failed to read last seq from message log");
          continue;
        },
      };
      channel.update_allowed_targets();

      // Use u32::MAX to bypass the per-client limit: persisted membership is authoritative
      // and limit changes should not retroactively evict members from their channels.
      for member in channel.members.iter() {
        self.membership.reserve_slot(&member.username, &handler, u32::MAX).await;
      }

      self.channels.insert(handler, channel);
      self.total_channels.fetch_add(1, Ordering::SeqCst);
      self.metrics.channels_active.inc();
    }
  }

  async fn handle(&mut self, cmd: Command) {
    match cmd {
      Command::JoinChannel { channel_id, nid, on_behalf_nid, transmitter, correlation_id, reply_tx } => {
        let result = self.join_channel(channel_id, nid, on_behalf_nid, transmitter, correlation_id).await;
        let _ = reply_tx.send(result).await;
      },
      Command::LeaveChannel { channel_id, nid, on_behalf_nid, transmitter, correlation_id, reply_tx } => {
        let result = self.leave_channel(channel_id, nid, on_behalf_nid, transmitter, correlation_id).await;
        let _ = reply_tx.send(result).await;
      },
      Command::LeaveChannels { nid, handlers, reply_tx } => {
        let result = self.leave_channels(nid, handlers).await;
        let _ = reply_tx.send(result).await;
      },
      Command::DeleteChannel { channel_id, nid, transmitter, correlation_id, reply_tx } => {
        let result = self.delete_channel(channel_id, nid, transmitter, correlation_id).await;
        let _ = reply_tx.send(result).await;
      },
      Command::BroadcastPayload { payload, channel_id, nid, transmitter, qos, correlation_id, reply_tx } => {
        let result = self.broadcast_payload(payload, channel_id, nid, transmitter, qos, correlation_id).await;
        let _ = reply_tx.send(result).await;
      },
      Command::GetChannelAcl { channel_id, nid, acl_type, page, page_size, transmitter, correlation_id, reply_tx } => {
        let result = self.get_channel_acl(channel_id, nid, acl_type, page, page_size, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
      Command::SetChannelAcl { channel_id, nid, nids, acl_type, acl_action, transmitter, correlation_id, reply_tx } => {
        let result =
          self.set_channel_acl(channel_id, nid, nids, acl_type, acl_action, transmitter, correlation_id).await;
        let _ = reply_tx.send(result).await;
      },
      Command::GetChannelConfiguration { channel_id, nid, transmitter, correlation_id, reply_tx } => {
        let result = self.get_channel_configuration(channel_id, nid, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
      Command::SetChannelConfiguration { config, channel_id, nid, transmitter, correlation_id, reply_tx } => {
        let result = self.set_channel_configuration(config, channel_id, nid, transmitter, correlation_id).await;
        let _ = reply_tx.send(result).await;
      },
      Command::FilterOwnedChannels { nid, handlers, reply_tx } => {
        let result = self.filter_owned_channels(&nid, &handlers);
        let _ = reply_tx.send(result).await;
      },
      Command::ListMembers { channel_id, nid, page, count, transmitter, correlation_id, reply_tx } => {
        let result = self.list_members(channel_id, nid, page, count, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
      Command::History {
        channel_id,
        nid,
        history_id,
        from_seq,
        limit,
        direction,
        transmitter,
        correlation_id,
        reply_tx,
      } => {
        let result = self.history(channel_id, nid, history_id, from_seq, limit, direction, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
      Command::ChannelSeq { channel_id, nid, transmitter, correlation_id, reply_tx } => {
        let result = self.channel_seq(channel_id, nid, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
    }
  }

  async fn join_channel(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<bool> {
    if channel_id.domain != self.local_domain {
      self.metrics.channel_joins.get_or_create(&FAILURE).inc();
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }
    let handler = channel_id.handler.clone();
    let as_owner = !self.channels.contains_key(&handler);

    // Create the channel if it doesn't exist.
    if as_owner {
      if self.total_channels.load(Ordering::SeqCst) >= self.limits.max_channels as usize {
        self.metrics.channel_joins.get_or_create(&FAILURE).inc();
        return Err(
          narwhal_protocol::Error::new(ResourceLimitReached)
            .with_id(correlation_id)
            .with_detail("maximum channels reached")
            .into(),
        );
      }

      let config = ChannelConfig {
        max_clients: Some(self.limits.max_clients_per_channel),
        max_payload_size: Some(self.limits.max_payload_size),
        max_persist_messages: Some(0),
        persist: Some(false),
        message_flush_interval: Some(0),
      };
      let message_log = self.message_log_factory.create(&handler);
      self.channels.insert(handler.clone(), Channel::new(handler.clone(), config, self.notifier.clone(), message_log));
      self.total_channels.fetch_add(1, Ordering::SeqCst);
    }

    let new_member_nid = match on_behalf_nid {
      Some(behalf_nid) => {
        let channel = self.channels.get(&handler).unwrap();
        if !channel.is_owner(&nid) {
          if as_owner {
            self.channels.remove(&handler);
            self.total_channels.fetch_sub(1, Ordering::SeqCst);
          }
          self.metrics.channel_joins.get_or_create(&FAILURE).inc();
          return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
        }
        if !self.router.c2s_router().has_connection(&behalf_nid.username).await {
          if as_owner {
            self.channels.remove(&handler);
            self.total_channels.fetch_sub(1, Ordering::SeqCst);
          }
          self.metrics.channel_joins.get_or_create(&FAILURE).inc();
          return Err(narwhal_protocol::Error::new(UserNotRegistered).with_id(correlation_id).into());
        }
        behalf_nid
      },
      None => nid.clone(),
    };

    let channel = self.channels.get(&handler).unwrap();

    // Validate before reserving a membership slot.
    if !channel.acl.is_join_allowed(&new_member_nid) {
      if as_owner {
        self.channels.remove(&handler);
        self.total_channels.fetch_sub(1, Ordering::SeqCst);
      }
      self.metrics.channel_joins.get_or_create(&FAILURE).inc();
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    if channel.is_member(&new_member_nid) {
      if as_owner {
        self.channels.remove(&handler);
        self.total_channels.fetch_sub(1, Ordering::SeqCst);
      }
      self.metrics.channel_joins.get_or_create(&FAILURE).inc();
      return Err(narwhal_protocol::Error::new(UserInChannel).with_id(correlation_id).into());
    }

    if channel.member_count() >= channel.config.max_clients.unwrap_or(0) as usize {
      if as_owner {
        self.channels.remove(&handler);
        self.total_channels.fetch_sub(1, Ordering::SeqCst);
      }
      self.metrics.channel_joins.get_or_create(&FAILURE).inc();
      return Err(narwhal_protocol::Error::new(ChannelIsFull).with_id(correlation_id).into());
    }

    // Reserve the membership slot before persisting or mutating in-memory state.
    if !self.membership.reserve_slot(&new_member_nid.username, &handler, self.limits.max_channels_per_client).await {
      if as_owner {
        self.channels.remove(&handler);
        self.total_channels.fetch_sub(1, Ordering::SeqCst);
      }
      self.metrics.channel_joins.get_or_create(&FAILURE).inc();
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("subscription limit reached")
          .into(),
      );
    }

    // Persist the projected membership before any in-memory changes.
    // Build new_members once and share the Rc between the persist and in-memory paths.
    let (new_members, store_hash) = {
      let channel = self.channels.get(&handler).unwrap();
      let pos = channel.members.partition_point(|m| m < &new_member_nid);
      let mut v: Vec<Nid> = channel.members.iter().cloned().collect();
      v.insert(pos, new_member_nid.clone());
      let new_members: Rc<[Nid]> = Rc::from(v);

      let store_hash = if channel.config.persist == Some(true) {
        let mut projected = channel.to_persisted();
        if projected.owner.is_none() {
          projected.owner = Some(new_member_nid.clone());
        }
        projected.members = new_members.clone();
        let start = Instant::now();
        match self.store.save_channel(&projected).await {
          Ok(hash) => {
            self.metrics.store_save_duration_seconds.observe(start.elapsed().as_secs_f64());
            self.metrics.store_saves.get_or_create(&SUCCESS).inc();
            Some(hash)
          },
          Err(e) => {
            self.metrics.store_save_duration_seconds.observe(start.elapsed().as_secs_f64());
            self.metrics.store_saves.get_or_create(&FAILURE).inc();
            self.membership.release_slot(&new_member_nid.username, &handler).await;
            if as_owner {
              self.channels.remove(&handler);
              self.total_channels.fetch_sub(1, Ordering::SeqCst);
            }
            self.metrics.channel_joins.get_or_create(&FAILURE).inc();
            return Err(e);
          },
        }
      } else {
        None
      };

      (new_members, store_hash)
    };

    let channel = self.channels.get_mut(&handler).unwrap();
    if let Some(hash) = store_hash {
      channel.store_hash = Some(hash);
    }
    if channel.owner.is_none() {
      channel.owner = Some(new_member_nid.clone());
    }
    channel.members = new_members;
    channel.update_allowed_targets();
    if let Err(e) = channel
      .notify_member_joined(&new_member_nid, Some(transmitter.resource()), as_owner, self.local_domain.clone())
      .await
    {
      warn!(channel = %handler, error = %e, "failed to notify member joined");
    }

    transmitter.send_message(Message::JoinChannelAck(JoinChannelAckParameters {
      id: correlation_id,
      channel: channel_id.into(),
    }));

    self.metrics.channel_joins.get_or_create(&SUCCESS).inc();
    if as_owner {
      self.metrics.channels_active.inc();
    }

    Ok(as_owner)
  }

  async fn leave_channel(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Option<Arc<dyn Transmitter>>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    if !self.channels.contains_key(&channel_id.handler) {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    }

    let left_member_nid = match on_behalf_nid {
      Some(behalf_nid) => {
        let channel = self.channels.get(&channel_id.handler).unwrap();
        if !channel.is_owner(&nid) {
          return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
        }
        behalf_nid
      },
      None => nid.clone(),
    };

    {
      let channel = self.channels.get(&channel_id.handler).unwrap();
      if !channel.is_member(&left_member_nid) {
        return Err(narwhal_protocol::Error::new(UserNotInChannel).with_id(correlation_id).into());
      }
    }

    let excluding_resource = transmitter.as_ref().map(|t| t.resource());
    self.do_leave(&channel_id.handler.clone(), &left_member_nid, excluding_resource).await?;

    // Release the membership slot.
    self.membership.release_slot(&left_member_nid.username, &channel_id.handler).await;

    if let Some(tx) = transmitter {
      tx.send_message(Message::LeaveChannelAck(LeaveChannelAckParameters { id: correlation_id }));
    }

    self.metrics.channel_leaves.inc();

    Ok(())
  }

  async fn leave_channels(&mut self, nid: Nid, handlers: Vec<StringAtom>) -> anyhow::Result<()> {
    let mut removed = 0u64;

    for handler in &handlers {
      // Skip persistent channels when auth is enabled, user stays until explicit leave.
      let is_persistent = self.channels.get(handler).is_some_and(|c| c.config.persist == Some(true));
      if self.auth_enabled && is_persistent {
        continue;
      }

      match self.do_leave(handler, &nid, None).await {
        Ok(true) => {
          self.membership.release_slot(&nid.username, handler).await;
          removed += 1;
        },
        Ok(false) => {
          self.membership.release_slot(&nid.username, handler).await;
        },
        Err(e) => {
          warn!(channel = %handler, nid = %nid, error = %e, "failed to leave channel during batch disconnect");
        },
      }
    }

    self.metrics.channel_leaves.inc_by(removed);

    Ok(())
  }

  async fn delete_channel(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    channel.notify_channel_deleted(Some(transmitter.resource()), self.local_domain.clone()).await?;

    // Collect member usernames to release membership slots.
    let member_usernames: Vec<StringAtom> =
      self.channels.get(&channel_id.handler).unwrap().members.iter().map(|m| m.username.clone()).collect();

    for username in &member_usernames {
      self.membership.release_slot(username, &channel_id.handler).await;
    }

    // Cancel flush task and perform final flush before cleanup.
    let is_persistent = self.channels.get(&channel_id.handler).is_some_and(|c| c.config.persist == Some(true));
    if let Some(channel) = self.channels.get_mut(&channel_id.handler) {
      channel.cancel_flush_task();
      if is_persistent {
        let start = Instant::now();
        let result = channel.message_log.flush().await;
        self.metrics.record_flush(start, &result);
        if let Err(e) = result {
          warn!(channel = %channel_id.handler, error = %e, "final flush on delete failed");
        }
      }
    }

    // Clean up persistent storage. Best effort, must not block channel removal or leak slots.
    if is_persistent {
      self.delete_persistent_storage(&channel_id.handler).await;
    }

    self.channels.remove(&channel_id.handler);

    transmitter.send_message(Message::DeleteChannelAck(DeleteChannelAckParameters { id: correlation_id }));

    self.total_channels.fetch_sub(1, Ordering::SeqCst);
    self.metrics.channels_deleted.inc();
    self.metrics.channels_active.dec();

    Ok(())
  }

  async fn broadcast_payload(
    &mut self,
    payload: PoolBuffer,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    qos: Option<u8>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get_mut(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    if !channel.acl.is_publish_allowed(&nid) {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    let max_payload_size = channel.config.max_payload_size.unwrap_or(0);
    let is_persistent = channel.config.persist == Some(true);
    let max_persist_messages = channel.config.max_persist_messages.unwrap_or(0);
    let allowed_targets = channel.allowed_targets.clone();
    let seq = channel.next_seq();
    let payload_length = payload.as_slice().len() as u32;

    if payload_length > max_payload_size {
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("payload size exceeds channel limit")
          .into(),
      );
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let qos = qos.map(QoS::try_from).transpose()?.unwrap_or(QoS::default());

    if qos == QoS::AckOnReceived {
      transmitter.send_message(Message::BroadcastAck(BroadcastAckParameters { id: correlation_id, seq }));
    }

    let msg = Message::Message(MessageParameters {
      from: (&nid).into(),
      channel: (&channel_id).into(),
      length: payload_length,
      seq,
      timestamp,
      history_id: None,
    });

    if is_persistent {
      channel.message_log.append(&msg, &payload, max_persist_messages).await?;

      let flush_interval = channel.config.message_flush_interval.unwrap_or(0);
      if flush_interval == 0 {
        let start = Instant::now();
        let result = channel.message_log.flush().await;
        self.metrics.record_flush(start, &result);
        result?;
      } else {
        channel.ensure_flush_task(flush_interval, &self.metrics);
      }
    }

    self.router.route_to_many(msg, Some(payload), allowed_targets.iter(), Some(transmitter.resource())).await?;

    if qos == QoS::AckOnDelivered {
      transmitter.send_message(Message::BroadcastAck(BroadcastAckParameters { id: correlation_id, seq }));
    }

    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  fn get_channel_acl(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    acl_type: AclType,
    page: Option<u32>,
    page_size: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    let acl = match acl_type {
      AclType::Join => channel.acl.join_acl.clone(),
      AclType::Publish => channel.acl.publish_acl.clone(),
      AclType::Read => channel.acl.read_acl.clone(),
    };

    let all_nids: Vec<StringAtom> = acl.allow_list().into_iter().map(|z| z.into()).collect();
    let total_count = all_nids.len() as u32;

    let (nids, response_page, response_page_size, response_total_count) =
      if let (Some(page), Some(page_size)) = (page, page_size) {
        let start = ((page - 1) * page_size) as usize;
        let end = (start + page_size as usize).min(all_nids.len());
        let paginated_nids = if start < all_nids.len() { all_nids[start..end].to_vec() } else { Vec::new() };
        (paginated_nids, Some(page), Some(page_size), Some(total_count))
      } else {
        (all_nids, None, None, None)
      };

    transmitter.send_message(Message::ChannelAcl(ChannelAclParameters {
      id: correlation_id,
      channel: channel_id.into(),
      r#type: acl_type.as_str().into(),
      nids,
      page: response_page,
      page_size: response_page_size,
      total_count: response_total_count,
    }));

    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  async fn set_channel_acl(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    nids: Vec<Nid>,
    acl_type: AclType,
    acl_action: AclAction,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get_mut(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    let mut new_acl = match acl_type {
      AclType::Join => channel.acl.join_acl.clone(),
      AclType::Publish => channel.acl.publish_acl.clone(),
      AclType::Read => channel.acl.read_acl.clone(),
    };
    new_acl.update(nids, acl_action);

    if new_acl.total_entries() > channel.config.max_clients.unwrap_or(0) as usize {
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("ACL allow list exceeds max entries")
          .into(),
      );
    }

    if channel.config.persist == Some(true) {
      let mut projected = channel.to_persisted();
      match acl_type {
        AclType::Join => projected.acl.join_acl = new_acl.clone(),
        AclType::Publish => projected.acl.publish_acl = new_acl.clone(),
        AclType::Read => projected.acl.read_acl = new_acl.clone(),
      }
      let start = Instant::now();
      let result = self.store.save_channel(&projected).await;
      self.metrics.store_save_duration_seconds.observe(start.elapsed().as_secs_f64());
      match result {
        Ok(hash) => {
          self.metrics.store_saves.get_or_create(&SUCCESS).inc();
          channel.store_hash = Some(hash);
        },
        Err(e) => {
          self.metrics.store_saves.get_or_create(&FAILURE).inc();
          return Err(e);
        },
      }
    }

    channel.set_acl(new_acl, acl_type);

    transmitter.send_message(Message::SetChannelAclAck(SetChannelAclAckParameters { id: correlation_id }));

    Ok(())
  }

  fn get_channel_configuration(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let Some(channel) = self.channels.get(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    let config = channel.config.clone();

    transmitter.send_message(Message::ChannelConfiguration(ChannelConfigurationParameters {
      id: correlation_id,
      channel: channel_id.into(),
      max_clients: config.max_clients.unwrap_or(0),
      max_payload_size: config.max_payload_size.unwrap_or(0),
      max_persist_messages: config.max_persist_messages.unwrap_or(0),
      persist: config.persist.unwrap_or(false),
      message_flush_interval: config.message_flush_interval.unwrap_or(0),
    }));

    Ok(())
  }

  async fn set_channel_configuration(
    &mut self,
    config: ChannelConfig,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    if let Some(v) = config.max_clients
      && v > self.limits.max_clients_per_channel
    {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_clients exceeds server established limit")
          .into(),
      );
    }

    if let Some(v) = config.max_payload_size
      && v > self.limits.max_payload_size
    {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_payload_size exceeds server established limit")
          .into(),
      );
    }

    if let Some(v) = config.max_persist_messages
      && v > self.limits.max_persist_messages
    {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_persist_messages exceeds server established limit")
          .into(),
      );
    }

    if let Some(v) = config.message_flush_interval
      && v > self.limits.max_message_flush_interval
    {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("message_flush_interval exceeds server established limit")
          .into(),
      );
    }
    let Some(channel) = self.channels.get_mut(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    let was_persistent = channel.config.persist == Some(true);
    let flush_interval_changed =
      config.message_flush_interval.is_some() && config.message_flush_interval != channel.config.message_flush_interval;
    let new_config = channel.config.merge(&config);
    let is_persistent = new_config.persist == Some(true);

    if is_persistent {
      let mut projected = channel.to_persisted();
      projected.config = new_config.clone();
      let start = Instant::now();
      let result = self.store.save_channel(&projected).await;
      self.metrics.store_save_duration_seconds.observe(start.elapsed().as_secs_f64());
      match result {
        Ok(hash) => {
          self.metrics.store_saves.get_or_create(&SUCCESS).inc();
          channel.store_hash = Some(hash);
        },
        Err(e) => {
          self.metrics.store_saves.get_or_create(&FAILURE).inc();
          return Err(e);
        },
      }
    }

    channel.config = new_config;

    // Cancel the flush task if persist was toggled off or flush interval changed.
    if (!is_persistent && was_persistent) || flush_interval_changed {
      channel.cancel_flush_task();

      // When the flush interval changed but persistence is still enabled, flush any buffered
      // messages and restart the periodic task immediately so they are not left pending
      // indefinitely waiting for the next broadcast.
      if flush_interval_changed && is_persistent {
        let start = Instant::now();
        let result = channel.message_log.flush().await;
        self.metrics.record_flush(start, &result);
        if let Err(e) = result {
          warn!(channel = %channel_id.handler, error = %e, "flush on interval change failed");
        }
        if let Some(interval) = channel.config.message_flush_interval
          && interval > 0
        {
          channel.ensure_flush_task(interval, &self.metrics);
        }
      }
    }

    if !is_persistent && was_persistent {
      self.delete_persistent_storage(&channel_id.handler).await;
      let channel = self.channels.get_mut(&channel_id.handler).unwrap();
      channel.store_hash = None;
    }

    transmitter
      .send_message(Message::SetChannelConfigurationAck(SetChannelConfigurationAckParameters { id: correlation_id }));

    Ok(())
  }

  fn filter_owned_channels(&self, nid: &Nid, handlers: &[StringAtom]) -> Vec<StringAtom> {
    handlers
      .iter()
      .filter_map(|handler| {
        self.channels.get(handler).and_then(|channel| {
          if channel.is_owner(nid) {
            Some(ChannelId::new_unchecked(handler.clone(), self.local_domain.clone()).into())
          } else {
            None
          }
        })
      })
      .collect()
  }

  fn list_members(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    page: Option<u32>,
    count: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(UserNotInChannel).with_id(correlation_id).into());
    }

    let member_list: Vec<StringAtom> = channel.members.iter().map(|m| m.into()).collect();

    let page = page.unwrap_or(1);
    let page_size = count.unwrap_or(20).min(MAX_MEMBERS_PAGE_SIZE);
    let start = ((page - 1) * page_size) as usize;
    let end = (page * page_size) as usize;

    let paginated_members =
      if start < member_list.len() { member_list[start..end.min(member_list.len())].to_vec() } else { Vec::new() };
    let include_pagination_info = paginated_members.len() < member_list.len();
    let (page, page_size, total_count) = if include_pagination_info {
      (Some(page), Some(page_size), Some(member_list.len() as u32))
    } else {
      (None, None, None)
    };

    transmitter.send_message(Message::ListMembersAck(ListMembersAckParameters {
      id: correlation_id,
      channel: channel_id.into(),
      members: paginated_members,
      page,
      page_size,
      total_count,
    }));

    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  fn history(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    history_id: StringAtom,
    _from_seq: u64,
    limit: u32,
    _direction: Option<StringAtom>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let _limit = limit.min(self.limits.max_history_limit);
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(UserNotInChannel).with_id(correlation_id).into());
    }

    if !channel.acl.is_read_allowed(&nid) {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    if !channel.is_persistent() {
      return Err(narwhal_protocol::Error::new(PersistenceNotEnabled).with_id(correlation_id).into());
    }

    // TODO: implement actual message log retrieval once the message log supports indexed reads.
    // For now, return an empty history result.
    transmitter.send_message(Message::HistoryAck(HistoryAckParameters {
      id: correlation_id,
      history_id,
      channel: channel_id.into(),
      count: 0,
    }));

    Ok(())
  }

  fn channel_seq(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    if channel_id.domain != self.local_domain {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    let Some(channel) = self.channels.get(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(UserNotInChannel).with_id(correlation_id).into());
    }

    if !channel.acl.is_read_allowed(&nid) {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    if !channel.is_persistent() {
      return Err(narwhal_protocol::Error::new(PersistenceNotEnabled).with_id(correlation_id).into());
    }

    // TODO: implement actual first_seq/last_seq retrieval once the message log supports indexed reads.
    // For now, return zeros (empty log).
    transmitter.send_message(Message::ChannelSeqAck(ChannelSeqAckParameters {
      id: correlation_id,
      channel: channel_id.into(),
      first_seq: 0,
      last_seq: 0,
    }));

    Ok(())
  }

  /// Best-effort cleanup of persistent storage for a channel (message log + store record).
  /// Errors are logged but never propagated. Callers must not depend on storage cleanup
  /// succeeding for in-memory consistency.
  async fn delete_persistent_storage(&mut self, handler: &StringAtom) {
    if let Some(channel) = self.channels.get_mut(handler)
      && let Err(e) = channel.message_log.delete().await
    {
      warn!(channel = %handler, error = %e, "failed to delete persisted message log");
    }
    if let Some(hash) = self.channels.get(handler).and_then(|c| c.store_hash.clone()) {
      match self.store.delete_channel(&hash).await {
        Ok(()) => {
          self.metrics.store_deletes.get_or_create(&SUCCESS).inc();
        },
        Err(e) => {
          self.metrics.store_deletes.get_or_create(&FAILURE).inc();
          warn!(channel = %handler, error = %e, "failed to delete persisted channel");
        },
      }
    }
  }

  /// Performs the core leave logic: notify members, remove from channel, handle owner change
  /// or channel deletion. Returns `true` if the member was actually removed.
  async fn do_leave(
    &mut self,
    handler: &StringAtom,
    nid: &Nid,
    excluding_resource: Option<Resource>,
  ) -> anyhow::Result<bool> {
    let Some(channel) = self.channels.get(handler) else {
      return Ok(false);
    };

    if !channel.is_member(nid) {
      return Ok(false);
    }

    let as_owner = channel.is_owner(nid);
    let will_be_empty = channel.member_count() == 1;

    // Pick the new owner once (if needed) so the same value is used for both
    // the persisted projection and the in-memory update.
    let new_owner =
      if as_owner && !will_be_empty { Some(channel.members.iter().find(|m| *m != nid).unwrap().clone()) } else { None };

    // Build new_members once (only when the channel will survive) so it can be
    // shared between the persist and in-memory paths without a second O(n) rebuild.
    let new_members: Option<Rc<[Nid]>> = if !will_be_empty {
      channel.members.binary_search(nid).ok().map(|pos| {
        let mut v: Vec<Nid> = channel.members.iter().cloned().collect();
        v.remove(pos);
        Rc::from(v)
      })
    } else {
      None
    };

    // Persist the projected membership before any in-memory changes or notifications.
    // Skip when the channel will be empty.
    let store_hash = if channel.config.persist == Some(true) && !will_be_empty {
      let mut projected = channel.to_persisted();
      if let Some(ref members) = new_members {
        projected.members = members.clone();
      }
      projected.owner = new_owner.clone().or(projected.owner);
      let start = Instant::now();
      let result = self.store.save_channel(&projected).await;
      self.metrics.store_save_duration_seconds.observe(start.elapsed().as_secs_f64());
      match result {
        Ok(hash) => {
          self.metrics.store_saves.get_or_create(&SUCCESS).inc();
          Some(hash)
        },
        Err(e) => {
          self.metrics.store_saves.get_or_create(&FAILURE).inc();
          return Err(e);
        },
      }
    } else {
      None
    };

    if let Err(e) = channel.notify_member_left(nid, excluding_resource, as_owner, self.local_domain.clone()).await {
      warn!(channel = %handler, error = %e, "failed to notify member left");
    }

    let channel = self.channels.get_mut(handler).unwrap();
    if let Some(hash) = store_hash {
      channel.store_hash = Some(hash);
    }
    if let Some(members) = new_members {
      if channel.owner.as_ref() == Some(nid) {
        channel.owner = None;
      }
      channel.members = members;
      channel.update_allowed_targets();
    } else {
      channel.remove_member(nid);
    }

    if channel.is_empty() {
      let is_persistent = channel.config.persist == Some(true);
      channel.cancel_flush_task();
      if is_persistent {
        let start = Instant::now();
        let result = channel.message_log.flush().await;
        self.metrics.record_flush(start, &result);
        if let Err(e) = result {
          warn!(channel = %handler, error = %e, "final flush on empty channel failed");
        }
        self.delete_persistent_storage(handler).await;
      }
      self.channels.remove(handler);
      self.total_channels.fetch_sub(1, Ordering::SeqCst);
      self.metrics.channels_active.dec();
      return Ok(true);
    }

    if let Some(new_owner) = new_owner {
      let channel = self.channels.get_mut(handler).unwrap();
      channel.owner = Some(new_owner.clone());
      if let Err(e) = channel.notify_member_joined(&new_owner, None, true, self.local_domain.clone()).await {
        warn!(channel = %handler, error = %e, "failed to notify new owner");
      }
    }

    Ok(true)
  }
}

/// Limits for the channel manager.
#[derive(Clone, Debug)]
pub struct ChannelManagerLimits {
  pub max_channels: u32,
  pub max_clients_per_channel: u32,
  pub max_channels_per_client: u32,
  pub max_payload_size: u32,
  pub max_persist_messages: u32,
  pub max_message_flush_interval: u32,
  pub max_history_limit: u32,
}

/// The channel manager.
pub struct ChannelManager<CS: ChannelStore, MLF: MessageLogFactory> {
  store: CS,
  message_log_factory: MLF,
  mailboxes: Arc<[Sender<Command>]>,
  membership: Membership,
  router: GlobalRouter,
  notifier: Notifier,
  total_channels: Arc<AtomicUsize>,
  limits: ChannelManagerLimits,
  metrics: ChannelManagerMetrics,
  mailbox_capacity: usize,
}

impl<CS: ChannelStore, MLF: MessageLogFactory> Clone for ChannelManager<CS, MLF> {
  fn clone(&self) -> Self {
    Self {
      store: self.store.clone(),
      message_log_factory: self.message_log_factory.clone(),
      mailboxes: self.mailboxes.clone(),
      membership: self.membership.clone(),
      router: self.router.clone(),
      notifier: self.notifier.clone(),
      total_channels: self.total_channels.clone(),
      limits: self.limits.clone(),
      metrics: self.metrics.clone(),
      mailbox_capacity: self.mailbox_capacity,
    }
  }
}

impl<CS: ChannelStore, MLF: MessageLogFactory> std::fmt::Debug for ChannelManager<CS, MLF> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChannelManager").finish_non_exhaustive()
  }
}

// === impl ChannelManager ===

impl<CS: ChannelStore, MLF: MessageLogFactory> ChannelManager<CS, MLF> {
  /// Creates a new channel manager with the specified configuration.
  pub fn new(
    router: GlobalRouter,
    notifier: Notifier,
    limits: ChannelManagerLimits,
    store: CS,
    message_log_factory: MLF,
    registry: &mut Registry,
  ) -> Self {
    Self {
      store,
      message_log_factory,
      mailboxes: Arc::from([]),
      membership: Membership::new(),
      router,
      notifier,
      total_channels: Arc::new(AtomicUsize::new(0)),
      limits,
      metrics: ChannelManagerMetrics::register(registry),
      mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
    }
  }

  /// Spawns one shard actor per core on the given dispatcher.
  ///
  /// When `restore_channels` is `true`, persisted channels are loaded from the
  /// store and restored into the appropriate shards. Pass `false` when auth is
  /// disabled — without auth, memberships are ephemeral, so restoring them at
  /// startup would leave orphaned channels.
  pub async fn bootstrap(&mut self, core_dispatcher: &CoreDispatcher, restore_channels: bool) -> anyhow::Result<()> {
    self.membership.bootstrap(core_dispatcher).await?;

    let shard_count = core_dispatcher.shard_count();
    let mut mailboxes = Vec::with_capacity(shard_count);
    let local_domain = self.router.c2s_router().local_domain();

    // Load persisted channel hashes and determine shard assignment.
    // Each hash's channel is loaded to extract the handler for shard routing,
    // then the shard re-loads the full metadata on its own thread during restore.
    let channel_hashes: Arc<[StringAtom]> =
      if restore_channels { self.store.load_channel_hashes().await? } else { Arc::from([]) };

    let mut shard_hashes: Vec<Vec<StringAtom>> = vec![Vec::new(); shard_count];
    for hash in channel_hashes.iter() {
      match self.store.load_channel(hash).await {
        Ok(persisted) => {
          let shard_id = shard_for(&persisted.handler, shard_count);
          shard_hashes[shard_id].push(hash.clone());
        },
        Err(e) => {
          warn!(hash = %hash, error = %e, "skipping channel restore: failed to load persisted channel");
        },
      }
    }

    for (shard_id, hashes_for_shard) in shard_hashes.into_iter().enumerate() {
      let (tx, rx) = async_channel::bounded(self.mailbox_capacity);
      mailboxes.push(tx);

      let store = self.store.clone();
      let message_log_factory = self.message_log_factory.clone();
      let membership = self.membership.clone();
      let router = self.router.clone();
      let notifier = self.notifier.clone();
      let local_domain = local_domain.clone();
      let total_channels = self.total_channels.clone();
      let limits = self.limits.clone();
      let metrics = self.metrics.clone();

      core_dispatcher
        .dispatch_at_shard(shard_id, move || async move {
          let shard = ChannelShard {
            channels: HashMap::new(),
            store,
            message_log_factory,
            membership,
            mailbox: rx,
            router,
            notifier,
            local_domain,
            total_channels,
            limits,
            metrics,
            auth_enabled: restore_channels,
          };
          shard.restore_and_run(hashes_for_shard).await;
        })
        .await?;
    }

    self.mailboxes = Arc::from(mailboxes);

    Ok(())
  }

  /// Shuts down the channel manager.
  pub fn shutdown(&self) {
    for tx in self.mailboxes.iter() {
      tx.close();
    }
    self.membership.shutdown();
  }

  /// Returns the total number of active channels across all shards.
  pub fn total_channels(&self) -> usize {
    self.total_channels.load(Ordering::SeqCst)
  }

  /// Joins a channel.
  pub async fn join_channel(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<bool> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::JoinChannel { channel_id, nid, on_behalf_nid, transmitter, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Leaves a channel.
  pub async fn leave_channel(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Option<Arc<dyn Transmitter>>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::LeaveChannel { channel_id, nid, on_behalf_nid, transmitter, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Removes a user from all channels they are a member of.
  pub async fn leave_all_channels(&self, nid: Nid) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    // Get channels first.
    let handlers = self.membership.get_channels(&nid.username).await;
    if handlers.is_empty() {
      return Ok(());
    }

    // Group handlers by channel shard.
    let mut by_shard: HashMap<usize, Vec<StringAtom>> = HashMap::new();
    for handler in handlers.iter() {
      let shard = shard_for(handler, self.mailboxes.len());
      by_shard.entry(shard).or_default().push(handler.clone());
    }

    // Send batch leave to each relevant shard.
    let mut reply_rxs = Vec::with_capacity(by_shard.len());

    for (shard, handlers) in by_shard {
      let (reply_tx, reply_rx) = async_channel::bounded(1);
      self.mailboxes[shard].send(Command::LeaveChannels { nid: nid.clone(), handlers, reply_tx }).await?;
      reply_rxs.push(reply_rx);
    }
    for reply_rx in reply_rxs {
      reply_rx.recv().await??;
    }

    Ok(())
  }

  /// Deletes a channel.
  pub async fn delete_channel(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::DeleteChannel { channel_id, nid, transmitter, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Broadcasts a payload to all members of a channel.
  pub async fn broadcast_payload(
    &self,
    payload: PoolBuffer,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    qos: Option<u8>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::BroadcastPayload { payload, channel_id, nid, transmitter, qos, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Gets the ACL for a channel.
  #[allow(clippy::too_many_arguments)]
  pub async fn get_channel_acl(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    acl_type: AclType,
    page: Option<u32>,
    page_size: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::GetChannelAcl {
        channel_id,
        nid,
        acl_type,
        page,
        page_size,
        transmitter,
        correlation_id,
        reply_tx,
      })
      .await?;

    reply_rx.recv().await?
  }

  /// Sets the ACL for a channel.
  #[allow(clippy::too_many_arguments)]
  pub async fn set_channel_acl(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    nids: Vec<Nid>,
    acl_type: AclType,
    acl_action: AclAction,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::SetChannelAcl {
        channel_id,
        nid,
        nids,
        acl_type,
        acl_action,
        transmitter,
        correlation_id,
        reply_tx,
      })
      .await?;

    reply_rx.recv().await?
  }

  /// Gets the configuration for a channel.
  pub async fn get_channel_configuration(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::GetChannelConfiguration { channel_id, nid, transmitter, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Sets the configuration for a channel.
  pub async fn set_channel_configuration(
    &self,
    config: ChannelConfig,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::SetChannelConfiguration { config, channel_id, nid, transmitter, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Lists all channels a user is a member of.
  pub async fn list_channels(
    &self,
    nid: Nid,
    page: Option<u32>,
    count: Option<u32>,
    as_owner: bool,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let all_handlers = self.membership.get_channels(&nid.username).await;
    let local_domain = self.router.c2s_router().local_domain();

    let mut channel_list: Vec<StringAtom> = if as_owner {
      // Group by shard, fan-out to check ownership.
      let mut by_shard: HashMap<usize, Vec<StringAtom>> = HashMap::new();
      for handler in all_handlers.iter() {
        by_shard.entry(shard_for(handler, self.mailboxes.len())).or_default().push(handler.clone());
      }
      let mut reply_rxs = Vec::with_capacity(by_shard.len());
      for (shard, handlers) in by_shard {
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        self.mailboxes[shard].send(Command::FilterOwnedChannels { nid: nid.clone(), handlers, reply_tx }).await?;
        reply_rxs.push(reply_rx);
      }
      let mut result = Vec::new();
      for reply_rx in reply_rxs {
        if let Ok(partial) = reply_rx.recv().await {
          result.extend(partial);
        }
      }
      result
    } else {
      all_handlers.iter().map(|h| ChannelId::new_unchecked(h.clone(), local_domain.clone()).into()).collect()
    };

    channel_list.sort();

    let page = page.unwrap_or(1);
    let page_size = count.unwrap_or(20).min(MAX_CHANNELS_PAGE_SIZE);
    let start = ((page - 1) * page_size) as usize;
    let end = (page * page_size) as usize;

    let paginated_channels =
      if start < channel_list.len() { channel_list[start..end.min(channel_list.len())].to_vec() } else { Vec::new() };
    let include_pagination_info = paginated_channels.len() < channel_list.len();
    let (page, page_size, total_count) = if include_pagination_info {
      (Some(page), Some(page_size), Some(channel_list.len() as u32))
    } else {
      (None, None, None)
    };

    transmitter.send_message(Message::ListChannelsAck(ListChannelsAckParameters {
      id: correlation_id,
      channels: paginated_channels,
      page,
      page_size,
      total_count,
    }));

    Ok(())
  }

  /// Lists all members of a channel.
  pub async fn list_members(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    page: Option<u32>,
    count: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::ListMembers { channel_id, nid, page, count, transmitter, correlation_id, reply_tx })
      .await?;

    reply_rx.recv().await?
  }

  /// Requests historical messages from a channel.
  #[allow(clippy::too_many_arguments)]
  pub async fn history(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    history_id: StringAtom,
    from_seq: u64,
    limit: u32,
    direction: Option<StringAtom>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard]
      .send(Command::History {
        channel_id,
        nid,
        history_id,
        from_seq,
        limit,
        direction,
        transmitter,
        correlation_id,
        reply_tx,
      })
      .await?;

    reply_rx.recv().await?
  }

  /// Queries the available sequence range of a channel's message log.
  pub async fn channel_seq(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    self.assert_bootstrapped();

    let shard = shard_for(&channel_id.handler, self.mailboxes.len());
    let (reply_tx, reply_rx) = async_channel::bounded(1);

    self.mailboxes[shard].send(Command::ChannelSeq { channel_id, nid, transmitter, correlation_id, reply_tx }).await?;

    reply_rx.recv().await?
  }

  /// Asserts that `bootstrap()` has been called.
  fn assert_bootstrapped(&self) {
    debug_assert!(!self.mailboxes.is_empty(), "ChannelManager::bootstrap() must be called before use");
  }
}

/// Per-domain ACLs.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Acl {
  allow_lists: HashMap<StringAtom, HashSet<StringAtom>>,
}

// === impl Acl ===

impl Acl {
  /// Updates the ACL based on action.
  fn update(&mut self, nids: Vec<Nid>, action: AclAction) {
    match action {
      AclAction::Add => {
        for nid in nids {
          let domain = nid.domain.clone();
          let username = nid.username.clone();
          let domain_users = self.allow_lists.entry(domain).or_default();
          if !nid.is_server() {
            domain_users.insert(username);
          }
        }
      },
      AclAction::Remove => {
        for nid in nids {
          let domain = nid.domain.clone();
          let username = nid.username.clone();
          if let Some(domain_users) = self.allow_lists.get_mut(&domain) {
            domain_users.remove(&username);
            if domain_users.is_empty() {
              self.allow_lists.remove(&domain);
            }
          }
        }
      },
    }
  }

  fn is_allowed(&self, nid: &Nid) -> bool {
    if self.allow_lists.is_empty() {
      return true;
    }
    let domain = nid.domain.clone();
    let username = nid.username.clone();
    if let Some(allowed_users) = self.allow_lists.get(&domain) {
      if allowed_users.is_empty() {
        return true;
      }
      return allowed_users.contains(&username);
    }
    false
  }

  pub fn allow_list(&self) -> Vec<Nid> {
    let mut allow_list: Vec<Nid> = Vec::with_capacity(self.allow_lists.len());
    for (domain, allowed_users) in self.allow_lists.iter() {
      if !allowed_users.is_empty() {
        for username in allowed_users.iter() {
          allow_list.push(Nid::new_unchecked(username.clone(), domain.clone()));
        }
      } else {
        allow_list.push(Nid::new_unchecked(StringAtom::default(), domain.clone()));
      }
    }
    allow_list.sort();
    allow_list
  }

  pub fn total_entries(&self) -> usize {
    let mut total = 0;
    for (_, allowed_users) in self.allow_lists.iter() {
      total += allowed_users.len();
    }
    total
  }
}

/// The channel ACL.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChannelAcl {
  join_acl: Acl,
  publish_acl: Acl,
  read_acl: Acl,
}

// === impl ChannelAcl ===

impl ChannelAcl {
  pub fn is_join_allowed(&self, nid: &Nid) -> bool {
    self.join_acl.is_allowed(nid)
  }

  pub fn is_publish_allowed(&self, nid: &Nid) -> bool {
    self.publish_acl.is_allowed(nid)
  }

  pub fn is_read_allowed(&self, nid: &Nid) -> bool {
    self.read_acl.is_allowed(nid)
  }

  pub fn update(&mut self, nids: Vec<Nid>, acl_type: AclType, action: AclAction) {
    match acl_type {
      AclType::Join => self.join_acl.update(nids, action),
      AclType::Publish => self.publish_acl.update(nids, action),
      AclType::Read => self.read_acl.update(nids, action),
    }
  }
}

/// The channel configuration.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChannelConfig {
  pub max_clients: Option<u32>,
  pub max_payload_size: Option<u32>,
  pub max_persist_messages: Option<u32>,
  pub persist: Option<bool>,
  pub message_flush_interval: Option<u32>,
}

// === impl ChannelConfig ===

impl ChannelConfig {
  pub fn merge(&self, other: &Self) -> Self {
    let mut config = self.clone();
    if let Some(v) = other.max_clients {
      config.max_clients = Some(v);
    }
    if let Some(v) = other.max_payload_size {
      config.max_payload_size = Some(v);
    }
    if let Some(v) = other.max_persist_messages {
      config.max_persist_messages = Some(v);
    }
    if let Some(v) = other.persist {
      config.persist = Some(v);
    }
    if let Some(v) = other.message_flush_interval {
      config.message_flush_interval = Some(v);
    }
    config
  }
}
