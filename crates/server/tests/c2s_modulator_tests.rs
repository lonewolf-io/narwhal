// SPDX-License-Identifier: BSD-3-Clause

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use async_lock::Mutex;

use narwhal_client::S2mConfig;
use narwhal_client::monoio::s2m::S2mClient;
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_modulator::create_s2m_listener;
use narwhal_modulator::modulator::{AuthResult, ForwardBroadcastPayloadResult, SendPrivatePayloadResult};
use narwhal_modulator::{OutboundPrivatePayload, create_m2s_listener};
use narwhal_protocol::EventKind::{MemberJoined, MemberLeft};
use narwhal_protocol::{
  AuthAckParameters, AuthParameters, BroadcastAckParameters, BroadcastParameters, ConnectParameters, ErrorParameters,
  Event, EventParameters, JoinChannelParameters, ListChannelsParameters, Message, ModDirectAckParameters,
  ModDirectParameters,
};
use narwhal_test_util::{
  C2sSuite, TestModulator, assert_message, default_c2s_config, default_m2s_config, default_s2m_config,
};
use narwhal_util::pool::Pool;
use narwhal_util::string_atom::StringAtom;

// Test usernames
const TEST_USER_1: &str = "test_user_1";
const TEST_USER_2: &str = "test_user_2";
const TEST_USER_3: &str = "test_user_3";

const SHARED_SECRET: &str = "a_secret";

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_single_step_auth() -> anyhow::Result<()> {
  let modulator = TestModulator::new()
    .with_auth_handler(|_| async { Ok(AuthResult::Success { username: StringAtom::from("test_user") }) });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  // Connect to the server
  let mut tls_socket = suite.tls_socket_connect().await?;

  // Send CONNECT message
  tls_socket.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;

  // Receive CONNECT_ACK
  let connect_ack = tls_socket.read_message().await?;
  assert!(matches!(connect_ack, Message::ConnectAck { .. }));

  // Send AUTH message with a token - the modulator will authenticate and return "test_user"
  tls_socket.write_message(Message::Auth(AuthParameters { token: StringAtom::from("any_token") })).await?;

  // Verify AUTH_ACK with the username from the modulator
  assert_message!(
    tls_socket.read_message().await?,
    Message::AuthAck,
    AuthAckParameters { challenge: None, succeeded: Some(true), nid: Some(StringAtom::from("test_user@localhost")) }
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_auth_failed() -> anyhow::Result<()> {
  let modulator = TestModulator::new().with_auth_handler(|_| async { Ok(AuthResult::Failure) });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  // Connect to the server
  let mut tls_socket = suite.tls_socket_connect().await?;

  // Send CONNECT message
  tls_socket.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;

  // Receive CONNECT_ACK
  let connect_ack = tls_socket.read_message().await?;
  assert!(matches!(connect_ack, Message::ConnectAck { .. }));

  // Send AUTH message with a token - the modulator will reject it
  tls_socket.write_message(Message::Auth(AuthParameters { token: StringAtom::from("any_token") })).await?;

  // Verify authentication failure
  assert_message!(
    tls_socket.read_message().await?,
    Message::AuthAck,
    AuthAckParameters { challenge: None, succeeded: Some(false), nid: None }
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_multi_step_auth() -> anyhow::Result<()> {
  let modulator = TestModulator::new().with_auth_handler(|token| async move {
    if token.as_ref() == "initial_token" {
      Ok(AuthResult::Continue { challenge: StringAtom::from("provide_second_token") })
    } else if token.as_ref() == "second_token" {
      Ok(AuthResult::Success { username: StringAtom::from("authenticated_user") })
    } else {
      Ok(AuthResult::Failure)
    }
  });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  // Connect to the server
  let mut tls_socket = suite.tls_socket_connect().await?;

  // Send CONNECT message
  tls_socket.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;

  // Receive CONNECT_ACK
  let connect_ack = tls_socket.read_message().await?;
  assert!(matches!(connect_ack, Message::ConnectAck { .. }));

  // Send first AUTH message with initial token
  tls_socket.write_message(Message::Auth(AuthParameters { token: StringAtom::from("initial_token") })).await?;

  // Verify AUTH_ACK with challenge
  assert_message!(
    tls_socket.read_message().await?,
    Message::AuthAck,
    AuthAckParameters { challenge: Some(StringAtom::from("provide_second_token")), succeeded: None, nid: None }
  );

  // Send second AUTH message with second token
  tls_socket.write_message(Message::Auth(AuthParameters { token: StringAtom::from("second_token") })).await?;

  // Verify final AUTH_ACK with success
  assert_message!(
    tls_socket.read_message().await?,
    Message::AuthAck,
    AuthAckParameters {
      challenge: None,
      succeeded: Some(true),
      nid: Some(StringAtom::from("authenticated_user@localhost"))
    }
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_send_private_payload() -> anyhow::Result<()> {
  // Create a modulator that validates private payloads - only accepts messages that contain "valid"
  let modulator = TestModulator::new().with_send_private_payload_handler(|payload, _from| async move {
    // Validate the payload - only accept payloads that contain "valid"
    let payload_str = std::str::from_utf8(payload.as_slice()).unwrap_or("");
    let is_valid = payload_str.contains("valid");

    if is_valid { Ok(SendPrivatePayloadResult::Valid) } else { Ok(SendPrivatePayloadResult::Invalid) }
  });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  // Identify the user
  suite.identify(TEST_USER_1).await?;

  // Send a valid direct message
  let valid_payload = "This is a valid message!";
  suite
    .write_message(
      TEST_USER_1,
      Message::ModDirect(ModDirectParameters {
        id: Some(1),
        from: TEST_USER_1.into(),
        length: valid_payload.len() as u32,
      }),
    )
    .await?;

  // Send the payload
  suite.write_raw_bytes(TEST_USER_1, valid_payload.as_bytes()).await?;
  suite.write_raw_bytes(TEST_USER_1, b"\n").await?;

  // Verify that the server sent the proper ModDirectAck
  assert_message!(suite.read_message(TEST_USER_1).await?, Message::ModDirectAck, ModDirectAckParameters { id: 1 });

  // Test 2: Send an invalid direct message (without "valid" in the content)
  let invalid_payload = "This message is not acceptable";
  suite
    .write_message(
      TEST_USER_1,
      Message::ModDirect(ModDirectParameters {
        id: Some(2),
        from: TEST_USER_1.into(),
        length: invalid_payload.len() as u32,
      }),
    )
    .await?;

  // Send the payload
  suite.write_raw_bytes(TEST_USER_1, invalid_payload.as_bytes()).await?;
  suite.write_raw_bytes(TEST_USER_1, b"\n").await?;

  // Verify that the server sent an error for invalid payload
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters { id: Some(2), reason: narwhal_protocol::ErrorReason::BadRequest.into(), detail: None }
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_receive_private_payload() -> anyhow::Result<()> {
  const TEST_PRIVATE_PAYLOAD: &str = r#"{"type":"test","message":"Hello from modulator"}"#;

  // Set up M2S server with a shared secret.
  let mut m2s_config = default_m2s_config();
  m2s_config.shared_secret = SHARED_SECRET.to_string();

  // Two separate broadcast channels:
  let (s2m_payload_tx, s2m_payload_rx) = async_broadcast::broadcast::<OutboundPrivatePayload>(16);
  let (m2s_payload_tx, m2s_payload_rx) = async_broadcast::broadcast::<OutboundPrivatePayload>(16);

  // Create M2S listener with the M2S→C2S channel.
  let mut core_dispatcher_m2s = CoreDispatcher::new(1);
  core_dispatcher_m2s.bootstrap().await?;

  let mut m2s_listener = create_m2s_listener(m2s_config, m2s_payload_tx, core_dispatcher_m2s.clone()).await?;
  m2s_listener.bootstrap().await?;

  // Create a modulator that provides the receiver for private payloads.
  let modulator = TestModulator::new().with_receive_private_payload_handler(move || {
    let rx = s2m_payload_rx.clone();
    async move { Ok(rx) }
  });

  // Configure S2M with the M2S server address.
  let mut s2m_config = default_s2m_config(SHARED_SECRET);
  s2m_config.m2s_client.address = m2s_listener.local_address().unwrap().to_string();
  s2m_config.m2s_client.shared_secret = SHARED_SECRET.to_string();

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(s2m_config, modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite =
    C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), Some(m2s_payload_rx)).await?;
  suite.setup().await?;

  // Identify test users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Build and send the private payload.
  let priv_payload_bytes = TEST_PRIVATE_PAYLOAD.as_bytes();
  let targets = vec![StringAtom::from(TEST_USER_1), StringAtom::from(TEST_USER_2)];

  let pool = Pool::new(1, TEST_PRIVATE_PAYLOAD.len());
  let mut mut_buffer = pool.acquire_buffer().await;
  mut_buffer.as_mut_slice()[..priv_payload_bytes.len()].copy_from_slice(priv_payload_bytes);
  let payload_buffer = mut_buffer.freeze(priv_payload_bytes.len());

  let outbound = OutboundPrivatePayload { payload: payload_buffer, targets };
  s2m_payload_tx.try_broadcast(outbound).expect("failed to send payload");

  // Verify that TEST_USER_1 received the proper private payload.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ModDirect,
    ModDirectParameters { id: None, from: "localhost".into(), length: TEST_PRIVATE_PAYLOAD.len() as u32 }
  );

  let mut user1_payload = vec![0u8; TEST_PRIVATE_PAYLOAD.len()];
  suite.read_raw_bytes(TEST_USER_1, &mut user1_payload).await?;
  assert_eq!(user1_payload.as_slice(), TEST_PRIVATE_PAYLOAD.as_bytes());

  // Verify that TEST_USER_2 received the proper private payload.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::ModDirect,
    ModDirectParameters { id: None, from: "localhost".into(), length: TEST_PRIVATE_PAYLOAD.len() as u32 }
  );

  let mut user2_payload = vec![0u8; TEST_PRIVATE_PAYLOAD.len()];
  suite.read_raw_bytes(TEST_USER_2, &mut user2_payload).await?;
  assert_eq!(user2_payload.as_slice(), TEST_PRIVATE_PAYLOAD.as_bytes());

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;
  m2s_listener.shutdown().await?;
  core_dispatcher_m2s.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_broadcast_payload_validation() -> anyhow::Result<()> {
  // Create a modulator that validates payloads - only accepts payloads that contain "valid"
  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|payload, _nid, _channel_handler| async move {
      // Only accept payloads that contain "valid"
      let payload_str = std::str::from_utf8(payload.as_slice()).unwrap_or("");
      let is_valid = payload_str.contains("valid");

      if is_valid { Ok(ForwardBroadcastPayloadResult::Valid) } else { Ok(ForwardBroadcastPayloadResult::Invalid) }
    });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  suite.identify(TEST_USER_1).await?;

  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Broadcast a valid payload.
  suite
    .write_message(
      TEST_USER_1,
      Message::Broadcast(BroadcastParameters {
        id: 1,
        channel: StringAtom::from("!test1@localhost"),
        qos: None,
        length: 12,
      }),
    )
    .await?;

  suite.write_raw_bytes(TEST_USER_1, "Hello valid!".as_bytes()).await?;
  suite.write_raw_bytes(TEST_USER_1, b"\n").await?;

  // Verify that the server sent the proper broadcast ack.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::BroadcastAck,
    BroadcastAckParameters { id: 1, seq: 1 }
  );

  // Broadcast a non-valid payload.
  suite
    .write_message(
      TEST_USER_1,
      Message::Broadcast(BroadcastParameters {
        id: 1,
        channel: StringAtom::from("!test1@localhost"),
        qos: None,
        length: 12,
      }),
    )
    .await?;

  suite.write_raw_bytes(TEST_USER_1, "Hello world!".as_bytes()).await?;
  suite.write_raw_bytes(TEST_USER_1, b"\n").await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters { id: Some(1), reason: narwhal_protocol::ErrorReason::BadRequest.into(), detail: None }
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_broadcast_payload_alteration() -> anyhow::Result<()> {
  // Create a modulator that reverses the payload text
  let modulator =
    TestModulator::new().with_forward_message_payload_handler(|payload, _nid, _channel_handler| async move {
      // Convert payload to string and reverse it
      let payload_str = std::str::from_utf8(payload.as_slice()).unwrap_or("");
      let reversed = payload_str.chars().rev().collect::<String>();

      // Create a new pool buffer with the reversed text
      let pool = Pool::new(1, 1024);
      let mut mut_pool_buffer = pool.acquire_buffer().await;

      let mut_buff_ptr = mut_pool_buffer.as_mut_slice();
      let n: usize = {
        let mut c = std::io::Cursor::new(mut_buff_ptr);
        write!(c, "{}", reversed)?;
        c.position() as usize
      };

      let pool_buffer = mut_pool_buffer.freeze(n);

      Ok(ForwardBroadcastPayloadResult::ValidWithAlteration { altered_payload: pool_buffer })
    });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  suite.identify(TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Join a second user to verify they receive the reversed message
  suite.identify(TEST_USER_2).await?;
  suite.join_channel(TEST_USER_2, "!test1@localhost", None).await?;

  // User 1 receives an event about User 2 joining
  suite.ignore_reply(TEST_USER_1).await?;

  let input_text = "Hello world!";
  let reversed_input_text = input_text.chars().rev().collect::<String>();

  suite
    .write_message(
      TEST_USER_1,
      Message::Broadcast(BroadcastParameters {
        id: 2,
        channel: StringAtom::from("!test1@localhost"),
        qos: None,
        length: input_text.len() as u32,
      }),
    )
    .await?;

  suite.write_raw_bytes(TEST_USER_1, input_text.as_bytes()).await?;
  suite.write_raw_bytes(TEST_USER_1, b"\n").await?;

  // User 1 gets ack
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::BroadcastAck,
    BroadcastAckParameters { id: 2, seq: 1 }
  );

  // User 2 receives the reversed message
  match suite.read_message(TEST_USER_2).await? {
    Message::Message(params) => {
      assert_eq!(params.from, StringAtom::from("test_user_1@localhost"));
      assert_eq!(params.channel, StringAtom::from("!test1@localhost"));
      assert_eq!(params.length, reversed_input_text.len() as u32);
      assert_eq!(params.seq, 1);
      assert!(params.timestamp > 0, "timestamp must be non-zero");
    },
    msg => panic!("expected Message::Message, got {:?}", msg),
  }

  // Read and verify the reversed payload from User 2's perspective
  let mut received_payload = vec![0u8; reversed_input_text.len()];
  suite.read_raw_bytes(TEST_USER_2, &mut received_payload).await?;
  assert_eq!(received_payload, reversed_input_text.as_bytes());

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_forward_event() -> anyhow::Result<()> {
  // Create a modulator that captures events
  let captured_events = Arc::new(Mutex::new(Vec::new()));
  let captured_events_clone = captured_events.clone();

  let modulator = TestModulator::new().with_forward_event_handler(move |event: Event| {
    let captured_events = captured_events_clone.clone();
    async move {
      captured_events.lock().await.push(event);
      Ok(())
    }
  });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  // User 1 joins and creates a channel (no event sent to creator but modulator is notified)
  suite.identify(TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // User 2 joins the same channel - this should trigger a MemberJoined event
  suite.identify(TEST_USER_2).await?;
  suite.join_channel(TEST_USER_2, "!test1@localhost", None).await?;

  // User 1 receives an event about User 2 joining
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Event,
    EventParameters {
      kind: MemberJoined.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_2@localhost")),
      owner: Some(false),
    }
  );

  // User 3 joins the channel - another MemberJoined event
  suite.identify(TEST_USER_3).await?;
  suite.join_channel(TEST_USER_3, "!test1@localhost", None).await?;

  // User 1 and User 2 receive events about User 3 joining
  suite.ignore_reply(TEST_USER_1).await?;
  suite.ignore_reply(TEST_USER_2).await?;

  // User 2 leaves the channel - this should trigger a MemberLeft event
  suite.leave_channel(TEST_USER_2, "!test1@localhost").await?;

  // User 1 and User 3 receive events about User 2 leaving
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Event,
    EventParameters {
      kind: MemberLeft.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_2@localhost")),
      owner: Some(false),
    }
  );

  assert_message!(
    suite.read_message(TEST_USER_3).await?,
    Message::Event,
    EventParameters {
      kind: MemberLeft.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_2@localhost")),
      owner: Some(false),
    }
  );

  // Since forward_event is called synchronously before clients receive notifications,
  // and we've already verified that all clients received their events above,
  // we know the modulator has already processed all events.

  // Verify that the modulator received the expected events
  let events = captured_events.lock().await;
  assert_eq!(events.len(), 4, "expected 4 events to be forwarded to the modulator");

  // Verify the first event (User 1 joined as owner/channel creator)
  assert_eq!(events[0].kind, MemberJoined);
  assert_eq!(events[0].channel, Some(StringAtom::from("!test1@localhost")));
  assert_eq!(events[0].nid, Some(StringAtom::from("test_user_1@localhost")));
  assert_eq!(events[0].owner, Some(true));

  // Verify the second event (User 2 joined)
  assert_eq!(events[1].kind, MemberJoined);
  assert_eq!(events[1].channel, Some(StringAtom::from("!test1@localhost")));
  assert_eq!(events[1].nid, Some(StringAtom::from("test_user_2@localhost")));

  // Verify the third event (User 3 joined)
  assert_eq!(events[2].kind, MemberJoined);
  assert_eq!(events[2].channel, Some(StringAtom::from("!test1@localhost")));
  assert_eq!(events[2].nid, Some(StringAtom::from("test_user_3@localhost")));

  // Verify the fourth event (User 2 left)
  assert_eq!(events[3].kind, MemberLeft);
  assert_eq!(events[3].channel, Some(StringAtom::from("!test1@localhost")));
  assert_eq!(events[3].nid, Some(StringAtom::from("test_user_2@localhost")));
  assert_eq!(events[3].owner, Some(false));

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_modulator_channel_survives_single_connection_drop() -> anyhow::Result<()> {
  // Create a modulator that authenticates all connections as the same user
  let modulator = TestModulator::new()
    .with_auth_handler(|_| async { Ok(AuthResult::Success { username: StringAtom::from(TEST_USER_1) }) });

  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(default_s2m_config(SHARED_SECRET), modulator, core_dispatcher.clone()).await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  let mut suite = C2sSuite::with_modulator(default_c2s_config(), Some(s2m_client.clone()), None).await?;
  suite.setup().await?;

  // First connection for TEST_USER_1
  let mut conn1 = suite.tls_socket_connect().await?;

  conn1.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;
  let _ = conn1.read_message().await?; // CONNECT_ACK

  conn1.write_message(Message::Auth(AuthParameters { token: StringAtom::from("token1") })).await?;

  assert_message!(
    conn1.read_message().await?,
    Message::AuthAck,
    AuthAckParameters { challenge: None, succeeded: Some(true), nid: Some(StringAtom::from("test_user_1@localhost")) }
  );

  // Second connection for the same TEST_USER_1
  let mut conn2 = suite.tls_socket_connect().await?;

  conn2.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;
  let _ = conn2.read_message().await?; // CONNECT_ACK

  conn2.write_message(Message::Auth(AuthParameters { token: StringAtom::from("token2") })).await?;

  assert_message!(
    conn2.read_message().await?,
    Message::AuthAck,
    AuthAckParameters { challenge: None, succeeded: Some(true), nid: Some(StringAtom::from("test_user_1@localhost")) }
  );

  // First connection joins a channel
  conn1
    .write_message(Message::JoinChannel(JoinChannelParameters {
      id: 1234,
      channel: "!test1@localhost".into(),
      on_behalf: None,
    }))
    .await?;
  let join_ack = conn1.read_message().await?;
  assert!(matches!(join_ack, Message::JoinChannelAck { .. }), "expected JoinChannelAck, got: {:?}", join_ack);

  // Extract the channel ID from the JoinChannelAck
  let channel_id = if let Message::JoinChannelAck(params) = join_ack {
    params.channel
  } else {
    panic!("expected JoinChannelAck");
  };

  // conn2 will receive a MemberJoined event since conn1 (same user) joined the channel
  let event_msg = conn2.read_message().await?;
  assert!(matches!(event_msg, Message::Event { .. }), "expected Event, got: {:?}", event_msg);

  // Drop the first connection
  drop(conn1);

  // Wait for the server to process the connection drop
  monoio::time::sleep(Duration::from_secs(1)).await;

  // Second connection should still be able to list channels
  // and see that the user is still a member of the channel
  conn2
    .write_message(Message::ListChannels(ListChannelsParameters {
      id: 5678,
      page_size: None,
      page: None,
      owner: false,
    }))
    .await?;

  let list_response = conn2.read_message().await?;
  assert!(
    matches!(list_response, Message::ListChannelsAck { .. }),
    "expected ListChannelsAck, got: {:?}",
    list_response
  );

  // Verify the response includes the channel the user joined
  if let Message::ListChannelsAck(params) = list_response {
    assert_eq!(params.id, 5678);
    assert!(params.channels.contains(&channel_id), "user should still be a member of the channel");
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  Ok(())
}
