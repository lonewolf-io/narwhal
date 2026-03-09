// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_lock::RwLock;
use dashmap::DashMap;

use narwhal_protocol::ErrorReason::{
  BadRequest, ChannelIsFull, ChannelNotFound, Forbidden, NotAllowed, NotImplemented, PolicyViolation, ResourceConflict,
  UserInChannel, UserNotInChannel, UserNotRegistered,
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

const DASH_MAP_SHARD_COUNT: usize = 1024;

const MAX_CHANNELS_PAGE_SIZE: u32 = 50;

const MAX_MEMBERS_PAGE_SIZE: u32 = 100;

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

/// The channel manager inner state.
#[derive(Debug)]
struct ChannelManagerInner {
  /// The channels.
  channels: Arc<DashMap<StringAtom, Channel>>,

  /// The channels a user is a member of.
  in_channels: Arc<DashMap<StringAtom, HashSet<ChannelId>>>,

  /// The global router.
  router: GlobalRouter,

  /// The event notifier.
  notifier: Notifier,

  /// The maximum allowed clients per channel.
  max_clients_per_channel: u32,

  /// The maximum number of channels a user can join.
  max_channels_per_client: u32,

  /// The maximum payload size allowed.
  max_payload_size: u32,

  /// Metric handles.
  metrics: ChannelManagerMetrics,
}

/// The channel manager.
#[derive(Clone, Debug)]
pub struct ChannelManager(Arc<RwLock<ChannelManagerInner>>);

// ===== impl ChannelManager =====

impl ChannelManager {
  /// Creates a new channel manager with the specified configuration.
  ///
  /// # Arguments
  ///
  /// * `router` - The global router
  /// * `max_channels` - Maximum number of channels allowed
  /// * `max_clients_per_channel` - Maximum number of clients allowed per channel
  /// * `max_channels_per_client` - Maximum number of channels a client can join
  /// * `max_payload_size` - Maximum payload size allowed in channels
  ///
  /// # Returns
  ///
  /// A new instance of `ChannelManager`
  pub fn new(
    router: GlobalRouter,
    notifier: Notifier,
    max_channels: u32,
    max_clients_per_channel: u32,
    max_channels_per_client: u32,
    max_payload_size: u32,
    registry: &mut Registry,
  ) -> Self {
    let channels_map = DashMap::with_capacity_and_shard_amount(max_channels as usize, DASH_MAP_SHARD_COUNT);
    let in_channels = DashMap::with_shard_amount(DASH_MAP_SHARD_COUNT);

    let inner = ChannelManagerInner {
      router,
      notifier,
      channels: Arc::new(channels_map),
      in_channels: Arc::new(in_channels),
      max_clients_per_channel,
      max_channels_per_client,
      max_payload_size,
      metrics: ChannelManagerMetrics::register(registry),
    };

    Self(Arc::new(RwLock::new(inner)))
  }

  /// Lists all channels that a user is a member of.
  ///
  /// # Arguments
  ///
  /// * `nid` - The user identifier
  /// * `as_owner` - If true, only lists channels where the user is the owner
  /// * `transmitter` - The transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn list_channels(
    &self,
    nid: Nid,
    page: Option<u32>,
    count: Option<u32>,
    as_owner: bool,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let channels = mng_guard.channels.clone();
    let in_channels = mng_guard.in_channels.clone();
    drop(mng_guard);

    // Gather all channels the user is a member of and sort them.
    let mut channel_list: Vec<StringAtom> = Default::default();

    if let Some(in_channels_set) = in_channels.get(&nid.username) {
      for channel_id in in_channels_set.iter() {
        if as_owner {
          match channels.get(&channel_id.handler) {
            Some(channel) => {
              if channel.0.read().await.is_owner(&nid) {
                channel_list.push(channel_id.into());
              }
            },
            None => continue,
          }
        } else {
          channel_list.push(channel_id.into());
        }
      }
    }

    channel_list.sort();

    // Apply pagination if specified
    let page = page.unwrap_or(1);
    let page_size = count.unwrap_or(20).min(MAX_CHANNELS_PAGE_SIZE);

    let start = ((page - 1) * page_size) as usize;
    let end = (page * page_size) as usize;

    let paginated_channels =
      if start < channel_list.len() { channel_list[start..end.min(channel_list.len())].to_vec() } else { Vec::new() };

    // Include pagination information if necessary
    let include_pagination_info = paginated_channels.len() < channel_list.len();

    let (page, page_size, total_count) = if include_pagination_info {
      (Some(page), Some(page_size), Some(channel_list.len() as u32))
    } else {
      (None, None, None)
    };

    // Send response back to the client.
    transmitter.send_message(Message::ListChannelsAck(ListChannelsAckParameters {
      id: correlation_id,
      channels: paginated_channels,
      page,
      page_size,
      total_count,
    }));

    Ok(())
  }

  /// Lists all members of a specific channel.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier
  /// * `nid` - The user identifier requesting the list
  /// * `transmitter` - The transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn list_members(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    page: Option<u32>,
    count: Option<u32>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotImplemented).into());
    }

    // Check if the channel exists and if the originating connection is a member of it.
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let channel_inner = channel.0.read().await;

    if !channel_inner.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(UserNotInChannel).with_id(correlation_id).into());
    }
    // Gather all members of the channel and sort them.
    let mut member_list: Vec<StringAtom> = channel_inner.members.iter().map(|member_nid| member_nid.into()).collect();
    drop(channel_inner);

    member_list.sort();

    // Apply pagination if specified
    let page = page.unwrap_or(1);
    let page_size = count.unwrap_or(20).min(MAX_MEMBERS_PAGE_SIZE);

    let start = ((page - 1) * page_size) as usize;
    let end = (page * page_size) as usize;

    let paginated_members =
      if start < member_list.len() { member_list[start..end.min(member_list.len())].to_vec() } else { Vec::new() };

    // Include pagination information if necessary
    let include_pagination_info = paginated_members.len() < member_list.len();

    let (page, page_size, total_count) = if include_pagination_info {
      (Some(page), Some(page_size), Some(member_list.len() as u32))
    } else {
      (None, None, None)
    };

    // Send response back to the client.
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

  /// Joins a channel.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier to join
  /// * `nid` - The user identifier joining the channel
  /// * `on_behalf_nid` - Optional user identifier to join on behalf of
  /// * `transmitter` - The connection transmitter for the client
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result containing a boolean that is `true` if the user joined as the channel owner
  /// (i.e., the channel was created), or `false` if joining an existing channel
  pub async fn join_channel(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    oh_behalf_nid: Option<Nid>,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<bool> {
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();

    let notifier = mng_guard.notifier.clone();
    let in_channels = mng_guard.in_channels.clone();

    let max_channels_per_client = mng_guard.max_channels_per_client;
    let max_clients_per_channel = mng_guard.max_clients_per_channel;
    let max_payload_size = mng_guard.max_payload_size;
    let metrics = mng_guard.metrics.clone();

    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }
    let handler = channel_id.handler.clone();

    // Check if the channel exists, and create it if it doesn't
    let mut as_owner = false;

    let entry_ref = channels.entry(handler.clone()).or_insert_with(|| {
      let channel_inner = ChannelInner {
        handler: handler.clone(),
        owner: None,
        config: ChannelConfig { max_clients: max_clients_per_channel, max_payload_size },
        acl: ChannelAcl::default(),
        members: HashSet::new(),
        allowed_targets: Arc::default(),
        notifier,
        seq: AtomicU64::new(1),
      };

      as_owner = true;

      Channel(Arc::new(RwLock::new(channel_inner)))
    });

    let channel = entry_ref.value().clone();
    drop(entry_ref);

    let mut channel_inner = channel.0.write().await;

    // Check if the channel was removed while we were waiting for the lock.
    // This can happen if the last member left, triggering channel removal.
    if !channels.contains_key(&handler) {
      metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(
        narwhal_protocol::Error::new(ResourceConflict)
          .with_id(correlation_id)
          .with_detail("channel was removed during join operation")
          .into(),
      );
    }

    // Get the NID of the new member.
    let new_member_nid = {
      match oh_behalf_nid {
        Some(oh_behalf_nid) => {
          // Ensure the client is authorized to join the channel on behalf of another user.
          if !channel_inner.is_owner(&nid) {
            return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
          }

          if !router.c2s_router().has_connection(&oh_behalf_nid.username).await {
            return Err(narwhal_protocol::Error::new(UserNotRegistered).with_id(correlation_id).into());
          }

          oh_behalf_nid.clone()
        },
        None => nid.clone(),
      }
    };
    // Check if the new member is allowed to join the channel.
    let acl = &channel_inner.acl;

    if !acl.is_join_allowed(&new_member_nid) {
      metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    // Insert the member into the channel in case it is not already a member
    // and the channel is not full, and notify all members about the new member.
    let config = &channel_inner.config;

    if channel_inner.is_member(&new_member_nid) {
      metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(UserInChannel).with_id(correlation_id).into());
    } else if channel_inner.member_count() >= config.max_clients as usize {
      metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(narwhal_protocol::Error::new(ChannelIsFull).with_id(correlation_id).into());
    }
    // Check if the maximum number of subscriptions is reached.
    if let Some(in_channels) = in_channels.get(&new_member_nid.username)
      && in_channels.len() >= max_channels_per_client as usize
    {
      metrics.channel_joins.get_or_create(&ResultLabel { result: "failure" }).inc();
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("subscription limit reached")
          .into(),
      );
    }

    channel_inner.insert_member(new_member_nid.clone());

    channel_inner
      .notify_member_joined(
        &new_member_nid,
        Some(transmitter.resource()),
        as_owner,
        router.c2s_router().local_domain().clone(),
      )
      .await?;
    drop(channel_inner);

    // Update the list of channels the connection is a member of.
    in_channels.entry(new_member_nid.username.clone()).or_default().insert(channel_id.clone());

    // Send response back to the client.
    transmitter.send_message(Message::JoinChannelAck(JoinChannelAckParameters {
      id: correlation_id,
      channel: channel_id.into(),
    }));

    metrics.channel_joins.get_or_create(&ResultLabel { result: "success" }).inc();
    if as_owner {
      metrics.channels_active.inc();
    }

    Ok(as_owner)
  }

  /// Leaves a channel.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier to leave
  /// * `nid` - The user identifier leaving the channel
  /// * `on_behalf_nid` - Optional user identifier to leave on behalf of
  /// * `transmitter` - Optional connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn leave_channel(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    on_behalf_nid: Option<Nid>,
    transmitter: Option<Arc<dyn Transmitter>>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;

    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    let in_channels = mng_guard.in_channels.clone();
    let metrics = mng_guard.metrics.clone();

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }
    // Check if the channel exists
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let mut channel_inner = channel.0.write().await;

    // Re-verify channel is still in the map after acquiring lock
    // (another thread might have removed it while we were waiting)
    if channels.get(&channel_id.handler).is_none() {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    }

    let left_member_nid = {
      match on_behalf_nid {
        Some(z) => {
          if !channel_inner.is_owner(&nid) {
            return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
          }
          z
        },
        None => nid.clone(),
      }
    };

    if !channel_inner.is_member(&left_member_nid) {
      return Err(narwhal_protocol::Error::new(UserNotInChannel).with_id(correlation_id).into());
    }

    // Notify members about the left member, and remove the member from the channel.
    let as_owner = channel_inner.is_owner(&left_member_nid);

    let resource = transmitter.as_ref().map(|transmitter| transmitter.resource());

    channel_inner
      .notify_member_left(&left_member_nid, resource, as_owner, router.c2s_router().local_domain().clone())
      .await?;

    channel_inner.remove_member(&left_member_nid);

    // Update the list of channels the connection is a member of.
    in_channels.remove_if_mut(&left_member_nid.username, |_, in_channels_set| {
      in_channels_set.remove(&channel_id);
      in_channels_set.is_empty()
    });

    // Send response back to the client if a handler is provided.
    if let Some(transmitter) = transmitter {
      transmitter.send_message(Message::LeaveChannelAck(LeaveChannelAckParameters { id: correlation_id }));
    }

    metrics.channel_leaves.inc();

    // If the channel is now empty, release it.
    if channel_inner.is_empty() {
      channels.remove(&channel_inner.handler);
      metrics.channels_active.dec();

      return Ok(());
    }

    // If the left member was the owner, pick a new owner (randomly) and notify all members.
    if as_owner {
      let new_owner_nid = channel_inner.pick_new_owner().unwrap();

      channel_inner
        .notify_member_joined(&new_owner_nid, None, true, router.c2s_router().local_domain().clone())
        .await?;
    }

    Ok(())
  }

  /// Leaves all channels that a user is a member of.
  ///
  /// # Arguments
  ///
  /// * `nid` - The user identifier leaving all channels
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn leave_all_channels(&mut self, nid: Nid) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let in_channels = mng_guard.in_channels.clone();
    drop(mng_guard);

    if let Some((_, in_channels_set)) = in_channels.remove(&nid.username) {
      for channel_id in in_channels_set.iter() {
        self.leave_channel(channel_id.clone(), nid.clone(), None, None, 0).await?;
      }
    }
    Ok(())
  }

  /// Deletes a channel.
  ///
  /// Only the channel owner is allowed to delete the channel. All members are
  /// removed, the channel is destroyed, and a `ChannelDeleted` event is sent
  /// to every member except the owner's own connection.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier to delete
  /// * `nid` - The user identifier requesting the deletion
  /// * `transmitter` - The connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn delete_channel(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    let in_channels = mng_guard.in_channels.clone();
    let metrics = mng_guard.metrics.clone();
    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    // Check if the channel exists.
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let channel_inner = channel.0.write().await;

    // Re-verify channel is still in the map after acquiring lock.
    if channels.get(&channel_id.handler).is_none() {
      return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into());
    }

    // Only the owner of the channel can delete it.
    if !channel_inner.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    // Notify all members (except the owner) that the channel has been deleted.
    channel_inner
      .notify_channel_deleted(Some(transmitter.resource()), router.c2s_router().local_domain().clone())
      .await?;

    // Collect all member usernames before dropping the lock.
    let member_usernames: Vec<StringAtom> = channel_inner.members.iter().map(|m| m.username.clone()).collect();
    drop(channel_inner);

    // Remove this channel from every member's in_channels set.
    for username in &member_usernames {
      in_channels.remove_if_mut(username, |_, in_channels_set| {
        in_channels_set.remove(&channel_id);
        in_channels_set.is_empty()
      });
    }

    // Remove the channel from the channels map.
    channels.remove(&channel_id.handler);

    // Send the ack to the owner.
    transmitter.send_message(Message::DeleteChannelAck(DeleteChannelAckParameters { id: correlation_id }));

    metrics.channels_deleted.inc();
    metrics.channels_active.dec();

    Ok(())
  }

  /// Gets the access control list (ACL) for a channel.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier
  /// * `nid` - The user identifier requesting the ACL
  /// * `acl_type` - The type of ACL to retrieve (Join, Publish, or Read)
  /// * `transmitter` - The connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
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
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    // Check if the channel exists
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let channel_inner = channel.0.read().await;

    // Only owner is allowed to get the channel ACL.
    if !channel_inner.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    // Obtain the channel's ACL.
    let acl = match acl_type {
      AclType::Join => channel_inner.acl.join_acl.clone(),
      AclType::Publish => channel_inner.acl.publish_acl.clone(),
      AclType::Read => channel_inner.acl.read_acl.clone(),
    };

    drop(channel_inner);

    let all_nids: Vec<StringAtom> = acl.allow_list().into_iter().map(|z| z.into()).collect();
    let total_count = all_nids.len() as u32;

    // Apply pagination if requested
    let (nids, response_page, response_page_size, response_total_count) =
      if let (Some(page), Some(page_size)) = (page, page_size) {
        let start = ((page - 1) * page_size) as usize;
        let end = (start + page_size as usize).min(all_nids.len());
        let paginated_nids = if start < all_nids.len() { all_nids[start..end].to_vec() } else { Vec::new() };
        (paginated_nids, Some(page), Some(page_size), Some(total_count))
      } else {
        (all_nids, None, None, None)
      };

    // Send response back to the client.
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

  /// Sets the access control list (ACL) for a channel.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier
  /// * `nid` - The user identifier setting the ACL
  /// * `nids` - The list of NIDs to add or remove
  /// * `acl_type` - The type of ACL being set
  /// * `acl_action` - The action being performed on the ACL
  /// * `transmitter` - The connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  #[allow(clippy::too_many_arguments)]
  pub async fn set_channel_acl(
    &mut self,
    channel_id: ChannelId,
    nid: Nid,
    nids: Vec<Nid>,
    acl_type: AclType,
    acl_action: AclAction,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }

    // Check if the channel exists
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let mut channel_inner = channel.0.write().await;

    // Only the owner of the channel can change its ACL.
    if !channel_inner.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    // Update the ACL
    let mut new_acl = match acl_type {
      AclType::Join => channel_inner.acl.join_acl.clone(),
      AclType::Publish => channel_inner.acl.publish_acl.clone(),
      AclType::Read => channel_inner.acl.read_acl.clone(),
    };

    new_acl.update(nids, acl_action);

    // Validate the updated ACL
    let channel_max_clients = channel_inner.config.max_clients as usize;

    if new_acl.total_entries() > channel_max_clients {
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("ACL allow list exceeds max entries")
          .into(),
      );
    }

    // Set the updated ACL
    channel_inner.set_acl(new_acl, acl_type);
    drop(channel_inner);

    // Send ACK response back to the client.
    transmitter.send_message(Message::SetChannelAclAck(SetChannelAclAckParameters { id: correlation_id }));

    Ok(())
  }

  /// Gets the configuration for a channel.
  ///
  /// # Arguments
  ///
  /// * `channel_id` - The channel identifier
  /// * `nid` - The user identifier requesting the configuration
  /// * `transmitter` - The connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn get_channel_configuration(
    &self,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let channels = mng_guard.channels.clone();
    drop(mng_guard);

    // Check if the channel exists
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let channel_inner = channel.0.read().await;

    // Only members of the channel can get its configuration.
    if !channel_inner.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }

    // Obtain the channel configuration.
    let config = channel_inner.config.clone();
    drop(channel_inner);

    // Send response back to the client.
    transmitter.send_message(Message::ChannelConfiguration(ChannelConfigurationParameters {
      id: correlation_id,
      channel: channel_id.into(),
      max_clients: config.max_clients,
      max_payload_size: config.max_payload_size,
    }));

    Ok(())
  }

  /// Sets the configuration for a channel.
  ///
  /// # Arguments
  ///
  /// * `config` - The new channel configuration
  /// * `channel_id` - The channel identifier
  /// * `nid` - The user identifier setting the configuration
  /// * `transmitter` - The connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn set_channel_configuration(
    &mut self,
    config: ChannelConfig,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    let max_clients_per_channel = mng_guard.max_clients_per_channel;
    let max_payload_size = mng_guard.max_payload_size;
    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }
    // Validate the new channel configuration
    if config.max_clients > max_clients_per_channel {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_clients exceeds server established limit")
          .into(),
      );
    }

    if config.max_payload_size > max_payload_size {
      return Err(
        narwhal_protocol::Error::new(BadRequest)
          .with_id(correlation_id)
          .with_detail("max_payload_size exceeds server established limit")
          .into(),
      );
    }

    // Check if the channel exists
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let mut channel_inner = channel.0.write().await;

    // Only the owner of the channel can change its configuration.
    if !channel_inner.is_owner(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }
    // Merge the current configuration with the new one.
    channel_inner.merge_config(&config);

    drop(channel_inner);

    // Send ACK response back to the client.
    transmitter
      .send_message(Message::SetChannelConfigurationAck(SetChannelConfigurationAckParameters { id: correlation_id }));

    Ok(())
  }

  /// Broadcasts a payload to all members of a channel.
  ///
  /// # Arguments
  ///
  /// * `payload` - The payload to broadcast
  /// * `channel_id` - The channel identifier
  /// * `nid` - The user identifier broadcasting the payload
  /// * `transmitter` - The connection transmitter for sending the response
  /// * `correlation_id` - The correlation ID for the request
  ///
  /// # Returns
  ///
  /// A result indicating success or failure
  pub async fn broadcast_payload(
    &mut self,
    payload: PoolBuffer,
    channel_id: ChannelId,
    nid: Nid,
    transmitter: Arc<dyn Transmitter>,
    qos: Option<u8>,
    correlation_id: u32,
  ) -> anyhow::Result<()> {
    let mng_guard = self.0.read().await;
    let router = mng_guard.router.clone();
    let channels = mng_guard.channels.clone();
    drop(mng_guard);

    // Ensure the channel is local.
    if channel_id.domain != router.c2s_router().local_domain() {
      return Err(narwhal_protocol::Error::new(NotImplemented).with_id(correlation_id).into());
    }

    // Check if the channel exists.
    let channel = {
      match channels.get(&channel_id.handler) {
        Some(kv) => {
          let channel = kv.value();
          (*channel).clone()
        },
        None => return Err(narwhal_protocol::Error::new(ChannelNotFound).with_id(correlation_id).into()),
      }
    };
    let channel_inner = channel.0.read().await;

    if !channel_inner.is_member(&nid) {
      return Err(narwhal_protocol::Error::new(Forbidden).with_id(correlation_id).into());
    }
    // Check if the member is allowed to publish to the channel.
    let acl = &channel_inner.acl;

    if !acl.is_publish_allowed(&nid) {
      return Err(narwhal_protocol::Error::new(NotAllowed).with_id(correlation_id).into());
    }
    let max_payload_size = channel_inner.config.max_payload_size;
    let allowed_targets = channel_inner.allowed_targets.clone();
    let seq = channel_inner.seq.fetch_add(1, Ordering::Relaxed);
    drop(channel_inner);

    // Capture the wall-clock timestamp once so all recipients see the same value.
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;

    // Validate channel established payload size limit.
    let payload_length = payload.as_slice().len() as u32;

    if payload_length > max_payload_size {
      return Err(
        narwhal_protocol::Error::new(PolicyViolation)
          .with_id(correlation_id)
          .with_detail("payload size exceeds channel limit")
          .into(),
      );
    }
    let qos = qos.map(QoS::try_from).transpose()?.unwrap_or(QoS::default());

    if qos == QoS::AckOnReceived {
      transmitter.send_message(Message::BroadcastAck(BroadcastAckParameters { id: correlation_id, seq }));
    }

    // Broadcast the payload to all members of the channel, except the sender.
    let msg = Message::Message(MessageParameters {
      from: (&nid).into(),
      channel: (&channel_id).into(),
      length: payload_length,
      seq,
      timestamp,
    });

    router.route_to_many(msg, Some(payload), allowed_targets.iter(), Some(transmitter.resource())).await?;

    if qos == QoS::AckOnDelivered {
      transmitter.send_message(Message::BroadcastAck(BroadcastAckParameters { id: correlation_id, seq }));
    }

    Ok(())
  }
}

/// A channel.
#[derive(Clone, Debug)]
pub struct Channel(Arc<RwLock<ChannelInner>>);

/// The inner state of a channel.
#[derive(Debug)]
pub struct ChannelInner {
  /// The channel handler.
  handler: StringAtom,

  /// The owner of the channel.
  owner: Option<Nid>,

  /// The channel configuration.
  config: ChannelConfig,

  /// The channel ACL.
  acl: ChannelAcl,

  /// The members of the channel (including the owner).
  members: HashSet<Nid>,

  /// The channel allowed targets for broadcasting.
  allowed_targets: Arc<[Nid]>,

  /// The notifier for sending events to channel members.
  notifier: Notifier,

  /// Monotonically increasing sequence counter for messages published to this channel.
  /// Starts at 1; each broadcast atomically claims the next value.
  seq: AtomicU64,
}

// ===== impl ChannelInner =====

impl ChannelInner {
  /// Checks if the channel is empty.
  fn is_empty(&self) -> bool {
    self.members.is_empty()
  }

  /// Checks if the channel is owned by a certain handler.
  fn is_owner(&self, nid: &Nid) -> bool {
    self.owner == Some(nid.clone())
  }

  /// Picks a new owner for the channel.
  fn pick_new_owner(&mut self) -> Option<Nid> {
    if self.is_empty() {
      return None;
    }

    let new_owner_nid = self.members.iter().next().unwrap().clone();

    // Update owner handler.
    self.owner = Some(new_owner_nid.clone());

    Some(new_owner_nid)
  }

  /// Checks if the channel has a certain member.
  fn is_member(&self, nid: &Nid) -> bool {
    self.members.contains(nid)
  }

  /// Returns the number of members in the channel.
  fn member_count(&self) -> usize {
    self.members.len()
  }

  /// Inserts a member into the channel.
  fn insert_member(&mut self, nid: Nid) {
    if self.owner.is_none() {
      self.owner = Some(nid.clone());
    }
    self.members.insert(nid);
    self.update_allowed_targets();
  }

  /// Removes a member from the channel.
  fn remove_member(&mut self, nid: &Nid) -> bool {
    if self.owner == Some(nid.clone()) {
      self.owner = None;
    }
    let removed = self.members.remove(nid);
    self.update_allowed_targets();
    removed
  }

  /// Sets the ACL for the channel.
  fn set_acl(&mut self, acl: Acl, acl_type: AclType) {
    match acl_type {
      AclType::Join => self.acl.join_acl = acl,
      AclType::Publish => self.acl.publish_acl = acl,
      AclType::Read => self.acl.read_acl = acl,
    }
    self.update_allowed_targets();
  }

  /// Merges the configuration for the channel.
  fn merge_config(&mut self, config: &ChannelConfig) -> ChannelConfig {
    self.config = self.config.merge(config);
    self.config.clone()
  }

  /// Updates the allowed targets for broadcasting.
  fn update_allowed_targets(&mut self) {
    let acl = &self.acl;
    let targets: Vec<Nid> = self.members.iter().filter(|m| acl.is_read_allowed(m)).cloned().collect();
    self.allowed_targets = Arc::from(targets);
  }

  /// Notifies all members that a new member has joined the channel.
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

    self.notifier.notify(event, self.members.iter(), excluding_resource).await?;

    Ok(())
  }

  /// Notifies all members that a member has left the channel.
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

    self.notifier.notify(event, self.members.iter(), excluding_resource).await?;

    Ok(())
  }

  /// Notifies all members that the channel has been deleted.
  async fn notify_channel_deleted(
    &self,
    excluding_resource: Option<Resource>,
    local_domain: StringAtom,
  ) -> anyhow::Result<()> {
    let channel_id = ChannelId::new_unchecked(self.handler.clone(), local_domain);

    let event = Event::new(EventKind::ChannelDeleted).with_channel(channel_id.into());

    self.notifier.notify(event, self.members.iter(), excluding_resource).await?;

    Ok(())
  }
}

/// Per-domain ACLs.
#[derive(Clone, Debug, Default)]
struct Acl {
  allow_lists: HashMap<StringAtom, HashSet<StringAtom>>,
}

// ===== impl Acl =====

impl Acl {
  /// Updates the ACL based on action.
  fn update(&mut self, nids: Vec<Nid>, action: AclAction) {
    match action {
      AclAction::Add => {
        // Add NIDs to the allow list
        for nid in nids {
          let domain = nid.domain.clone();
          let username = nid.username.clone();

          let domain_users = self.allow_lists.entry(domain).or_default();

          // Only add non-server users
          if !nid.is_server() {
            domain_users.insert(username);
          }
        }
      },
      AclAction::Remove => {
        // Remove NIDs from the allow list
        for nid in nids {
          let domain = nid.domain.clone();
          let username = nid.username.clone();

          if let Some(domain_users) = self.allow_lists.get_mut(&domain) {
            domain_users.remove(&username);

            // If the domain has no more users, remove the domain entry
            if domain_users.is_empty() {
              self.allow_lists.remove(&domain);
            }
          }
        }
      },
    }
  }

  /// Checks if a NID is allowed based on the ACL.
  fn is_allowed(&self, nid: &Nid) -> bool {
    // If ACL map is empty, access is allowed by default
    if self.allow_lists.is_empty() {
      return true;
    }
    let domain = nid.domain.clone();
    let username = nid.username.clone();

    if let Some(allowed_users) = self.allow_lists.get(&domain) {
      if allowed_users.is_empty() {
        // If allowed users is empty, access is allowed by default
        return true;
      }
      return allowed_users.contains(&username);
    }
    false
  }

  /// Returns the ACL allowlist.
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

  /// Returns the total number of entries in the ACL.
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
  /// Domain ACL for allowed users to join to the channel.
  join_acl: Acl,

  /// Domain ACL for allowed users to publish to the channel.
  publish_acl: Acl,

  /// Domain ACL for allowed users to read from the channel.
  read_acl: Acl,
}

// ===== impl ChannelAcl =====

impl ChannelAcl {
  /// Checks if a NID is allowed to join to the channel.
  pub fn is_join_allowed(&self, nid: &Nid) -> bool {
    self.join_acl.is_allowed(nid)
  }

  /// Checks if a NID is allowed to publish to the channel.
  pub fn is_publish_allowed(&self, nid: &Nid) -> bool {
    self.publish_acl.is_allowed(nid)
  }

  /// Checks if a NID is allowed to read from the channel.
  pub fn is_read_allowed(&self, nid: &Nid) -> bool {
    self.read_acl.is_allowed(nid)
  }

  /// Updates the ACL based on the type and action.
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
  /// The maximum number of clients allowed in the channel.
  pub max_clients: u32,

  /// The maximum payload size allowed.
  pub max_payload_size: u32,
}

// ===== impl ChannelConfig =====

impl ChannelConfig {
  /// Merges two channel configurations.
  pub fn merge(&self, other: &Self) -> Self {
    let mut config = self.clone();

    if other.max_clients > 0 {
      config.max_clients = other.max_clients;
    }
    if other.max_payload_size > 0 {
      config.max_payload_size = other.max_payload_size;
    }

    config
  }
}
