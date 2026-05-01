// SPDX-License-Identifier: BSD-3-Clause

use std::io::{Cursor, Read, Seek};

use crate::message::Message;

use narwhal_util::string_atom::StringAtom;

const MALFORMED_ERR_MSG: &str = "malformed message";

/// The default size of the message buffer used for deserialization.
pub const DEFAULT_MESSAGE_BUFFER_SIZE: usize = 4096;

/// Represents a message parameter.
/// A parameter consists of a name and a value.
#[derive(Debug)]
pub struct Parameter<'a> {
  /// The name of the parameter.
  pub name: &'a [u8],

  /// The value of the parameter.
  pub value: &'a [u8],
}

// === impl Parameter ===

impl Parameter<'_> {
  pub fn as_atom(&self) -> anyhow::Result<StringAtom> {
    Ok(StringAtom::from(std::str::from_utf8(self.value)?))
  }

  pub fn as_u8(&self) -> anyhow::Result<u8> {
    let v: u8 = std::str::from_utf8(self.value)?.parse()?;
    Ok(v)
  }

  pub fn as_u16(&self) -> anyhow::Result<u16> {
    let v: u16 = std::str::from_utf8(self.value)?.parse()?;
    Ok(v)
  }

  pub fn as_u32(&self) -> anyhow::Result<u32> {
    let v: u32 = std::str::from_utf8(self.value)?.parse()?;
    Ok(v)
  }

  pub fn as_u64(&self) -> anyhow::Result<u64> {
    let v: u64 = std::str::from_utf8(self.value)?.parse()?;
    Ok(v)
  }

  pub fn as_bool(&self) -> anyhow::Result<bool> {
    let v: bool = std::str::from_utf8(self.value)?.parse()?;
    Ok(v)
  }
}

/// Reads a message parameter from the cursor.
pub struct ParameterReader<'a> {
  /// The cursor to read from.
  c: Cursor<&'a [u8]>,

  /// The name of the current parameter.
  current_name: Option<&'a [u8]>,

  /// The number of values the current parameter has.
  current_value_count: usize,
}

// === impl ParameterReader ===

impl<'a> ParameterReader<'a> {
  fn new(c: Cursor<&'a [u8]>) -> Self {
    Self { c, current_name: None, current_value_count: 0 }
  }
}

impl<'a> Iterator for ParameterReader<'a> {
  type Item = anyhow::Result<Parameter<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    // If we don't have a current name, read the next parameter.
    if self.current_name.is_none() {
      let param_res = read_parameter(&mut self.c);
      if param_res.is_err() {
        return Some(Err(param_res.err().unwrap()));
      }
      let param_opt = param_res.unwrap();
      param_opt?;

      let param = param_opt.unwrap();

      self.current_name = Some(param.0);
      self.current_value_count = param.1;
    }
    // Extract the current parameter value.
    let param_value_res = read_escaped_string(&mut self.c);
    if param_value_res.is_err() {
      return Some(Err(param_value_res.err().unwrap()));
    }
    let param_value_opt = param_value_res.unwrap();
    if param_value_opt.is_none() {
      return Some(Err(anyhow::anyhow!(MALFORMED_ERR_MSG)));
    }
    let param_name = self.current_name.unwrap();

    // Reset the current name if we've read all values for the current parameter.
    self.current_value_count -= 1;
    if self.current_value_count == 0 {
      self.current_name = None;
    }
    Some(Ok(Parameter { name: param_name, value: param_value_opt.unwrap() }))
  }
}

/// Deserializes a message from a byte slice.
/// Returns the deserialized message or an error if the message is malformed.
pub fn deserialize(mut c: Cursor<&[u8]>) -> anyhow::Result<Message> {
  let msg_name_opt = read_string(&mut c)?;
  if msg_name_opt.is_none() {
    return Err(anyhow::anyhow!(MALFORMED_ERR_MSG));
  }
  let mut msg = Message::from_name(msg_name_opt.unwrap())?;

  let mut param_reader = ParameterReader::new(c);
  msg.decode_parameters(&mut param_reader)?;
  msg.validate_parameters()?;

  Ok(msg)
}

/// Reads a string from the cursor in the form of a byte slice.
fn read_string<'a>(c: &mut Cursor<&'a [u8]>) -> anyhow::Result<Option<&'a [u8]>> {
  let from_opt = seek_char(c)?;
  if from_opt.is_none() {
    return Ok(None);
  }
  let from = from_opt.unwrap() as usize;
  let mut to = from;
  loop {
    let b = read_byte(c)?;
    if b == 0 || is_space(b) {
      break;
    }
    to += 1;
  }
  Ok(Some(&c.get_ref()[from..to]))
}

/// Reads an escaped string from the cursor in the form of a byte slice.
fn read_escaped_string<'a>(c: &mut Cursor<&'a [u8]>) -> anyhow::Result<Option<&'a [u8]>> {
  let from_opt = seek_char(c)?;
  if from_opt.is_none() {
    return Ok(None);
  }

  if read_byte(c)? != b'\\' {
    unread_bytes(c, 1)?;
    return read_string(c);
  }

  let esc_char = read_byte(c)?;
  if !is_escape_char(esc_char) {
    unread_bytes(c, 2)?;
    return read_string(c);
  }

  let from = c.position() as usize;
  let mut to = from;

  loop {
    let b = read_byte(c)?;

    if b == b'\\' && to == from {
      to = (c.position() - 1) as usize;
    } else if b == esc_char && to != from {
      break;
    } else if b == 0 {
      return Err(anyhow::anyhow!(MALFORMED_ERR_MSG));
    } else if to != from {
      to = from;
    }
  }
  Ok(Some(&c.get_ref()[from..to]))
}

/// Reads a message parameter name from the cursor.
/// Returns the parameter name along with the number of values it has.
fn read_parameter<'a>(c: &mut Cursor<&'a [u8]>) -> anyhow::Result<Option<(&'a [u8], usize)>> {
  if seek_char(c)?.is_none() {
    return Ok(None);
  }
  let buffer = c.get_ref();
  let pos = c.position() as usize;

  if let Some(eq_pos) = buffer[pos..].iter().position(|&b| b == b'=') {
    let mut name = &buffer[pos..pos + eq_pos];
    let mut value_count = 1;

    if let Some(colon_pos) = name.iter().position(|&b| b == b':') {
      let value_count_str =
        std::str::from_utf8(&name[colon_pos + 1..]).map_err(|_| anyhow::anyhow!(MALFORMED_ERR_MSG))?;

      value_count = value_count_str.parse::<usize>().map_err(|_| anyhow::anyhow!(MALFORMED_ERR_MSG))?;
      name = &name[..colon_pos]
    }

    if !is_valid_option_name(name) {
      return Err(anyhow::anyhow!(MALFORMED_ERR_MSG));
    }
    // Move cursor to after the parsed `=`
    c.set_position((pos + eq_pos + 1) as u64);

    return Ok(Some((name, value_count)));
  }
  Err(anyhow::anyhow!(MALFORMED_ERR_MSG))
}

/// Seeks the cursor to the next non-space character.
/// Returns the position of the next non-space character, or None if the end of the buffer is reached.
fn seek_char(c: &mut Cursor<&[u8]>) -> anyhow::Result<Option<u64>> {
  loop {
    let b = read_byte(c)?;
    if b == 0 {
      return Ok(None);
    }
    if !is_space(b) {
      unread_bytes(c, 1)?;
      return Ok(Some(c.position()));
    }
  }
}

/// Reads a byte from the cursor.
fn read_byte(c: &mut Cursor<&[u8]>) -> anyhow::Result<u8> {
  if c.get_ref().len() - c.position() as usize == 0 {
    // End of buffer
    return Ok(0);
  }
  let mut buff: [u8; 1] = [0];
  c.read_exact(&mut buff)?;
  Ok(buff[0])
}

/// Seeks the cursor back one byte.
fn unread_bytes(c: &mut Cursor<&[u8]>, n: usize) -> anyhow::Result<()> {
  c.seek_relative(-(n as i64))?;
  Ok(())
}

fn is_valid_option_name(name: &[u8]) -> bool {
  name.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

fn is_escape_char(c: u8) -> bool {
  matches!(c, b'"' | b'\'' | b':' | b'*')
}

fn is_space(c: u8) -> bool {
  matches!(c, b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r')
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::message::Message;
  use crate::*;

  #[test]
  fn test_deserialize() {
    struct TestCase {
      name: &'static str,
      input: &'static [u8],
      expected: Result<Message, Error>,
    }

    let test_cases = vec![
    TestCase { name: "AUTH", input: b"AUTH id=1 token=a_token", expected: Ok(Message::Auth(AuthParameters {  token: StringAtom::from("a_token") })) },
    TestCase { name: "AUTH_ACK", input: b"AUTH_ACK id=1 succeeded=false challenge=1234567890 username=test_user nid=test_user@localhost", expected: Ok(Message::AuthAck(AuthAckParameters {  succeeded: Some(false), challenge: Some(StringAtom::from("1234567890")),  nid: Some(StringAtom::from("test_user@localhost")) })) },
    TestCase { name: "BROADCAST", input: b"BROADCAST id=1 channel=!1@localhost length=10 qos=1", expected: Ok(Message::Broadcast(BroadcastParameters { id: 1, channel: StringAtom::from("!1@localhost"), qos: Some(1), length: 10 })) },
    TestCase { name: "BROADCAST_ACK", input: b"BROADCAST_ACK id=1 seq=1", expected: Ok(Message::BroadcastAck(BroadcastAckParameters { id: 1, seq: 1 })) },
    TestCase {
            name: "CHAN_ACL",
            input: b"CHAN_ACL id=1 channel=!1@localhost type=publish nids:2=test_user_1@localhost example.com",
            expected: Ok(Message::ChannelAcl(ChannelAclParameters {
                id: 1,
                channel: StringAtom::from("!1@localhost"),
                r#type: StringAtom::from("publish"),
                nids: Vec::from([StringAtom::from("test_user_1@localhost"), StringAtom::from("example.com")].as_slice()),
                page: None,
                page_size: None,
                total_count: None,
            })),
    },
    TestCase {
            name: "CHAN_CONFIG",
            input: b"CHAN_CONFIG id=1 channel=!1@localhost max_clients=100 max_payload_size=16",
            expected: Ok(Message::ChannelConfiguration(ChannelConfigurationParameters { id: 1, channel: StringAtom::from("!1@localhost"),  max_clients: 100, max_payload_size:16, ..Default::default() })),
        },
    TestCase {
            name: "CONNECT",
            input: b"CONNECT version=1 heartbeat_interval=120",
            expected: Ok(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 120 })),
        },
    TestCase {
            name: "CONNECT_ACK",
            input: b" CONNECT_ACK application_protocol=NCP/1.0 heartbeat_interval=20000 max_inflight_requests=100 max_message_size=8192 max_payload_size=262144 max_subscriptions=100",
            expected: Ok(Message::ConnectAck(ConnectAckParameters {
                auth_required: false,
                application_protocol: Some("NCP/1.0".into()),
                heartbeat_interval: 20000,
                max_inflight_requests: 100,
                max_message_size: 8192,
                max_payload_size: 262144,
                max_subscriptions: 100,
                ..Default::default()
            })),
        },
    TestCase {
            name: "ERROR",
            input: b"ERROR reason=BAD_REQUEST detail=\\\"error detail\\\"  ",
            expected: Ok(Message::Error(ErrorParameters {
                id: None,
                reason: StringAtom::from("BAD_REQUEST"),
                detail: Some(StringAtom::from("error detail")),
            })),
        },
    TestCase { name: "EVENT", input: b"EVENT kind=MEMBER_JOINED channel=!1@localhost nid=test@localhost owner=true", expected: Ok(Message::Event(EventParameters { kind: StringAtom::from("MEMBER_JOINED"), channel: Some(StringAtom::from("!1@localhost")), nid: Some(StringAtom::from("test@localhost")), owner: Some(true) })) },
    TestCase { name: "GET_CHAN_ACL", input: b"GET_CHAN_ACL id=1 channel=!1@localhost page=1 page_size=10 type=publish", expected: Ok(Message::GetChannelAcl(GetChannelAclParameters { id: 1, channel: StringAtom::from("!1@localhost"), r#type: StringAtom::from("publish"), page: Some(1), page_size: Some(10) })) },
    TestCase { name: "GET_CHAN_CONFIG", input: b"GET_CHAN_CONFIG id=1 channel=!1@localhost", expected: Ok(Message::GetChannelConfiguration(GetChannelConfigurationParameters { id: 1, channel: StringAtom::from("!1@localhost") })) },
    TestCase {
        name: "IDENTIFY",
        input: b"IDENTIFY username=test",
        expected: Ok(Message::Identify(IdentifyParameters { username: StringAtom::from("test") })),
    },
    TestCase { name: "IDENTIFY_ACK", input: b"IDENTIFY_ACK nid=test_user@localhost", expected: Ok(Message::IdentifyAck(IdentifyAckParameters { nid: StringAtom::from("test_user@localhost") })) },
    TestCase { name: "JOIN", input: b"JOIN id=1 channel=!1@localhost on_behalf=test_user@localhost", expected: Ok(Message::JoinChannel(JoinChannelParameters { id: 1, channel: "!1@localhost".into(), on_behalf: Some(StringAtom::from("test_user@localhost")) })) },
    TestCase {
            name: "JOIN_ACK",
            input: b"JOIN_ACK id=1 channel=!1@localhost",
            expected: Ok(Message::JoinChannelAck(JoinChannelAckParameters { id: 1, channel: StringAtom::from("!1@localhost") })),
        },
    TestCase {
            name: "LEAVE",
            input: b"LEAVE id=1 channel=!1@localhost on_behalf=test_user@localhost",
            expected: Ok(Message::LeaveChannel(LeaveChannelParameters { id: 1, channel: StringAtom::from("!1@localhost"), on_behalf: Some(StringAtom::from("test_user@localhost")) })),
        },
    TestCase {
            name: "LEAVE_ACK",
            input: b"LEAVE_ACK id=1",
            expected: Ok(Message::LeaveChannelAck(LeaveChannelAckParameters { id: 1 })),
        },
    TestCase {
            name: "CHANNELS",
            input: b"CHANNELS id=1 page_size=10 page=1 owner=true",
            expected: Ok(Message::ListChannels(ListChannelsParameters { id: 1, page_size: Some(10), page: Some(1), owner: true })),
        },
    TestCase {
            name: "CHANNELS_ACK",
            input: b"CHANNELS_ACK id=1 channels:2=!1@localhost !2@localhost page=1 page_size=2 total_count=2",
            expected: Ok(Message::ListChannelsAck(ListChannelsAckParameters { id: 1, channels: Vec::from([StringAtom::from("!1@localhost"), StringAtom::from("!2@localhost")].as_slice()), page: Some(1), page_size: Some(2), total_count: Some(2) })),
        },
    TestCase {
        name: "MEMBERS",
        input: b"MEMBERS id=1 channel=!1@localhost",
        expected: Ok(Message::ListMembers(ListMembersParameters { id: 1, channel: StringAtom::from("!1@localhost"), page: None, page_size: None })),
    },
    TestCase {
        name: "MEMBERS_ACK",
        input: b"MEMBERS_ACK id=1 channel=!1@localhost members:2=test_user@localhost test_user2@localhost",
        expected: Ok(Message::ListMembersAck(ListMembersAckParameters {
            id: 1,
            channel: StringAtom::from("!1@localhost"),
            members: Vec::from([StringAtom::from("test_user@localhost"), StringAtom::from("test_user2@localhost")].as_slice()),
            page: None,
            page_size: None,
            total_count: None,
        })),
    },
    TestCase { name: "HISTORY", input: b"HISTORY id=1 channel=!1@localhost from_seq=5 history_id=h1 limit=10", expected: Ok(Message::History(HistoryParameters { id: 1, history_id: StringAtom::from("h1"), channel: StringAtom::from("!1@localhost"), from_seq: 5, limit: 10 })) },
    TestCase { name: "HISTORY_ACK", input: b"HISTORY_ACK id=1 channel=!1@localhost count=5 history_id=h1", expected: Ok(Message::HistoryAck(HistoryAckParameters { id: 1, history_id: StringAtom::from("h1"), channel: StringAtom::from("!1@localhost"), count: 5 })) },
    TestCase { name: "CHAN_SEQ", input: b"CHAN_SEQ id=1 channel=!1@localhost", expected: Ok(Message::ChannelSeq(ChannelSeqParameters { id: 1, channel: StringAtom::from("!1@localhost") })) },
    TestCase { name: "CHAN_SEQ_ACK", input: b"CHAN_SEQ_ACK id=1 channel=!1@localhost first_seq=1 last_seq=100", expected: Ok(Message::ChannelSeqAck(ChannelSeqAckParameters { id: 1, channel: StringAtom::from("!1@localhost"), first_seq: 1, last_seq: 100 })) },
    TestCase { name: "MESSAGE", input: b"MESSAGE from=test_user2@localhost channel=!1@localhost length=10 seq=1 timestamp=1", expected: Ok(Message::Message(MessageParameters { from: StringAtom::from("test_user2@localhost"), channel: StringAtom::from("!1@localhost"), length: 10, seq: 1, timestamp: 1, history_id: None })) },
    TestCase { name: "MESSAGE with history_id", input: b"MESSAGE from=test_user2@localhost channel=!1@localhost history_id=h1 length=10 seq=1 timestamp=1", expected: Ok(Message::Message(MessageParameters { from: StringAtom::from("test_user2@localhost"), channel: StringAtom::from("!1@localhost"), length: 10, seq: 1, timestamp: 1, history_id: Some(StringAtom::from("h1")) })) },
    TestCase { name: "M2S_MOD_DIRECT", input: b"M2S_MOD_DIRECT id=1 targets=ortuman length=10", expected: Ok(Message::M2sModDirect(M2sModDirectParameters { id: 1, targets: Vec::from([StringAtom::from("ortuman")].as_slice()), length: 10 })) },
    TestCase { name: "M2S_MOD_DIRECT_ACK", input: b"M2S_MOD_DIRECT_ACK id=1", expected: Ok(Message::M2sModDirectAck(M2sModDirectAckParameters { id: 1 })) },
    TestCase { name: "MOD_DIRECT", input: b"MOD_DIRECT id=2 from=test_user@localhost length=10", expected: Ok(Message::ModDirect(ModDirectParameters { id: Some(2), from: StringAtom::from("test_user@localhost"), length: 10 })) },
    TestCase { name: "MOD_DIRECT_ACK", input: b"MOD_DIRECT_ACK id=2", expected: Ok(Message::ModDirectAck(ModDirectAckParameters { id: 2 })) },
    TestCase { name: "PING", input: b"PING id=1234", expected: Ok(Message::Ping(PingParameters { id: 1234 })) },
    TestCase { name: "PING", input: b"PONG id=1234", expected: Ok(Message::Pong(PongParameters { id: 1234 })) },
    TestCase { name: "SET_CHAN_ACL_ACK", input: b"SET_CHAN_ACL_ACK id=1", expected: Ok(Message::SetChannelAclAck(SetChannelAclAckParameters { id: 1 })) },
    TestCase { name: "SET_CHAN_CONFIG_ACK", input: b"SET_CHAN_CONFIG_ACK id=1", expected: Ok(Message::SetChannelConfigurationAck(SetChannelConfigurationAckParameters { id: 1 })) },
    TestCase {
            name: "S2M_CONNECT",
            input: b"S2M_CONNECT version=1 secret=a_secret heartbeat_interval=120",
            expected: Ok(Message::S2mConnect(S2mConnectParameters { protocol_version: 1, secret: Some("a_secret".into()), heartbeat_interval: 120 })),
        },
    TestCase {
            name: "S2M_CONNECT_ACK",
            input: b"S2M_CONNECT_ACK application_protocol=my-proto/1.0 operation_mask=1 heartbeat_interval=20000 max_inflight_requests=100 max_message_size=8192 max_payload_size=262144",
            expected: Ok(Message::S2mConnectAck(S2mConnectAckParameters { application_protocol: "my-proto/1.0".into(), operation_mask: 1, heartbeat_interval: 20000, max_inflight_requests: 100, max_message_size: 8192, max_payload_size: 262144 }),),
        },
    TestCase {
            name: "S2M_CONNECT_ACK (all bits)",
            input: b"S2M_CONNECT_ACK application_protocol=my-proto/1.0 operation_mask=31 heartbeat_interval=20000 max_inflight_requests=100 max_message_size=8192 max_payload_size=262144",
            expected: Ok(Message::S2mConnectAck(S2mConnectAckParameters { application_protocol: "my-proto/1.0".into(), operation_mask: 0x1F, heartbeat_interval: 20000, max_inflight_requests: 100, max_message_size: 8192, max_payload_size: 262144 }),),
        },
    TestCase { name: "S2M_FORWARD_EVENT", input: b"S2M_FORWARD_EVENT id=1 kind=MEMBER_JOINED channel=!1@localhost nid=test@localhost owner=true", expected: Ok(Message::S2mForwardEvent(S2mForwardEventParameters { id: 1, kind: StringAtom::from("MEMBER_JOINED"), channel: Some(StringAtom::from("!1@localhost")), nid: Some(StringAtom::from("test@localhost")), owner: Some(true) })) },
    TestCase { name: "S2M_FORWARD_EVENT_ACK", input: b"S2M_FORWARD_EVENT_ACK id=1", expected: Ok(Message::S2mForwardEventAck(S2mForwardEventAckParameters { id: 1 })) },
    TestCase { name: "S2M_FORWARD_BROADCAST_PAYLOAD", input: b"S2M_FORWARD_BROADCAST_PAYLOAD id=1 from=ortuman@localhost channel=abc123 length=12", expected: Ok(Message::S2mForwardBroadcastPayload(S2mForwardBroadcastPayloadParameters { id: 1, from:"ortuman@localhost".into(), channel: "abc123".into(), length: 12 })) },
    TestCase { name: "S2M_FORWARD_BROADCAST_PAYLOAD_ACK", input: b"S2M_FORWARD_BROADCAST_PAYLOAD_ACK id=1 valid=true altered_payload=false altered_payload_length=0", expected: Ok(Message::S2mForwardBroadcastPayloadAck(S2mForwardBroadcastPayloadAckParameters { id: 1, valid: true, altered_payload: false, altered_payload_length: 0 })) },
    TestCase { name: "S2M_MOD_DIRECT", input: b"S2M_MOD_DIRECT id=1 from=ortuman@localhost length=10", expected: Ok(Message::S2mModDirect(S2mModDirectParameters { id: 1, from: "ortuman@localhost".into(), length: 10 })) },
    TestCase { name: "S2M_MOD_DIRECT_ACK", input: b"S2M_MOD_DIRECT_ACK id=1 valid=true", expected: Ok(Message::S2mModDirectAck(S2mModDirectAckParameters { id: 1, valid: true })) },
    TestCase {
            name: "SET_CHAN_ACL",
            input: b"SET_CHAN_ACL id=1 channel=!1@localhost type=publish action=add nids:2=test_user_1@localhost example.com",
            expected: Ok(Message::SetChannelAcl(SetChannelAclParameters {
                id: 1,
                channel: StringAtom::from("!1@localhost"),
                r#type: StringAtom::from("publish"),
                action: StringAtom::from("add"),
                nids: Vec::from([StringAtom::from("test_user_1@localhost"), StringAtom::from("example.com")].as_slice()),
            })),
    },
    TestCase {
            name: "SET_CHAN_CONFIG",
            input: b"SET_CHAN_CONFIG id=1 channel=!1@localhost max_clients=100 max_payload_size=16",
            expected: Ok(Message::SetChannelConfiguration(SetChannelConfigurationParameters { id: 1, channel: StringAtom::from("!1@localhost"), max_clients: Some(100), max_payload_size: Some(16), ..Default::default() })),
        },
    ];

    for test_case in test_cases.iter() {
      let cursor = Cursor::new(test_case.input);
      let result = deserialize(cursor);

      match &test_case.expected {
        Ok(expected_msg) => {
          assert!(result.is_ok(), "test case '{}': expected success but got error", test_case.name);
          assert_eq!(result.unwrap(), *expected_msg, "test case {}: message mismatch", test_case.name);
        },
        Err(expected_err) => {
          assert!(result.is_err(), "test case '{}': expected error but got success", test_case.name);
          assert_eq!(
            result.unwrap_err().to_string(),
            expected_err.to_string(),
            "test case '{}': error mismatch",
            test_case.name
          );
        },
      }
    }
  }

  #[test]
  fn test_deserialize_errors() {
    struct TestCase {
      name: &'static str,
      input: &'static [u8],
      expected: &'static str,
    }

    let test_cases = [
      TestCase { name: "empty", input: b"", expected: "malformed message" },
      TestCase { name: "only spaces", input: b"     ", expected: "malformed message" },
      TestCase { name: "unknown message", input: b"1 UNKNOWN", expected: "unknown message" },
      TestCase {
        name: "bad escaped string",
        input: b"ERROR reason=BAD_REQUEST detail=\\\"error detail\\:  ",
        expected: "malformed message",
      },
    ];

    for test_case in test_cases.iter() {
      let cursor = Cursor::new(test_case.input);
      let result = deserialize(cursor);

      assert!(result.is_err(), "test case '{}': expected error but got success", test_case.name);
      assert_eq!(
        result.unwrap_err().to_string(),
        test_case.expected.to_string(),
        "test case '{}': error mismatch",
        test_case.name
      );
    }
  }
}
