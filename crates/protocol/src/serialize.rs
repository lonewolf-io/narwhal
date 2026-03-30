// SPDX-License-Identifier: BSD-3-Clause

use std::fmt;
use std::io::{Cursor, Write};
use std::ops::Deref;

use crate::message::Message;

use narwhal_util::string_atom::StringAtom;

const ESC_CHAR: [char; 4] = ['"', '\'', ':', '*'];

/// Errors that can occur during message serialization.
#[derive(Debug)]
pub enum SerializeError {
  /// Message exceeds maximum size limit.
  MessageTooLarge,
  /// Other serialization errors.
  Other(anyhow::Error),
}

impl std::fmt::Display for SerializeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      SerializeError::MessageTooLarge => write!(f, "message exceeds maximum size limit"),
      SerializeError::Other(e) => write!(f, "{}", e),
    }
  }
}

impl std::error::Error for SerializeError {}

impl From<anyhow::Error> for SerializeError {
  fn from(e: anyhow::Error) -> Self {
    SerializeError::Other(e)
  }
}

impl From<std::io::Error> for SerializeError {
  fn from(e: std::io::Error) -> Self {
    SerializeError::Other(e.into())
  }
}

impl From<std::str::Utf8Error> for SerializeError {
  fn from(e: std::str::Utf8Error) -> Self {
    SerializeError::Other(e.into())
  }
}

/// Helper function to write formatted output to a cursor, converting WriteZero errors to MessageTooLarge.
fn write_fmt_to_cursor(c: &mut Cursor<&mut [u8]>, args: fmt::Arguments) -> Result<(), SerializeError> {
  use std::io::ErrorKind;
  c.write_fmt(args).map_err(|e| {
    if e.kind() == ErrorKind::WriteZero { SerializeError::MessageTooLarge } else { SerializeError::from(e) }
  })
}

/// This trait is used to format a parameter value for a message.
pub trait ParameterValueDisplay<'a>: fmt::Display {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError>;
}

impl<'a> ParameterValueDisplay<'a> for &str {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    if self.is_empty() {
      return write_fmt_to_cursor(c, format_args!("\\\"\\\""));
    }
    // If the value doesn't contain any whitespace, we can write it directly.
    if self.find(|c| [' ', '\t', '\x0b', '\x0c', '\r'].contains(&c)).is_none() {
      return write_fmt_to_cursor(c, format_args!("{}", self));
    }
    for esc_char in ESC_CHAR.iter() {
      if self.find(*esc_char).is_some() {
        continue;
      }
      return write_fmt_to_cursor(c, format_args!("\\{}{}\\{}", esc_char, self, esc_char));
    }
    Err(SerializeError::Other(anyhow::anyhow!("unable to escape parameter value")))
  }
}

impl<'a> ParameterValueDisplay<'a> for StringAtom {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    self.deref().fmt_param(c)
  }
}

impl<'a> ParameterValueDisplay<'a> for u8 {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    write_fmt_to_cursor(c, format_args!("{}", self))
  }
}

impl<'a> ParameterValueDisplay<'a> for u16 {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    write_fmt_to_cursor(c, format_args!("{}", self))
  }
}

impl<'a> ParameterValueDisplay<'a> for u32 {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    write_fmt_to_cursor(c, format_args!("{}", self))
  }
}

impl<'a> ParameterValueDisplay<'a> for u64 {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    write_fmt_to_cursor(c, format_args!("{}", self))
  }
}

impl<'a> ParameterValueDisplay<'a> for bool {
  fn fmt_param(&self, c: &mut Cursor<&'a mut [u8]>) -> Result<(), SerializeError> {
    write_fmt_to_cursor(c, format_args!("{}", self))
  }
}

/// Writes message parameters to a byte buffer.
pub struct ParameterWriter<'a> {
  /// The cursor to write to.
  c: Cursor<&'a mut [u8]>,
}

// === impl ParameterWriter ===

impl<'a> ParameterWriter<'a> {
  fn new(c: Cursor<&'a mut [u8]>) -> Self {
    Self { c }
  }

  pub fn write_param<T: ParameterValueDisplay<'a>>(&mut self, name: &[u8], value: &T) -> Result<usize, SerializeError> {
    let pos = self.c.position() as usize;
    write_fmt_to_cursor(&mut self.c, format_args!(" {}=", std::str::from_utf8(name)?))?;
    value.fmt_param(&mut self.c)?;
    let n = (self.c.position() as usize) - pos;
    Ok(n)
  }

  pub fn write_param_slice<T: ParameterValueDisplay<'a>>(
    &mut self,
    name: &[u8],
    values: &[T],
  ) -> Result<usize, SerializeError> {
    let val_len = values.len();

    // Don't write anything if the slice is empty.
    if val_len == 0 {
      return Ok(0);
    }

    let pos = self.c.position() as usize;
    write_fmt_to_cursor(&mut self.c, format_args!(" {}:{}", std::str::from_utf8(name)?, val_len))?;

    if val_len > 0 {
      write_fmt_to_cursor(&mut self.c, format_args!("="))?;

      let mut first = true;
      for value in values {
        if !first {
          write_fmt_to_cursor(&mut self.c, format_args!(" "))?;
        }
        value.fmt_param(&mut self.c)?;
        first = false;
      }
    }
    let n = (self.c.position() as usize) - pos;
    Ok(n)
  }
}

/// Serializes a message to a byte buffer.
pub fn serialize(msg: &Message, out: &mut [u8]) -> Result<usize, SerializeError> {
  let mut n = 0;
  let mut c = Cursor::new(out);

  n += write_bytes(msg.name().as_bytes(), &mut c)?;
  {
    let param_buf = &mut c.get_mut()[n..];
    let mut param_writer = ParameterWriter::new(Cursor::new(param_buf));
    n += msg.encode_parameters(&mut param_writer)?;
    c.set_position(n as u64);
  }
  n += write_bytes(b"\n", &mut c)?;
  Ok(n)
}

/// Writes bytes to a cursor.
fn write_bytes(bytes: &[u8], c: &mut Cursor<&mut [u8]>) -> Result<usize, SerializeError> {
  let n = c.write(bytes)?;
  if n < bytes.len() {
    return Err(SerializeError::MessageTooLarge);
  }
  Ok(n)
}

#[cfg(test)]
mod tests {
  use crate::*;

  const DEFAULT_OUT_BUFF_SIZE: usize = 1024;

  #[test]
  fn test_serialize() {
    struct TestCase {
      name: &'static str,
      msg: Message,
      expected_out: Option<String>,
    }

    let test_cases = vec![
            TestCase {
                name: "AUTH",
                msg: Message::Auth(AuthParameters { token: "a_token".into() }),
                expected_out: Some("AUTH token=a_token\n".to_string()),
            },
            TestCase {
                name: "AUTH_ACK",
                msg: Message::AuthAck(AuthAckParameters { succeeded: Some(true), challenge: Some("challenge".into()), nid: Some("test_user@localhost".into()) }),
                expected_out: Some("AUTH_ACK challenge=challenge nid=test_user@localhost succeeded=true\n".to_string()),
            },
            TestCase {
                name: "BROADCAST",
                msg: Message::Broadcast(BroadcastParameters { id: 1, channel: "!1@localhost".into(), qos: Some(1), length: 10 }),
                expected_out: Some("BROADCAST id=1 channel=!1@localhost length=10 qos=1\n".to_string()),
            },
            TestCase {
                name: "BROADCAST_ACK",
                msg: Message::BroadcastAck(BroadcastAckParameters { id: 1, seq: 1 }),
                expected_out: Some("BROADCAST_ACK id=1 seq=1\n".to_string()),
            },
            TestCase {
                name: "CHAN_ACL",
                msg: Message::ChannelAcl(ChannelAclParameters {
                    id: 1,
                    channel: "!1@localhost".into(),
                    r#type: "publish".into(),
                    nids: Vec::from(["test_user_1@localhost".into(), "example.com".into()].as_slice()),
                    page: None,
                    page_size: None,
                    total_count: None,
                }),
                expected_out: Some("CHAN_ACL id=1 channel=!1@localhost nids:2=test_user_1@localhost example.com type=publish\n".to_string()),
            },
            TestCase {
                name: "CHAN_CONFIG",
                msg: Message::ChannelConfiguration(ChannelConfigurationParameters {
                    id: 1,
                    channel: "!1@localhost".into(),
                    max_clients: 100,
                    max_payload_size: 16,
                    ..Default::default()
                }),
                expected_out: Some("CHAN_CONFIG id=1 channel=!1@localhost max_clients=100 max_payload_size=16 max_persist_messages=0 message_flush_interval=0 persist=false\n".to_string()),
            },
            TestCase {
                name: "CONNECT",
                msg: Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 30 }),
                expected_out: Some("CONNECT heartbeat_interval=30 version=1\n".to_string()),
            },
            TestCase {
                name: "CONNECT_ACK",
                msg: Message::ConnectAck(ConnectAckParameters { auth_required: true, application_protocol: Some("NCP/1.0".into()), max_inflight_requests: 100, heartbeat_interval: 20000, max_message_size: 8192, max_payload_size: 262144, max_subscriptions: 100, ..Default::default() }),
                expected_out: Some(
                    "CONNECT_ACK application_protocol=NCP/1.0 auth_required=true heartbeat_interval=20000 max_inflight_requests=100 max_message_size=8192 max_payload_size=262144 max_subscriptions=100\n".to_string(),
                ),
            },
            TestCase {
                name: "ERROR",
                msg: Message::Error(ErrorParameters {
                    id: Some(1234),
                    reason: "BAD_REQUEST".into(),
                    detail: Some("bad request".into()),
                }),
                expected_out: Some("ERROR id=1234 detail=\\\"bad request\\\" reason=BAD_REQUEST\n".to_string()),
            },
            TestCase {
                name: "EVENT",
                msg: Message::Event(EventParameters { kind: "MEMBER_JOINED".into(), channel: Some("!1@localhost".into()), nid: Some("test_user@localhost".into()), owner: Some(true) }),
                expected_out: Some("EVENT channel=!1@localhost kind=MEMBER_JOINED nid=test_user@localhost owner=true\n".to_string()),
            },
            TestCase {
                name: "GET_CHAN_ACL",
                msg: Message::GetChannelAcl(GetChannelAclParameters { id: 1, channel: "!1@localhost".into(), r#type: "publish".into(), page: Some(2), page_size: Some(10) }),
                expected_out: Some("GET_CHAN_ACL id=1 channel=!1@localhost page=2 page_size=10 type=publish\n".to_string()),
            },
            TestCase {
                name: "GET_CHAN_CONFIG",
                msg: Message::GetChannelConfiguration(GetChannelConfigurationParameters { id: 1, channel: "!1@localhost".into() }),
                expected_out: Some("GET_CHAN_CONFIG id=1 channel=!1@localhost\n".to_string()),
            },
            TestCase {
                name: "IDENTIFY",
                msg: Message::Identify(IdentifyParameters { username: "test".into() }),
                expected_out: Some("IDENTIFY username=test\n".to_string()),
            },
            TestCase {
                name: "IDENTIFY_ACK",
                msg: Message::IdentifyAck(IdentifyAckParameters { nid: "test_user@localhost".into() }),
                expected_out: Some("IDENTIFY_ACK nid=test_user@localhost\n".to_string()),
            },
            TestCase {
                name: "JOIN",
                msg: Message::JoinChannel(JoinChannelParameters { id: 1, channel: "!1@localhost".into(), on_behalf: Some("hamlet@domain.com".into()) }),
                expected_out: Some("JOIN id=1 channel=!1@localhost on_behalf=hamlet@domain.com\n".to_string()),
            },
            TestCase {
                name: "JOIN_ACK",
                msg: Message::JoinChannelAck(JoinChannelAckParameters { id: 1, channel: "!1@localhost".into() }),
                expected_out: Some("JOIN_ACK id=1 channel=!1@localhost\n".to_string()),
            },
            TestCase {
                name: "LEAVE",
                msg: Message::LeaveChannel(LeaveChannelParameters { id: 1, channel: "!1@localhost".into(), on_behalf: Some("test_user@localhost".into())}),
                expected_out: Some("LEAVE id=1 channel=!1@localhost on_behalf=test_user@localhost\n".to_string()),
            },
            TestCase {
                name: "LEAVE_ACK",
                msg: Message::LeaveChannelAck(LeaveChannelAckParameters { id: 1 }),
                expected_out: Some("LEAVE_ACK id=1\n".to_string()),
            },
            TestCase {
                name: "CHANNELS",
                msg: Message::ListChannels(ListChannelsParameters { id: 1, page_size: Some(10), page: Some(1), owner: true }),
                expected_out: Some("CHANNELS id=1 owner=true page=1 page_size=10\n".to_string()),
            },
            TestCase {
                name: "CHANNELS_ACK",
                msg: Message::ListChannelsAck(ListChannelsAckParameters { id: 1, channels: Vec::from(["!1@localhost".into(), "!2@localhost".into()].as_slice()), page: Some(1), page_size: Some(2), total_count: Some(2) }),
                expected_out: Some("CHANNELS_ACK id=1 channels:2=!1@localhost !2@localhost page=1 page_size=2 total_count=2\n".to_string()),
            },
            TestCase {
                name: "MEMBERS",
                msg: Message::ListMembers(ListMembersParameters { id: 1, channel: "!1@localhost".into(), page: None, page_size: None }),
                expected_out: Some("MEMBERS id=1 channel=!1@localhost\n".to_string()),
            },
            TestCase {
                name: "MEMBERS_ACK",
                msg: Message::ListMembersAck(ListMembersAckParameters { id: 1, channel: "!1@localhost".into(), members: Vec::from(["test_user@localhost".into()].as_slice()), page: None, page_size: None, total_count: None }),
                expected_out: Some("MEMBERS_ACK id=1 channel=!1@localhost members:1=test_user@localhost\n".to_string()),
            },
            TestCase {
                name: "M2S_MOD_DIRECT",
                msg: Message::M2sModDirect(M2sModDirectParameters { id: 1, targets: Vec::from(["test_user".into()].as_slice()), length: 10 }),
                expected_out: Some("M2S_MOD_DIRECT id=1 length=10 targets:1=test_user\n".to_string()),
            },
            TestCase {
                name: "M2S_MOD_DIRECT_ACK",
                msg: Message::M2sModDirectAck(M2sModDirectAckParameters { id: 1 }),
                expected_out: Some("M2S_MOD_DIRECT_ACK id=1\n".to_string()),
            },
            TestCase {
                name: "MESSAGE",
                msg: Message::Message(MessageParameters { from: "test_user@localhost".into(), channel: "!1@localhost".into(), length: 10, seq: 1, timestamp: 1 }),
                expected_out: Some("MESSAGE channel=!1@localhost from=test_user@localhost length=10 seq=1 timestamp=1\n".to_string()),
            },
            TestCase {
                name: "MOD_DIRECT",
                msg: Message::ModDirect(ModDirectParameters { id: Some(1), from: "test_user@localhost".into(), length: 10 }),
                expected_out: Some("MOD_DIRECT id=1 from=test_user@localhost length=10\n".to_string()),
            },
            TestCase {
                name: "MOD_DIRECT_ACK",
                msg: Message::ModDirectAck(ModDirectAckParameters { id: 2 }),
                expected_out: Some("MOD_DIRECT_ACK id=2\n".to_string()),
            },
            TestCase {
                name: "PING",
                msg: Message::Ping(PingParameters { id: 1234 }),
                expected_out: Some("PING id=1234\n".to_string()),
            },
            TestCase {
                name: "PONG",
                msg: Message::Pong(PongParameters { id: 1234 }),
                expected_out: Some("PONG id=1234\n".to_string()),
            },
            TestCase {
                name: "SET_CHAN_ACL_ACK",
                msg: Message::SetChannelAclAck(SetChannelAclAckParameters { id: 1 }),
                expected_out: Some("SET_CHAN_ACL_ACK id=1\n".to_string()),
            },
            TestCase {
                name: "SET_CHAN_CONFIG_ACK",
                msg: Message::SetChannelConfigurationAck(SetChannelConfigurationAckParameters { id: 1 }),
                expected_out: Some("SET_CHAN_CONFIG_ACK id=1\n".to_string()),
            },
            TestCase {
                name: "S2M_AUTH",
                msg: Message::S2mAuth(S2mAuthParameters { id: 1, token: "a_token".into() }),
                expected_out: Some("S2M_AUTH id=1 token=a_token\n".to_string()),
            },
            TestCase {
                name: "S2M_AUTH_ACK",
                msg: Message::S2mAuthAck(S2mAuthAckParameters { id: 1, challenge: Some("a_challenge".into()), username: Some("test_user".into()), succeeded: true }),
                expected_out: Some("S2M_AUTH_ACK id=1 challenge=a_challenge succeeded=true username=test_user\n".to_string()),
            },
            TestCase {
                name: "S2M_CONNECT",
                msg: Message::S2mConnect(S2mConnectParameters { protocol_version: 1, secret: Some("a_secret".into()), heartbeat_interval: 100 }),
                expected_out: Some("S2M_CONNECT heartbeat_interval=100 secret=a_secret version=1\n".to_string()),
            },
            TestCase {
                name: "S2M_CONNECT_ACK",
                msg: Message::S2mConnectAck(S2mConnectAckParameters { application_protocol: "my-proto/1.0".into(), operations: Vec::from(["auth".into()].as_slice()), heartbeat_interval: 20000, max_inflight_requests: 100, max_message_size: 8192, max_payload_size: 262144 }),
                expected_out: Some("S2M_CONNECT_ACK application_protocol=my-proto/1.0 heartbeat_interval=20000 max_inflight_requests=100 max_message_size=8192 max_payload_size=262144 operations:1=auth\n".to_string()),
            },
            TestCase {
                name: "S2M_FORWARD_EVENT",
                msg: Message::S2mForwardEvent(S2mForwardEventParameters { id: 1, kind: "MEMBER_JOINED".into(), channel: Some("!1@localhost".into()), nid: Some("test_user@localhost".into()), owner: Some(true) }),
                expected_out: Some("S2M_FORWARD_EVENT id=1 channel=!1@localhost kind=MEMBER_JOINED nid=test_user@localhost owner=true\n".to_string()),
            },
            TestCase {
                name: "S2M_FORWARD_EVENT_ACK",
                msg: Message::S2mForwardEventAck(S2mForwardEventAckParameters { id: 1 }),
                expected_out: Some("S2M_FORWARD_EVENT_ACK id=1\n".to_string()),
            },
            TestCase {
                name: "S2M_FORWARD_BROADCAST_PAYLOAD",
                msg: Message::S2mForwardBroadcastPayload(S2mForwardBroadcastPayloadParameters { id: 1, from: "ortuman@localhost".into(), channel: "abc123".into(), length: 12 }),
                expected_out: Some("S2M_FORWARD_BROADCAST_PAYLOAD id=1 channel=abc123 from=ortuman@localhost length=12\n".to_string()),
            },
            TestCase {
                name: "S2M_FORWARD_BROADCAST_PAYLOAD_ACK",
                msg: Message::S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters { id: 1, valid: true, altered_payload: false, altered_payload_length: 0 }),
                expected_out: Some("S2M_FORWARD_BROADCAST_PAYLOAD_ACK id=1 altered_payload=false altered_payload_length=0 valid=true\n".to_string()),
            },
            TestCase {
                name: "S2M_MOD_DIRECT",
                msg: Message::S2mModDirect(S2mModDirectParameters { id: 1, from: "ortuman@localhost".into(), length: 12 }),
                expected_out: Some("S2M_MOD_DIRECT id=1 from=ortuman@localhost length=12\n".to_string()),
            },
            TestCase {
                name: "S2M_MOD_DIRECT_ACK",
                msg: Message::S2mModDirectAck(S2mModDirectAckParameters { id: 1, valid: true }),
                expected_out: Some("S2M_MOD_DIRECT_ACK id=1 valid=true\n".to_string()),
            },
            TestCase {
                name: "SET_CHAN_ACL",
                msg: Message::SetChannelAcl(SetChannelAclParameters {
                    id: 1,
                    channel: "!1@localhost".into(),
                    r#type: "publish".into(),
                    action: "add".into(),
                    nids: Vec::from(["test_user_1@localhost".into(), "example.com".into()].as_slice()),
                }),
                expected_out: Some("SET_CHAN_ACL id=1 action=add channel=!1@localhost nids:2=test_user_1@localhost example.com type=publish\n".to_string()),
            },
            TestCase {
                name: "SET_CHAN_CONFIG",
                msg: Message::SetChannelConfiguration ( SetChannelConfigurationParameters {
                    id: 1,
                    channel: "!1@localhost".into(),
                    max_clients: Some(100),
                    max_payload_size: Some(16),
                    ..Default::default()
                    }
                ),
                expected_out: Some("SET_CHAN_CONFIG id=1 channel=!1@localhost max_clients=100 max_payload_size=16\n".to_string()),
            },
        ];

    for tc in test_cases {
      let mut out = vec![0; DEFAULT_OUT_BUFF_SIZE];
      let res = super::serialize(&tc.msg, &mut out);

      assert!(res.is_ok());
      let n = res.unwrap();
      let out_str = String::from_utf8(out[..n].to_vec()).unwrap();
      assert_eq!(tc.expected_out.unwrap(), out_str, "test case '{}': error mismatch", tc.name);
    }
  }

  #[test]
  fn test_serialize_error() {
    struct TestCase {
      name: &'static str,
      msg: Message,
      out_buff_size: usize,
    }

    let test_cases = vec![TestCase {
      name: "!!! - message too large",
      msg: Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 120 }),
      out_buff_size: 10,
    }];

    for tc in test_cases {
      let mut out = vec![0; tc.out_buff_size];
      let res = super::serialize(&tc.msg, &mut out);
      assert!(res.is_err(), "test case '{}': expected error", tc.name);
    }
  }
}
