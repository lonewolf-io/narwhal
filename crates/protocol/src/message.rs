// SPDX-License-Identifier: BSD-3-Clause

use std::str::FromStr;

use crate::deserialize::ParameterReader;
use crate::serialize::{ParameterWriter, SerializeError};
use crate::{AclAction, AclType, ErrorReason, EventKind};

use narwhal_protocol_macros::ProtocolMessageParameters;
use narwhal_util::string_atom::StringAtom;

/// A trait for decoding and encoding the parameters of a message.
pub trait ProtocolMessageParameters {
  /// Encodes the message parameters.
  /// The number of bytes written is returned.
  fn encode(&self, parameter_writer: &mut ParameterWriter) -> Result<usize, SerializeError>;

  /// Decodes the message parameters.
  fn decode(&mut self, parameter_reader: &mut ParameterReader) -> anyhow::Result<()>;

  /// Validates the message parameters.
  fn validate(&self) -> anyhow::Result<()>;
}

/// Represents information about a payload associated with a message.
#[derive(Debug, Default)]
pub struct PayloadInfo {
  /// The message identifier associated with the payload.
  pub id: Option<u32>,

  /// The length of the payload in bytes.
  pub length: usize,
}

/// Represents a protocol message that can be sent or received.
/// Each variant contains the parameters specific to that message type.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
  Auth(AuthParameters),
  AuthAck(AuthAckParameters),
  Broadcast(BroadcastParameters),
  BroadcastAck(BroadcastAckParameters),
  ChannelAcl(ChannelAclParameters),
  ChannelConfiguration(ChannelConfigurationParameters),
  Connect(ConnectParameters),
  ConnectAck(ConnectAckParameters),
  DeleteChannel(DeleteChannelParameters),
  DeleteChannelAck(DeleteChannelAckParameters),
  Error(ErrorParameters),
  Event(EventParameters),
  GetChannelAcl(GetChannelAclParameters),
  GetChannelConfiguration(GetChannelConfigurationParameters),
  History(HistoryParameters),
  HistoryAck(HistoryAckParameters),
  ChannelSeq(ChannelSeqParameters),
  ChannelSeqAck(ChannelSeqAckParameters),
  Identify(IdentifyParameters),
  IdentifyAck(IdentifyAckParameters),
  JoinChannel(JoinChannelParameters),
  JoinChannelAck(JoinChannelAckParameters),
  LeaveChannel(LeaveChannelParameters),
  LeaveChannelAck(LeaveChannelAckParameters),
  ListChannels(ListChannelsParameters),
  ListChannelsAck(ListChannelsAckParameters),
  ListMembers(ListMembersParameters),
  ListMembersAck(ListMembersAckParameters),
  M2sConnect(M2sConnectParameters),
  M2sConnectAck(M2sConnectAckParameters),
  M2sModDirect(M2sModDirectParameters),
  M2sModDirectAck(M2sModDirectAckParameters),
  Message(MessageParameters),
  ModDirect(ModDirectParameters),
  ModDirectAck(ModDirectAckParameters),
  Ping(PingParameters),
  Pong(PongParameters),
  S2mAuth(S2mAuthParameters),
  S2mAuthAck(S2mAuthAckParameters),
  S2mConnect(S2mConnectParameters),
  S2mConnectAck(S2mConnectAckParameters),
  S2mForwardBroadcastPayload(S2mForwardBroadcastPayloadParameters),
  S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters),
  S2mForwardEvent(S2mForwardEventParameters),
  S2mForwardEventAck(S2mForwardEventAckParameters),
  S2mModDirect(S2mModDirectParameters),
  S2mModDirectAck(S2mModDirectAckParameters),
  SetChannelAcl(SetChannelAclParameters),
  SetChannelAclAck(SetChannelAclAckParameters),
  SetChannelConfiguration(SetChannelConfigurationParameters),
  SetChannelConfigurationAck(SetChannelConfigurationAckParameters),
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct AuthParameters {
  #[param(validate = "non_empty")]
  pub token: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct AuthAckParameters {
  pub challenge: Option<StringAtom>,
  pub succeeded: Option<bool>,
  pub nid: Option<StringAtom>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct BroadcastParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub qos: Option<u8>,

  #[param(validate = "non_zero")]
  pub length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct BroadcastAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_zero")]
  pub seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ChannelAclParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  #[param(name = "type", validate = "non_empty")]
  pub r#type: StringAtom,

  pub nids: Vec<StringAtom>,

  pub page: Option<u32>,

  pub page_size: Option<u32>,

  pub total_count: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ChannelConfigurationParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub max_clients: u32,
  pub max_payload_size: u32,
  pub max_persist_messages: u32,
  pub persist: bool,
  pub message_flush_interval: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ConnectParameters {
  #[param(name = "version", validate = "non_zero")]
  pub protocol_version: u16,

  pub heartbeat_interval: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ConnectAckParameters {
  pub auth_required: bool,
  pub application_protocol: Option<StringAtom>,
  pub heartbeat_interval: u32,
  pub max_subscriptions: u32,
  pub max_message_size: u32,
  pub max_payload_size: u32,
  pub max_inflight_requests: u32,
  pub max_persist_messages: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct DeleteChannelParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct DeleteChannelAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ErrorParameters {
  pub id: Option<u32>,

  #[param(validate = "non_empty")]
  pub reason: StringAtom,

  pub detail: Option<StringAtom>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct EventParameters {
  #[param(validate = "non_empty")]
  pub kind: StringAtom,

  pub channel: Option<StringAtom>,

  pub nid: Option<StringAtom>,
  pub owner: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct GetChannelAclParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  #[param(name = "type", validate = "non_empty")]
  pub r#type: StringAtom,

  pub page: Option<u32>,

  pub page_size: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct GetChannelConfigurationParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct HistoryParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub history_id: StringAtom,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  #[param(validate = "non_zero")]
  pub from_seq: u64,

  #[param(validate = "non_zero")]
  pub limit: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct HistoryAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub history_id: StringAtom,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub count: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ChannelSeqParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ChannelSeqAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub first_seq: u64,

  pub last_seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct IdentifyParameters {
  #[param(validate = "non_empty")]
  pub username: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct IdentifyAckParameters {
  #[param(validate = "non_empty")]
  pub nid: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct JoinChannelParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub on_behalf: Option<StringAtom>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct JoinChannelAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub channel: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct LeaveChannelParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub on_behalf: Option<StringAtom>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct LeaveChannelAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ListChannelsParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub page: Option<u32>,

  pub page_size: Option<u32>,

  pub owner: bool,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ListChannelsAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub channels: Vec<StringAtom>,

  pub page: Option<u32>,

  pub page_size: Option<u32>,

  pub total_count: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ListMembersParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub page: Option<u32>,

  pub page_size: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ListMembersAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub members: Vec<StringAtom>,

  pub page: Option<u32>,

  pub page_size: Option<u32>,

  pub total_count: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct M2sConnectParameters {
  #[param(name = "version", validate = "non_zero")]
  pub protocol_version: u16,

  pub secret: Option<StringAtom>,

  pub heartbeat_interval: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct M2sConnectAckParameters {
  #[param(validate = "non_zero")]
  pub heartbeat_interval: u32,

  #[param(validate = "non_zero")]
  pub max_inflight_requests: u32,

  #[param(validate = "non_zero")]
  pub max_message_size: u32,

  #[param(validate = "non_zero")]
  pub max_payload_size: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct M2sModDirectParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub targets: Vec<StringAtom>,

  #[param(validate = "non_zero")]
  pub length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct M2sModDirectAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct MessageParameters {
  #[param(validate = "non_empty")]
  pub from: StringAtom,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  #[param(validate = "non_zero")]
  pub length: u32,

  #[param(validate = "non_zero")]
  pub seq: u64,

  #[param(validate = "non_zero")]
  pub timestamp: u64,

  pub history_id: Option<StringAtom>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ModDirectParameters {
  pub id: Option<u32>,

  #[param(validate = "non_empty")]
  pub from: StringAtom,

  #[param(validate = "non_zero")]
  pub length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct ModDirectAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct PingParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct PongParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mAuthParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub token: StringAtom,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mAuthAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub challenge: Option<StringAtom>,
  pub username: Option<StringAtom>,

  pub succeeded: bool,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mConnectParameters {
  #[param(name = "version", validate = "non_zero")]
  pub protocol_version: u16,

  pub secret: Option<StringAtom>,

  pub heartbeat_interval: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mConnectAckParameters {
  #[param(validate = "non_empty")]
  pub application_protocol: StringAtom,

  #[param(validate = "non_zero")]
  pub operation_mask: u64,

  #[param(validate = "non_zero")]
  pub heartbeat_interval: u32,

  #[param(validate = "non_zero")]
  pub max_inflight_requests: u32,

  #[param(validate = "non_zero")]
  pub max_message_size: u32,

  #[param(validate = "non_zero")]
  pub max_payload_size: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct SetChannelAclParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  #[param(name = "type", validate = "non_empty")]
  pub r#type: StringAtom,

  #[param(validate = "non_empty")]
  pub action: StringAtom,

  pub nids: Vec<StringAtom>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct SetChannelAclAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct SetChannelConfigurationParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  pub max_clients: Option<u32>,
  pub max_payload_size: Option<u32>,
  pub max_persist_messages: Option<u32>,
  pub persist: Option<bool>,
  pub message_flush_interval: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct SetChannelConfigurationAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mForwardEventParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub channel: Option<StringAtom>,

  #[param(validate = "non_empty")]
  pub kind: StringAtom,

  pub nid: Option<StringAtom>,

  pub owner: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mForwardEventAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mForwardBroadcastPayloadParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub from: StringAtom,

  #[param(validate = "non_empty")]
  pub channel: StringAtom,

  #[param(validate = "non_zero")]
  pub length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mForwardBroadcastPayloadAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub valid: bool,

  pub altered_payload: bool,

  pub altered_payload_length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mModDirectParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  #[param(validate = "non_empty")]
  pub from: StringAtom,

  #[param(validate = "non_zero")]
  pub length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, ProtocolMessageParameters)]
pub struct S2mModDirectAckParameters {
  #[param(validate = "non_zero")]
  pub id: u32,

  pub valid: bool,
}

// === impl Message ===

impl Message {
  pub fn from_name(msg_name: &[u8]) -> anyhow::Result<Message> {
    match msg_name {
      b"AUTH" => Ok(Message::Auth(AuthParameters::default())),
      b"AUTH_ACK" => Ok(Message::AuthAck(AuthAckParameters::default())),
      b"BROADCAST" => Ok(Message::Broadcast(BroadcastParameters::default())),
      b"BROADCAST_ACK" => Ok(Message::BroadcastAck(BroadcastAckParameters::default())),
      b"CONNECT" => Ok(Message::Connect(ConnectParameters::default())),
      b"CONNECT_ACK" => Ok(Message::ConnectAck(ConnectAckParameters::default())),
      b"CHAN_ACL" => Ok(Message::ChannelAcl(ChannelAclParameters::default())),
      b"CHAN_CONFIG" => Ok(Message::ChannelConfiguration(ChannelConfigurationParameters::default())),
      b"DELETE" => Ok(Message::DeleteChannel(DeleteChannelParameters::default())),
      b"DELETE_ACK" => Ok(Message::DeleteChannelAck(DeleteChannelAckParameters::default())),
      b"ERROR" => Ok(Message::Error(ErrorParameters::default())),
      b"EVENT" => Ok(Message::Event(EventParameters::default())),
      b"GET_CHAN_ACL" => Ok(Message::GetChannelAcl(GetChannelAclParameters::default())),
      b"GET_CHAN_CONFIG" => Ok(Message::GetChannelConfiguration(GetChannelConfigurationParameters::default())),
      b"HISTORY" => Ok(Message::History(HistoryParameters::default())),
      b"HISTORY_ACK" => Ok(Message::HistoryAck(HistoryAckParameters::default())),
      b"CHAN_SEQ" => Ok(Message::ChannelSeq(ChannelSeqParameters::default())),
      b"CHAN_SEQ_ACK" => Ok(Message::ChannelSeqAck(ChannelSeqAckParameters::default())),
      b"IDENTIFY" => Ok(Message::Identify(IdentifyParameters::default())),
      b"IDENTIFY_ACK" => Ok(Message::IdentifyAck(IdentifyAckParameters::default())),
      b"JOIN" => Ok(Message::JoinChannel(JoinChannelParameters::default())),
      b"JOIN_ACK" => Ok(Message::JoinChannelAck(JoinChannelAckParameters::default())),
      b"LEAVE" => Ok(Message::LeaveChannel(LeaveChannelParameters::default())),
      b"LEAVE_ACK" => Ok(Message::LeaveChannelAck(LeaveChannelAckParameters::default())),
      b"CHANNELS" => Ok(Message::ListChannels(ListChannelsParameters::default())),
      b"CHANNELS_ACK" => Ok(Message::ListChannelsAck(ListChannelsAckParameters::default())),
      b"M2S_CONNECT" => Ok(Message::M2sConnect(M2sConnectParameters::default())),
      b"M2S_CONNECT_ACK" => Ok(Message::M2sConnectAck(M2sConnectAckParameters::default())),
      b"M2S_MOD_DIRECT" => Ok(Message::M2sModDirect(M2sModDirectParameters::default())),
      b"M2S_MOD_DIRECT_ACK" => Ok(Message::M2sModDirectAck(M2sModDirectAckParameters::default())),
      b"MEMBERS" => Ok(Message::ListMembers(ListMembersParameters::default())),
      b"MEMBERS_ACK" => Ok(Message::ListMembersAck(ListMembersAckParameters::default())),
      b"MESSAGE" => Ok(Message::Message(MessageParameters::default())),
      b"MOD_DIRECT" => Ok(Message::ModDirect(ModDirectParameters::default())),
      b"MOD_DIRECT_ACK" => Ok(Message::ModDirectAck(ModDirectAckParameters::default())),
      b"PING" => Ok(Message::Ping(PingParameters::default())),
      b"PONG" => Ok(Message::Pong(PongParameters::default())),
      b"S2M_AUTH" => Ok(Message::S2mAuth(S2mAuthParameters::default())),
      b"S2M_AUTH_ACK" => Ok(Message::S2mAuthAck(S2mAuthAckParameters::default())),
      b"S2M_CONNECT" => Ok(Message::S2mConnect(S2mConnectParameters::default())),
      b"S2M_CONNECT_ACK" => Ok(Message::S2mConnectAck(S2mConnectAckParameters::default())),
      b"S2M_FORWARD_EVENT" => Ok(Message::S2mForwardEvent(S2mForwardEventParameters::default())),
      b"S2M_FORWARD_EVENT_ACK" => Ok(Message::S2mForwardEventAck(S2mForwardEventAckParameters::default())),
      b"S2M_FORWARD_BROADCAST_PAYLOAD" => {
        Ok(Message::S2mForwardBroadcastPayload(S2mForwardBroadcastPayloadParameters::default()))
      },
      b"S2M_FORWARD_BROADCAST_PAYLOAD_ACK" => {
        Ok(Message::S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters::default()))
      },
      b"S2M_MOD_DIRECT" => Ok(Message::S2mModDirect(S2mModDirectParameters::default())),
      b"S2M_MOD_DIRECT_ACK" => Ok(Message::S2mModDirectAck(S2mModDirectAckParameters::default())),
      b"SET_CHAN_ACL" => Ok(Message::SetChannelAcl(SetChannelAclParameters::default())),
      b"SET_CHAN_ACL_ACK" => Ok(Message::SetChannelAclAck(SetChannelAclAckParameters::default())),
      b"SET_CHAN_CONFIG" => Ok(Message::SetChannelConfiguration(SetChannelConfigurationParameters::default())),
      b"SET_CHAN_CONFIG_ACK" => {
        Ok(Message::SetChannelConfigurationAck(SetChannelConfigurationAckParameters::default()))
      },
      _ => Err(anyhow::anyhow!("unknown message")),
    }
  }

  pub fn name(&self) -> &'static str {
    match self {
      Message::Auth { .. } => "AUTH",
      Message::AuthAck { .. } => "AUTH_ACK",
      Message::Broadcast { .. } => "BROADCAST",
      Message::BroadcastAck { .. } => "BROADCAST_ACK",
      Message::Connect { .. } => "CONNECT",
      Message::ConnectAck { .. } => "CONNECT_ACK",
      Message::ChannelAcl { .. } => "CHAN_ACL",
      Message::ChannelConfiguration { .. } => "CHAN_CONFIG",
      Message::DeleteChannel { .. } => "DELETE",
      Message::DeleteChannelAck { .. } => "DELETE_ACK",
      Message::Error { .. } => "ERROR",
      Message::Event { .. } => "EVENT",
      Message::GetChannelAcl { .. } => "GET_CHAN_ACL",
      Message::GetChannelConfiguration { .. } => "GET_CHAN_CONFIG",
      Message::History { .. } => "HISTORY",
      Message::HistoryAck { .. } => "HISTORY_ACK",
      Message::ChannelSeq { .. } => "CHAN_SEQ",
      Message::ChannelSeqAck { .. } => "CHAN_SEQ_ACK",
      Message::Identify { .. } => "IDENTIFY",
      Message::IdentifyAck { .. } => "IDENTIFY_ACK",
      Message::JoinChannel { .. } => "JOIN",
      Message::JoinChannelAck { .. } => "JOIN_ACK",
      Message::LeaveChannel { .. } => "LEAVE",
      Message::LeaveChannelAck { .. } => "LEAVE_ACK",
      Message::ListChannels { .. } => "CHANNELS",
      Message::ListChannelsAck { .. } => "CHANNELS_ACK",
      Message::M2sConnect { .. } => "M2S_CONNECT",
      Message::M2sConnectAck { .. } => "M2S_CONNECT_ACK",
      Message::ListMembers { .. } => "MEMBERS",
      Message::ListMembersAck { .. } => "MEMBERS_ACK",
      Message::M2sModDirect { .. } => "M2S_MOD_DIRECT",
      Message::M2sModDirectAck { .. } => "M2S_MOD_DIRECT_ACK",
      Message::Message { .. } => "MESSAGE",
      Message::ModDirect { .. } => "MOD_DIRECT",
      Message::ModDirectAck { .. } => "MOD_DIRECT_ACK",
      Message::Ping { .. } => "PING",
      Message::Pong { .. } => "PONG",
      Message::S2mAuth { .. } => "S2M_AUTH",
      Message::S2mAuthAck { .. } => "S2M_AUTH_ACK",
      Message::S2mConnect { .. } => "S2M_CONNECT",
      Message::S2mConnectAck { .. } => "S2M_CONNECT_ACK",
      Message::S2mForwardEvent { .. } => "S2M_FORWARD_EVENT",
      Message::S2mForwardEventAck { .. } => "S2M_FORWARD_EVENT_ACK",
      Message::S2mForwardBroadcastPayload { .. } => "S2M_FORWARD_BROADCAST_PAYLOAD",
      Message::S2mForwardBroadcastPayloadAck { .. } => "S2M_FORWARD_BROADCAST_PAYLOAD_ACK",
      Message::S2mModDirect { .. } => "S2M_MOD_DIRECT",
      Message::S2mModDirectAck { .. } => "S2M_MOD_DIRECT_ACK",
      Message::SetChannelAcl { .. } => "SET_CHAN_ACL",
      Message::SetChannelAclAck { .. } => "SET_CHAN_ACL_ACK",
      Message::SetChannelConfiguration { .. } => "SET_CHAN_CONFIG",
      Message::SetChannelConfigurationAck { .. } => "SET_CHAN_CONFIG_ACK",
    }
  }

  /// Encodes the parameters of the message.
  /// The number of bytes written is returned.
  pub fn encode_parameters(&self, parameter_writer: &mut ParameterWriter) -> Result<usize, SerializeError> {
    use crate::Message::*;

    match self {
      Auth(params) => params.encode(parameter_writer),
      AuthAck(params) => params.encode(parameter_writer),
      Broadcast(params) => params.encode(parameter_writer),
      BroadcastAck(params) => params.encode(parameter_writer),
      Connect(params) => params.encode(parameter_writer),
      ConnectAck(params) => params.encode(parameter_writer),
      ChannelAcl(params) => params.encode(parameter_writer),
      ChannelConfiguration(params) => params.encode(parameter_writer),
      DeleteChannel(params) => params.encode(parameter_writer),
      DeleteChannelAck(params) => params.encode(parameter_writer),
      Error(params) => params.encode(parameter_writer),
      Event(params) => params.encode(parameter_writer),
      GetChannelAcl(params) => params.encode(parameter_writer),
      GetChannelConfiguration(params) => params.encode(parameter_writer),
      History(params) => params.encode(parameter_writer),
      HistoryAck(params) => params.encode(parameter_writer),
      ChannelSeq(params) => params.encode(parameter_writer),
      ChannelSeqAck(params) => params.encode(parameter_writer),
      Identify(params) => params.encode(parameter_writer),
      IdentifyAck(params) => params.encode(parameter_writer),
      JoinChannel(params) => params.encode(parameter_writer),
      JoinChannelAck(params) => params.encode(parameter_writer),
      LeaveChannel(params) => params.encode(parameter_writer),
      LeaveChannelAck(params) => params.encode(parameter_writer),
      ListChannels(params) => params.encode(parameter_writer),
      ListChannelsAck(params) => params.encode(parameter_writer),
      ListMembers(params) => params.encode(parameter_writer),
      ListMembersAck(params) => params.encode(parameter_writer),
      M2sConnect(params) => params.encode(parameter_writer),
      M2sConnectAck(params) => params.encode(parameter_writer),
      M2sModDirect(params) => params.encode(parameter_writer),
      M2sModDirectAck(params) => params.encode(parameter_writer),
      Message(params) => params.encode(parameter_writer),
      ModDirect(params) => params.encode(parameter_writer),
      ModDirectAck(params) => params.encode(parameter_writer),
      Ping(params) => params.encode(parameter_writer),
      Pong(params) => params.encode(parameter_writer),
      S2mAuth(params) => params.encode(parameter_writer),
      S2mAuthAck(params) => params.encode(parameter_writer),
      S2mConnect(params) => params.encode(parameter_writer),
      S2mConnectAck(params) => params.encode(parameter_writer),
      S2mForwardEvent(params) => params.encode(parameter_writer),
      S2mForwardEventAck(params) => params.encode(parameter_writer),
      S2mForwardBroadcastPayload(params) => params.encode(parameter_writer),
      S2mForwardBroadcastPayloadAck(params) => params.encode(parameter_writer),
      S2mModDirect(params) => params.encode(parameter_writer),
      S2mModDirectAck(params) => params.encode(parameter_writer),
      SetChannelAcl(params) => params.encode(parameter_writer),
      SetChannelAclAck(params) => params.encode(parameter_writer),
      SetChannelConfiguration(params) => params.encode(parameter_writer),
      SetChannelConfigurationAck(params) => params.encode(parameter_writer),
    }
  }

  /// Decodes the parameters of the message.
  pub fn decode_parameters(&mut self, parameter_reader: &mut ParameterReader) -> anyhow::Result<()> {
    use crate::Message::*;

    match self {
      Auth(params) => params.decode(parameter_reader),
      AuthAck(params) => params.decode(parameter_reader),
      Broadcast(params) => params.decode(parameter_reader),
      BroadcastAck(params) => params.decode(parameter_reader),
      ChannelAcl(params) => params.decode(parameter_reader),
      ChannelConfiguration(params) => params.decode(parameter_reader),
      Connect(params) => params.decode(parameter_reader),
      ConnectAck(params) => params.decode(parameter_reader),
      DeleteChannel(params) => params.decode(parameter_reader),
      DeleteChannelAck(params) => params.decode(parameter_reader),
      Error(params) => params.decode(parameter_reader),
      Event(params) => params.decode(parameter_reader),
      GetChannelAcl(params) => params.decode(parameter_reader),
      GetChannelConfiguration(params) => params.decode(parameter_reader),
      History(params) => params.decode(parameter_reader),
      HistoryAck(params) => params.decode(parameter_reader),
      ChannelSeq(params) => params.decode(parameter_reader),
      ChannelSeqAck(params) => params.decode(parameter_reader),
      Identify(params) => params.decode(parameter_reader),
      IdentifyAck(params) => params.decode(parameter_reader),
      JoinChannel(params) => params.decode(parameter_reader),
      JoinChannelAck(params) => params.decode(parameter_reader),
      LeaveChannel(params) => params.decode(parameter_reader),
      LeaveChannelAck(params) => params.decode(parameter_reader),
      ListChannels(params) => params.decode(parameter_reader),
      ListChannelsAck(params) => params.decode(parameter_reader),
      ListMembers(params) => params.decode(parameter_reader),
      ListMembersAck(params) => params.decode(parameter_reader),
      M2sConnect(params) => params.decode(parameter_reader),
      M2sConnectAck(params) => params.decode(parameter_reader),
      M2sModDirect(params) => params.decode(parameter_reader),
      M2sModDirectAck(params) => params.decode(parameter_reader),
      Message(params) => params.decode(parameter_reader),
      ModDirect(params) => params.decode(parameter_reader),
      ModDirectAck(params) => params.decode(parameter_reader),
      Ping(params) => params.decode(parameter_reader),
      Pong(params) => params.decode(parameter_reader),
      S2mAuth(params) => params.decode(parameter_reader),
      S2mAuthAck(params) => params.decode(parameter_reader),
      S2mConnect(params) => params.decode(parameter_reader),
      S2mConnectAck(params) => params.decode(parameter_reader),
      S2mForwardEvent(params) => params.decode(parameter_reader),
      S2mForwardEventAck(params) => params.decode(parameter_reader),
      S2mForwardBroadcastPayload(params) => params.decode(parameter_reader),
      S2mForwardBroadcastPayloadAck(params) => params.decode(parameter_reader),
      S2mModDirect(params) => params.decode(parameter_reader),
      S2mModDirectAck(params) => params.decode(parameter_reader),
      SetChannelAcl(params) => params.decode(parameter_reader),
      SetChannelAclAck(params) => params.decode(parameter_reader),
      SetChannelConfiguration(params) => params.decode(parameter_reader),
      SetChannelConfigurationAck(params) => params.decode(parameter_reader),
    }
  }

  /// Validates the parameters of the message.
  pub fn validate_parameters(&self) -> anyhow::Result<()> {
    use crate::Message::*;

    match self {
      Auth(params) => params.validate(),
      AuthAck(params) => params.validate(),
      Broadcast(params) => {
        params.validate()?;

        params.qos.map(crate::qos::QoS::try_from).transpose()?;

        Ok(())
      },
      BroadcastAck(params) => params.validate(),
      ChannelAcl(params) => {
        params.validate()?;

        // Validate that type enum is valid
        AclType::from_str(params.r#type.as_ref())?;

        Ok(())
      },
      ChannelConfiguration(params) => params.validate(),
      Connect(params) => params.validate(),
      ConnectAck(params) => params.validate(),
      DeleteChannel(params) => params.validate(),
      DeleteChannelAck(params) => params.validate(),
      Error(params) => {
        params.validate()?;

        // Validate that the error reason is valid
        ErrorReason::from_str(params.reason.as_ref())?;

        Ok(())
      },
      Event(params) => {
        params.validate()?;

        // Validate that the event kind is valid
        EventKind::from_str(params.kind.as_ref())?;

        Ok(())
      },
      GetChannelAcl(params) => {
        params.validate()?;

        // Validate that type enum is valid
        AclType::from_str(params.r#type.as_ref())?;

        Ok(())
      },
      GetChannelConfiguration(params) => params.validate(),
      History(params) => params.validate(),
      HistoryAck(params) => params.validate(),
      ChannelSeq(params) => params.validate(),
      ChannelSeqAck(params) => params.validate(),
      Identify(params) => params.validate(),
      IdentifyAck(params) => params.validate(),
      JoinChannel(params) => params.validate(),
      JoinChannelAck(params) => params.validate(),
      LeaveChannel(params) => params.validate(),
      LeaveChannelAck(params) => params.validate(),
      ListChannels(params) => params.validate(),
      ListChannelsAck(params) => params.validate(),
      ListMembers(params) => params.validate(),
      ListMembersAck(params) => params.validate(),
      M2sConnect(params) => params.validate(),
      M2sConnectAck(params) => params.validate(),
      M2sModDirect(params) => params.validate(),
      M2sModDirectAck(params) => params.validate(),
      Message(params) => params.validate(),
      ModDirect(params) => params.validate(),
      ModDirectAck(params) => params.validate(),
      Ping(params) => params.validate(),
      Pong(params) => params.validate(),
      S2mAuth(params) => params.validate(),
      S2mAuthAck(params) => params.validate(),
      S2mConnect(params) => params.validate(),
      S2mConnectAck(params) => params.validate(),
      S2mForwardEvent(params) => params.validate(),
      S2mForwardEventAck(params) => params.validate(),
      S2mForwardBroadcastPayload(params) => params.validate(),
      S2mForwardBroadcastPayloadAck(params) => params.validate(),
      S2mModDirect(params) => params.validate(),
      S2mModDirectAck(params) => params.validate(),
      SetChannelAcl(params) => {
        params.validate()?;

        // Validate that type and action enum values are valid
        AclType::from_str(params.r#type.as_ref())?;
        AclAction::from_str(params.action.as_ref())?;

        Ok(())
      },
      SetChannelAclAck(params) => params.validate(),
      SetChannelConfiguration(params) => params.validate(),
      SetChannelConfigurationAck(params) => params.validate(),
    }
  }

  /// Returns information about the payload associated with the message.
  /// If the message does not have an associated payload, returns `None`.
  pub fn payload_info(&self) -> Option<PayloadInfo> {
    use crate::Message::*;

    match self {
      Broadcast(params) => Some(PayloadInfo { id: Some(params.id), length: params.length as usize }),
      ModDirect(params) => Some(PayloadInfo { id: params.id, length: params.length as usize }),
      M2sModDirect(params) => Some(PayloadInfo { id: Some(params.id), length: params.length as usize }),
      S2mModDirect(params) => Some(PayloadInfo { id: Some(params.id), length: params.length as usize }),
      S2mForwardBroadcastPayload(params) => Some(PayloadInfo { id: Some(params.id), length: params.length as usize }),
      S2mForwardBroadcastPayloadAck(params) => {
        if params.altered_payload {
          return Some(PayloadInfo { id: Some(params.id), length: params.altered_payload_length as usize });
        }
        None
      },
      Message(params) => Some(PayloadInfo { id: None, length: params.length as usize }),
      _ => None, // Other messages do not have an associated payload.
    }
  }

  /// Gets the correlation ID from the message if present.
  pub fn correlation_id(&self) -> Option<u32> {
    match self {
      // messages with required id field
      Message::Broadcast(params) => Some(params.id),
      Message::BroadcastAck(params) => Some(params.id),
      Message::ChannelAcl(params) => Some(params.id),
      Message::ChannelConfiguration(params) => Some(params.id),
      Message::DeleteChannel(params) => Some(params.id),
      Message::DeleteChannelAck(params) => Some(params.id),
      Message::GetChannelAcl(params) => Some(params.id),
      Message::GetChannelConfiguration(params) => Some(params.id),
      Message::History(params) => Some(params.id),
      Message::HistoryAck(params) => Some(params.id),
      Message::ChannelSeq(params) => Some(params.id),
      Message::ChannelSeqAck(params) => Some(params.id),
      Message::JoinChannel(params) => Some(params.id),
      Message::JoinChannelAck(params) => Some(params.id),
      Message::LeaveChannel(params) => Some(params.id),
      Message::LeaveChannelAck(params) => Some(params.id),
      Message::ListChannels(params) => Some(params.id),
      Message::ListChannelsAck(params) => Some(params.id),
      Message::ListMembers(params) => Some(params.id),
      Message::ListMembersAck(params) => Some(params.id),
      Message::M2sModDirect(params) => Some(params.id),
      Message::M2sModDirectAck(params) => Some(params.id),
      Message::ModDirectAck(params) => Some(params.id),
      Message::Ping(params) => Some(params.id),
      Message::Pong(params) => Some(params.id),
      Message::SetChannelAcl(params) => Some(params.id),
      Message::SetChannelAclAck(params) => Some(params.id),
      Message::SetChannelConfiguration(params) => Some(params.id),
      Message::SetChannelConfigurationAck(params) => Some(params.id),
      Message::S2mAuth(params) => Some(params.id),
      Message::S2mAuthAck(params) => Some(params.id),
      Message::S2mForwardEvent(params) => Some(params.id),
      Message::S2mForwardEventAck(params) => Some(params.id),
      Message::S2mForwardBroadcastPayload(params) => Some(params.id),
      Message::S2mForwardBroadcastPayloadAck(params) => Some(params.id),
      Message::S2mModDirect(params) => Some(params.id),
      Message::S2mModDirectAck(params) => Some(params.id),

      // messages with optional id
      Message::Error(params) => params.id,
      Message::ModDirect(params) => params.id,

      _ => None,
    }
  }
}
