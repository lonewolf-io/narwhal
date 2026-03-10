// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_channel::{Receiver, Sender};

use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_protocol::ErrorReason::{
  BadRequest, ChannelIsFull, ChannelNotFound, Forbidden, NotAllowed, NotImplemented, PolicyViolation,
  ResourceLimitReached, UserInChannel, UserNotInChannel, UserNotRegistered,
};
use narwhal_protocol::{
  AclAction, AclType, BroadcastAckParameters, ChannelAclParameters, ChannelConfigurationParameters,
  DeleteChannelAckParameters, JoinChannelAckParameters, LeaveChannelAckParameters, ListChannelsAckParameters,
  ListMembersAckParameters, Message, MessageParameters, QoS, SetChannelAclAckParameters,
  SetChannelConfigurationAckParameters,
};
use narwhal_protocol::{ChannelId, Nid};
use narwhal_protocol::{Event, EventKind};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::notifier::Notifier;
use crate::router::GlobalRouter;
use crate::transmitter::{Resource, Transmitter};

use super::membership::Membership;

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

/// Metric handles for `ChannelManager`.
#[derive(Clone)]
struct ChannelManagerMetrics {
  channels_active: Gauge,
  channel_joins: Family<ResultLabel, Counter>,
  channel_leaves: Counter,
  channels_deleted: Counter,
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
    Self { channels_active, channel_joins, channel_leaves, channels_deleted }
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
}

/// A channel.
struct Channel {
  handler: StringAtom,
  owner: Option<Nid>,
  config: ChannelConfig,
  acl: ChannelAcl,
  members: HashSet<Nid>,
  allowed_targets: Arc<[Nid]>,
  notifier: Notifier,
  seq: u64,
}

// === impl Channel ===

impl Channel {
  fn new(handler: StringAtom, config: ChannelConfig, notifier: Notifier) -> Self {
    Self {
      handler,
      owner: None,
      config,
      acl: ChannelAcl::default(),
      members: HashSet::new(),
      allowed_targets: Arc::from([]),
      notifier,
      seq: 1,
    }
  }

  fn is_empty(&self) -> bool {
    self.members.is_empty()
  }

  fn is_owner(&self, nid: &Nid) -> bool {
    self.owner == Some(nid.clone())
  }

  fn pick_new_owner(&mut self) -> Option<Nid> {
    if self.is_empty() {
      return None;
    }
    let new_owner_nid = self.members.iter().next().unwrap().clone();
    self.owner = Some(new_owner_nid.clone());
    Some(new_owner_nid)
  }

  fn is_member(&self, nid: &Nid) -> bool {
    self.members.contains(nid)
  }

  fn member_count(&self) -> usize {
    self.members.len()
  }

  fn insert_member(&mut self, nid: Nid) {
    if self.owner.is_none() {
      self.owner = Some(nid.clone());
    }
    self.members.insert(nid);
    self.update_allowed_targets();
  }

  fn remove_member(&mut self, nid: &Nid) -> bool {
    if self.owner == Some(nid.clone()) {
      self.owner = None;
    }
    let removed = self.members.remove(nid);
    self.update_allowed_targets();
    removed
  }

  fn set_acl(&mut self, acl: Acl, acl_type: AclType) {
    match acl_type {
      AclType::Join => self.acl.join_acl = acl,
      AclType::Publish => self.acl.publish_acl = acl,
      AclType::Read => self.acl.read_acl = acl,
    }
    self.update_allowed_targets();
  }

  fn merge_config(&mut self, config: &ChannelConfig) {
    self.config = self.config.merge(config);
  }

  fn update_allowed_targets(&mut self) {
    let acl = &self.acl;
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
}

struct ChannelShard {
  channels: HashMap<StringAtom, Channel>,
  membership: Membership,
  mailbox: Receiver<Command>,
  router: GlobalRouter,
  notifier: Notifier,
  local_domain: StringAtom,
  total_channels: Arc<AtomicUsize>,
  limits: ChannelManagerLimits,
  metrics: ChannelManagerMetrics,
}

// === impl ChannelShard ===

impl ChannelShard {
  async fn run(mut self) {
    while let Ok(cmd) = self.mailbox.recv().await {
      self.handle(cmd).await;
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
        let result = self.set_channel_acl(channel_id, nid, nids, acl_type, acl_action, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
      Command::GetChannelConfiguration { channel_id, nid, transmitter, correlation_id, reply_tx } => {
        let result = self.get_channel_configuration(channel_id, nid, transmitter, correlation_id);
        let _ = reply_tx.send(result).await;
      },
      Command::SetChannelConfiguration { config, channel_id, nid, transmitter, correlation_id, reply_tx } => {
        let result = self.set_channel_configuration(config, channel_id, nid, transmitter, correlation_id);
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
      self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }
    let handler = channel_id.handler.clone();
    let as_owner = !self.channels.contains_key(&handler);

    // Create the channel if it doesn't exist.
    if as_owner {
      if self.total_channels.load(Ordering::SeqCst) >= self.limits.max_channels as usize {
        self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
        return Err(
          narwhal_protocol::Error::new(ResourceLimitReached)
            .with_id(correlation_id)
            .with_detail("maximum channels reached")
            .into(),
        );
      }

      let config = ChannelConfig {
        max_clients: self.limits.max_clients_per_channel,
        max_payload_size: self.limits.max_payload_size,
        max_persist_messages: 0,
      };
      self.channels.insert(handler.clone(), Channel::new(handler.clone(), config, self.notifier.clone()));
      self.total_channels.fetch_add(1, Ordering::SeqCst);
    }

    let new_member_nid = match on_behalf_nid {
      Some(behalf_nid) => {
        let channel = self.channels.get(&handler).unwrap();
        if !channel.is_owner(&nid) {
          if as_owner {
            self.channels.remove(&handler);
          }
          self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
          return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
        }
        if !self.router.c2s_router().has_connection(&behalf_nid.username).await {
          if as_owner {
            self.channels.remove(&handler);
          }
          self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
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
      }
      self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    if channel.is_member(&new_member_nid) {
      if as_owner {
        self.channels.remove(&handler);
      }
      self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(UserInChannel).with_id(correlation_id).into());
    }

    if channel.member_count() >= channel.config.max_clients as usize {
      if as_owner {
        self.channels.remove(&handler);
      }
      self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(ChannelIsFull).with_id(correlation_id).into());
    }

    // All channel-level checks passed, now reserve the membership slot.
    if !self.membership.reserve_slot(&new_member_nid.username, &handler, self.limits.max_channels_per_client).await {
      if as_owner {
        self.channels.remove(&handler);
      }
      self.metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("subscription limit reached")
          .into(),
      );
    }

    let channel = self.channels.get_mut(&handler).unwrap();
    channel.insert_member(new_member_nid.clone());
    channel
      .notify_member_joined(&new_member_nid, Some(transmitter.resource()), as_owner, self.local_domain.clone())
      .await?;

    transmitter.send_message(Message::JoinChannelAck(JoinChannelAckParameters {
      id: correlation_id,
      channel: channel_id.into(),
    }));

    self.metrics.channel_joins.get_or_create(&ResultLabel { result: "success" }).inc();
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
      if self.do_leave(handler, &nid, None).await? {
        removed += 1;
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

    let max_payload_size = channel.config.max_payload_size;
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
    });

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
  fn set_channel_acl(
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

    if new_acl.total_entries() > channel.config.max_clients as usize {
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("ACL allow list exceeds max entries")
          .into(),
      );
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
      max_clients: config.max_clients,
      max_payload_size: config.max_payload_size,
      max_persist_messages: config.max_persist_messages,
    }));

    Ok(())
  }

  fn set_channel_configuration(
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

    if config.max_clients > self.limits.max_clients_per_channel {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_clients exceeds server established limit")
          .into(),
      );
    }

    if config.max_payload_size > self.limits.max_payload_size {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_payload_size exceeds server established limit")
          .into(),
      );
    }

    if config.max_persist_messages > self.limits.max_persist_messages {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_persist_messages exceeds server established limit")
          .into(),
      );
    }
    let Some(channel) = self.channels.get_mut(&channel_id.handler) else {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    };

    if !channel.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    channel.merge_config(&config);

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

    let mut member_list: Vec<StringAtom> = channel.members.iter().map(|m| m.into()).collect();
    member_list.sort();

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

    channel.notify_member_left(nid, excluding_resource, as_owner, self.local_domain.clone()).await?;

    let channel = self.channels.get_mut(handler).unwrap();
    channel.remove_member(nid);

    if channel.is_empty() {
      self.channels.remove(handler);
      self.total_channels.fetch_sub(1, Ordering::SeqCst);
      self.metrics.channels_active.dec();
      return Ok(true);
    }

    if as_owner {
      let channel = self.channels.get_mut(handler).unwrap();
      let new_owner = channel.pick_new_owner().unwrap();
      channel.notify_member_joined(&new_owner, None, true, self.local_domain.clone()).await?;
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
}

/// The channel manager.
#[derive(Clone)]
pub struct ChannelManager {
  mailboxes: Arc<[Sender<Command>]>,
  membership: Membership,
  router: GlobalRouter,
  notifier: Notifier,
  total_channels: Arc<AtomicUsize>,
  limits: ChannelManagerLimits,
  metrics: ChannelManagerMetrics,
  mailbox_capacity: usize,
}

impl std::fmt::Debug for ChannelManager {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChannelManager").finish_non_exhaustive()
  }
}

// === impl ChannelManager ===

impl ChannelManager {
  /// Creates a new channel manager with the specified configuration.
  pub fn new(router: GlobalRouter, notifier: Notifier, limits: ChannelManagerLimits, registry: &mut Registry) -> Self {
    Self {
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
  pub async fn bootstrap(&mut self, core_dispatcher: &CoreDispatcher) -> anyhow::Result<()> {
    self.membership.bootstrap(core_dispatcher).await?;

    let shard_count = core_dispatcher.shard_count();
    let mut mailboxes = Vec::with_capacity(shard_count);
    let local_domain = self.router.c2s_router().local_domain();

    for shard_id in 0..shard_count {
      let (tx, rx) = async_channel::bounded(self.mailbox_capacity);
      mailboxes.push(tx);

      let shard = ChannelShard {
        channels: HashMap::new(),
        membership: self.membership.clone(),
        mailbox: rx,
        router: self.router.clone(),
        notifier: self.notifier.clone(),
        local_domain: local_domain.clone(),
        total_channels: self.total_channels.clone(),
        limits: self.limits.clone(),
        metrics: self.metrics.clone(),
      };

      core_dispatcher
        .dispatch_at_shard(shard_id, move || async move {
          shard.run().await;
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

    // All leaves succeeded, now release the membership slots.
    self.membership.release_all_slots(&nid.username).await;

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

  /// Asserts that `bootstrap()` has been called.
  fn assert_bootstrapped(&self) {
    debug_assert!(!self.mailboxes.is_empty(), "ChannelManager::bootstrap() must be called before use");
  }
}

/// Per-domain ACLs.
#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug, Default)]
pub struct ChannelConfig {
  pub max_clients: u32,
  pub max_payload_size: u32,
  pub max_persist_messages: u32,
}

// === impl ChannelConfig ===

impl ChannelConfig {
  pub fn merge(&self, other: &Self) -> Self {
    let mut config = self.clone();
    if other.max_clients > 0 {
      config.max_clients = other.max_clients;
    }
    if other.max_payload_size > 0 {
      config.max_payload_size = other.max_payload_size;
    }
    if other.max_persist_messages > 0 {
      config.max_persist_messages = other.max_persist_messages;
    }
    config
  }
}
