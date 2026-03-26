// SPDX-License-Identifier: BSD-3-Clause

use std::io::Write;
use std::time::Duration;

use narwhal_modulator::modulator::{AuthResult, ForwardBroadcastPayloadResult, SendPrivatePayloadResult};
use narwhal_protocol::{
  ErrorParameters, EventKind, Message, S2mAuthAckParameters, S2mAuthParameters, S2mConnectParameters,
  S2mForwardBroadcastPayloadAckParameters, S2mForwardBroadcastPayloadParameters, S2mForwardEventAckParameters,
  S2mForwardEventParameters, S2mModDirectAckParameters, S2mModDirectParameters,
};
use narwhal_test_util::{S2mSuite, TestModulator, assert_message, default_s2m_config_with_secret};
use narwhal_util::string_atom::StringAtom;

const TEST_MODULATOR_SECRET: &str = "a_test_secret";

#[compio::test]
async fn test_s2m_connect_timeout() -> anyhow::Result<()> {
  // Set the connection timeout to 50ms.
  let mut config = default_s2m_config_with_secret(TEST_MODULATOR_SECRET);
  config.server.connect_timeout = Duration::from_millis(50);

  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    });

  let mut suite = S2mSuite::with_config(config, modulator).await;
  suite.setup().await?;

  // Connect to the server.
  let mut socket = suite.socket_connect().await?;

  // Wait for the connection to timeout.
  narwhal_common::runtime::sleep(Duration::from_millis(250)).await;

  // Verify that the connection timed out and the server sent an error message.
  assert_message!(
    socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::Timeout.into(),
      detail: Some(StringAtom::from("connection timeout")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_ping_timeout() -> anyhow::Result<()> {
  // Configure keep-alive parameters.
  let mut config = default_s2m_config_with_secret(TEST_MODULATOR_SECRET);
  config.server.keep_alive_interval = Duration::from_millis(100);
  config.server.min_keep_alive_interval = Duration::from_millis(100);

  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    });

  let mut suite = S2mSuite::with_config(config, modulator).await;
  suite.setup().await?;

  // Identify a user.
  let mut conn = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Wait until ping is received.
  narwhal_common::runtime::sleep(Duration::from_millis(150)).await;

  let ping_msg = conn.read_message().await?;
  assert!(matches!(ping_msg, Message::Ping { .. }));

  // Wait for keep-alive timeout.
  narwhal_common::runtime::sleep(Duration::from_millis(200)).await;

  // Verify that the server sent the proper error message.
  assert_message!(
    conn.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::Timeout.into(),
      detail: Some(StringAtom::from("ping timeout")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_max_connection_limit_reached() -> anyhow::Result<()> {
  // Set the maximum number of streams to 1.
  let mut config = default_s2m_config_with_secret(TEST_MODULATOR_SECRET);
  config.server.limits.max_connections = 1;

  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    });

  let mut suite = S2mSuite::with_config(config, modulator).await;
  suite.setup().await?;

  // Establish a first connection — keep it alive as a local variable.
  let _first = suite.socket_connect().await?;

  // Connect to the server again and expect an error.
  let mut socket = suite.socket_connect().await?;

  // Verify that the connection was rejected and the server sent an error message.
  assert_message!(
    socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::ServerOverloaded.into(),
      detail: Some(StringAtom::from("max connections reached")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_max_message_size_exceeded() -> anyhow::Result<()> {
  // Set the maximum message size to 1024 bytes.
  let mut config = default_s2m_config_with_secret(TEST_MODULATOR_SECRET);
  config.server.limits.max_message_size = 1024;

  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    });

  let mut suite = S2mSuite::with_config(config, modulator).await;
  suite.setup().await?;

  // Identify a user.
  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Construct a message that exceeds the maximum message size.
  let message = b"PONG opt=";

  let mut buffer = Vec::from(message);
  buffer.extend(std::iter::repeat_n(b'0', 1024));

  // Write the message to the server.
  socket.write_raw_bytes(&buffer).await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::PolicyViolation.into(),
      detail: Some(StringAtom::from("max message size exceeded")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_auth_success() -> anyhow::Result<()> {
  let modulator = TestModulator::new().with_auth_handler(|token| async move {
    if token.as_ref() == "valid_token" {
      Ok(AuthResult::Success { username: StringAtom::from("test_user") })
    } else {
      Ok(AuthResult::Failure)
    }
  });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send auth message
  socket.write_message(Message::S2mAuth(S2mAuthParameters { id: 1, token: StringAtom::from("valid_token") })).await?;

  // Verify auth success response
  assert_message!(
    socket.read_message().await?,
    Message::S2mAuthAck,
    S2mAuthAckParameters { id: 1, challenge: None, username: Some(StringAtom::from("test_user")), succeeded: true }
  );

  suite.teardown().await?;
  Ok(())
}

#[compio::test]
async fn test_s2m_auth_failure() -> anyhow::Result<()> {
  let modulator = TestModulator::new().with_auth_handler(|token| async move {
    if token.as_ref() == "valid_token" {
      Ok(AuthResult::Success { username: StringAtom::from("test_user") })
    } else {
      Ok(AuthResult::Failure)
    }
  });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send auth message with invalid token
  socket.write_message(Message::S2mAuth(S2mAuthParameters { id: 1, token: StringAtom::from("invalid_token") })).await?;

  // Verify auth failure response
  assert_message!(
    socket.read_message().await?,
    Message::S2mAuthAck,
    S2mAuthAckParameters { id: 1, challenge: None, username: None, succeeded: false }
  );

  suite.teardown().await?;
  Ok(())
}

#[compio::test]
async fn test_s2m_auth_continue() -> anyhow::Result<()> {
  let modulator = TestModulator::new().with_auth_handler(|token| async move {
    if token.as_ref() == "challenge_token" {
      Ok(AuthResult::Continue { challenge: StringAtom::from("challenge_response") })
    } else {
      Ok(AuthResult::Failure)
    }
  });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send auth message that requires challenge
  socket
    .write_message(Message::S2mAuth(S2mAuthParameters { id: 1, token: StringAtom::from("challenge_token") }))
    .await?;

  // Verify auth continue response with challenge
  assert_message!(
    socket.read_message().await?,
    Message::S2mAuthAck,
    S2mAuthAckParameters {
      id: 1,
      challenge: Some(StringAtom::from("challenge_response")),
      username: None,
      succeeded: false,
    }
  );

  suite.teardown().await?;
  Ok(())
}

#[compio::test]
async fn test_s2m_forward_payload_without_alter() -> anyhow::Result<()> {
  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|payload, _from, _channel_handler| async move {
      if payload.as_slice() == b"valid" {
        Ok(ForwardBroadcastPayloadResult::Valid)
      } else {
        Ok(ForwardBroadcastPayloadResult::Invalid)
      }
    });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send forward a payload message
  socket
    .write_message(Message::S2mForwardBroadcastPayload(S2mForwardBroadcastPayloadParameters {
      id: 1,
      from: "ortuman@localhost".into(),
      channel: "test1".into(),
      length: 5,
    }))
    .await?;

  // Write the payload
  socket.write_raw_bytes(b"valid").await?;
  socket.write_raw_bytes(b"\n").await?;

  // Verify forward payload response
  assert_message!(
    socket.read_message().await?,
    Message::S2mForwardBroadcastPayloadAck,
    S2mForwardBroadcastPayloadAckParameters { id: 1, valid: true, altered_payload: false, altered_payload_length: 0 }
  );

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_forward_payload_with_alter() -> anyhow::Result<()> {
  const PAYLOAD: &str = "to be";
  const ALTERED_PAYLOAD_SUFFIX: &str = "or not to be";
  const ALTERED_PAYLOAD_LENGTH: usize = PAYLOAD.len() + ALTERED_PAYLOAD_SUFFIX.len() + 1; // +1 for the space character

  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|payload, _from, _channel_handler| async move {
      let pool = narwhal_util::pool::Pool::new(1, 1024);
      let mut mut_pool_buffer = pool.acquire_buffer().await;

      let mut_buff_ptr = mut_pool_buffer.as_mut_slice();
      let n: usize = {
        let mut c = std::io::Cursor::new(mut_buff_ptr);
        let payload_str = std::str::from_utf8(payload.as_slice())?;

        write!(c, "{} {}", payload_str, ALTERED_PAYLOAD_SUFFIX)?;

        c.position() as usize
      };

      let pool_buffer = mut_pool_buffer.freeze(n);

      Ok(ForwardBroadcastPayloadResult::ValidWithAlteration { altered_payload: pool_buffer })
    });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send forward a payload message
  socket
    .write_message(Message::S2mForwardBroadcastPayload(S2mForwardBroadcastPayloadParameters {
      id: 1,
      from: "ortuman@localhost".into(),
      channel: "test1".into(),
      length: PAYLOAD.len() as u32,
    }))
    .await?;

  // Write the payload
  socket.write_raw_bytes(PAYLOAD.as_bytes()).await?;
  socket.write_raw_bytes(b"\n").await?;

  // Verify forward payload response
  assert_message!(
    socket.read_message().await?,
    Message::S2mForwardBroadcastPayloadAck,
    S2mForwardBroadcastPayloadAckParameters {
      id: 1,
      valid: true,
      altered_payload: true,
      altered_payload_length: ALTERED_PAYLOAD_LENGTH as u32,
    }
  );

  // Verify that the altered payload was received
  let mut altered_payload: [u8; ALTERED_PAYLOAD_LENGTH] = Default::default();
  socket.read_raw_bytes(altered_payload.as_mut_slice()).await?;

  assert!(altered_payload.eq(b"to be or not to be"));

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_forward_event() -> anyhow::Result<()> {
  let modulator = TestModulator::new().with_forward_event_handler(|_event| async { Ok(()) });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send forward event message
  socket
    .write_message(Message::S2mForwardEvent(S2mForwardEventParameters {
      id: 1,
      kind: EventKind::MemberJoined.into(),
      channel: None,
      nid: None,
      owner: None,
    }))
    .await?;

  // Verify forward payload response
  assert_message!(socket.read_message().await?, Message::S2mForwardEventAck, S2mForwardEventAckParameters { id: 1 });

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_unsupported_protocol_version() -> anyhow::Result<()> {
  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.socket_connect().await?;

  // Send a connect message with an unsupported protocol version
  socket
    .write_message(Message::S2mConnect(S2mConnectParameters {
      protocol_version: 999,
      secret: Some(TEST_MODULATOR_SECRET.into()),
      heartbeat_interval: 0,
    }))
    .await?;

  // Verify error response
  assert_message!(
    socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::UnsupportedProtocolVersion.into(),
      detail: None
    }
  );

  suite.teardown().await?;
  Ok(())
}

#[compio::test]
async fn test_s2m_invalid_message() -> anyhow::Result<()> {
  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send an invalid message type
  socket.write_raw_bytes(b"INVALID_MESSAGE\n").await?;

  // Verify error response
  assert_message!(
    socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::BadRequest.into(),
      detail: Some(StringAtom::from("unknown message")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[compio::test]
async fn test_s2m_mod_direct() -> anyhow::Result<()> {
  const PAYLOAD: &str = "private payload";

  let modulator = TestModulator::new().with_send_private_payload_handler(|payload, _from| async move {
    let payload_str = String::from_utf8(payload.to_vec()).unwrap();
    if payload_str == "private payload" {
      Ok(SendPrivatePayloadResult::Valid)
    } else {
      Ok(SendPrivatePayloadResult::Invalid)
    }
  });

  let mut suite = S2mSuite::with_config(default_s2m_config_with_secret(TEST_MODULATOR_SECRET), modulator).await;
  suite.setup().await?;

  let mut socket = suite.connect(TEST_MODULATOR_SECRET).await?;

  // Send mod direct message
  socket
    .write_message(Message::S2mModDirect(S2mModDirectParameters {
      id: 1,
      from: "ortuman@localhost".into(),
      length: PAYLOAD.len() as u32,
    }))
    .await?;

  // Write the payload
  socket.write_raw_bytes(PAYLOAD.as_bytes()).await?;
  socket.write_raw_bytes(b"\n").await?;

  // Verify mod direct response
  assert_message!(
    socket.read_message().await?,
    Message::S2mModDirectAck,
    S2mModDirectAckParameters { id: 1, valid: true }
  );

  suite.teardown().await?;

  Ok(())
}
