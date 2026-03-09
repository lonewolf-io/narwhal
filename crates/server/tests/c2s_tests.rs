// SPDX-License-Identifier: BSD-3-Clause

use std::time::Duration;

use narwhal_protocol::EventKind::{ChannelDeleted, MemberJoined, MemberLeft};
use narwhal_protocol::{
  AclAction, AclType, BroadcastParameters, ChannelAclParameters, ConnectParameters, DeleteChannelAckParameters,
  DeleteChannelParameters, ErrorParameters, EventParameters, GetChannelAclParameters, JoinChannelAckParameters,
  JoinChannelParameters, LeaveChannelAckParameters, LeaveChannelParameters, ListChannelsAckParameters,
  ListChannelsParameters, ListMembersAckParameters, ListMembersParameters, SetChannelAclAckParameters,
  SetChannelAclParameters, SetChannelConfigurationAckParameters, SetChannelConfigurationParameters,
};
use narwhal_protocol::{IdentifyParameters, Message};
use narwhal_test_util::{C2sSuite, assert_message, default_c2s_config};
use narwhal_util::string_atom::StringAtom;

// Usernames for testing.
const TEST_USER_1: &str = "test_user_1";
const TEST_USER_2: &str = "test_user_2";
const TEST_USER_3: &str = "test_user_3";
const TEST_USER_4: &str = "test_user_4";
const TEST_USER_5: &str = "test_user_5";

#[monoio::test(enable_timer = true)]
async fn test_c2s_connect_timeout() -> anyhow::Result<()> {
  // Set the connection timeout to 50ms.
  let mut config = default_c2s_config();
  config.connect_timeout = Duration::from_millis(50);

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Connect to the server.
  let mut tls_socket = suite.tls_socket_connect().await?;

  // Wait for the connection to timeout.
  monoio::time::sleep(Duration::from_millis(250)).await;

  // Verify that the connection timed out and the server sent an error message.
  assert_message!(
    tls_socket.read_message().await?,
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

#[monoio::test(enable_timer = true)]
async fn test_c2s_authentication_timeout() -> anyhow::Result<()> {
  // Set the authentication timeout to 50ms.
  let mut config = default_c2s_config();
  config.authenticate_timeout = Duration::from_millis(50);

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Connect to the server.
  let mut tls_socket = suite.tls_socket_connect().await?;

  tls_socket.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;

  // Ignore CONNECT_ACK reply.
  let client_connected_msg = tls_socket.read_message().await?;
  assert!(matches!(client_connected_msg, Message::ConnectAck { .. }));

  // Wait for the authentication to timeout.
  monoio::time::sleep(Duration::from_millis(250)).await;

  // Verify that the server sent the proper error message.
  assert_message!(
    tls_socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::Timeout.into(),
      detail: Some(StringAtom::from("authentication timeout")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_ping_timeout() -> anyhow::Result<()> {
  // Configure keep-alive parameters.
  let mut config = default_c2s_config();
  config.keep_alive_interval = Duration::from_millis(100);
  config.min_keep_alive_interval = Duration::from_millis(100);

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Wait until ping is received.
  monoio::time::sleep(Duration::from_millis(150)).await;

  let ping_msg = suite.read_message(TEST_USER_1).await?;
  assert!(matches!(ping_msg, Message::Ping { .. }));

  // Wait for keep-alive timeout.
  monoio::time::sleep(Duration::from_millis(200)).await;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
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

#[monoio::test(enable_timer = true)]
async fn test_c2s_unknown_message() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Connect to the server.
  let mut tls_socket = suite.tls_socket_connect().await?;

  // Send an unknown message.
  tls_socket.write_raw_bytes(b"UNKNOWN\r\n").await?;

  // Read reply and verify that the server sent the proper error message.
  assert_message!(
    tls_socket.read_message().await?,
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

#[monoio::test(enable_timer = true)]
async fn test_c2s_max_connection_limit_reached() -> anyhow::Result<()> {
  // Set the maximum number of streams to 1.
  let mut config = default_c2s_config();
  config.limits.max_connections = 1;

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Establish a first connection and complete the protocol handshake.
  // This ensures the server has fully processed and counted the connection.
  let (tx, rx) = futures::channel::oneshot::channel();

  let mut tls_socket = suite.tls_socket_connect().await?;

  // Complete the CONNECT handshake to ensure connection is fully registered
  tls_socket.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;

  let connect_ack = tls_socket.read_message().await?;
  assert!(matches!(connect_ack, Message::ConnectAck { .. }));

  // Spawn task to keep the connection alive
  let _handle = monoio::spawn(async move {
    let _ = rx.await;
    tls_socket.shutdown().await.ok();
  });

  // Connect to the server again and expect an error.
  let mut tls_socket = suite.tls_socket_connect().await?;

  // Verify that the connection was rejected and the server sent an error message.
  assert_message!(
    tls_socket.read_message().await?,
    Message::Error,
    ErrorParameters {
      id: None,
      reason: narwhal_protocol::ErrorReason::ServerOverloaded.into(),
      detail: Some(StringAtom::from("max connections reached")),
    }
  );

  suite.teardown().await?;

  tx.send(()).unwrap();

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_max_message_size_exceeded() -> anyhow::Result<()> {
  // Set the maximum message size to 1024 bytes.
  let mut config = default_c2s_config();
  config.limits.max_message_size = 1024;

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Construct a message that exceeds the maximum message size.
  let message = b"PONG opt=";

  let mut buffer = Vec::from(message);
  buffer.extend(std::iter::repeat_n(b'0', 1024));

  // Write the message to the server.
  suite.write_raw_bytes(TEST_USER_1, &buffer).await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
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

#[monoio::test(enable_timer = true)]
async fn test_c2s_max_subscriptions_reached() -> anyhow::Result<()> {
  // Set the maximum number of channels per client to 1.
  let mut config = default_c2s_config();
  config.limits.max_channels_per_client = 1;

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Join to a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Join to another channel and expect an error.
  suite
    .write_message(
      TEST_USER_1,
      Message::JoinChannel(JoinChannelParameters { id: 1, channel: "!test2@localhost".into(), on_behalf: None }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  let reply = suite.read_message(TEST_USER_1).await?;
  assert_message!(
    reply,
    Message::Error,
    ErrorParameters {
      id: Some(1),
      reason: narwhal_protocol::ErrorReason::PolicyViolation.into(),
      detail: Some(StringAtom::from("subscription limit reached")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_username_in_use() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Identify the same user again and expect an error.
  let mut tls_socket = suite.tls_socket_connect().await?;

  tls_socket.write_message(Message::Connect(ConnectParameters { protocol_version: 1, heartbeat_interval: 0 })).await?;

  let client_connected_msg = tls_socket.read_message().await?;
  assert!(matches!(client_connected_msg, Message::ConnectAck { .. }));

  tls_socket.write_message(Message::Identify(IdentifyParameters { username: StringAtom::from(TEST_USER_1) })).await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    tls_socket.read_message().await?,
    Message::Error,
    ErrorParameters { id: None, reason: narwhal_protocol::ErrorReason::UsernameInUse.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_join_on_behalf() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Join on behalf of another user (test_user_2@localhost).
  suite
    .write_message(
      TEST_USER_1,
      Message::JoinChannel(JoinChannelParameters {
        id: 2,
        channel: "!test1@localhost".into(),
        on_behalf: Some(StringAtom::from("test_user_2@localhost")),
      }),
    )
    .await?;

  // Verify that the server sent the proper join channel ack message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::JoinChannelAck,
    JoinChannelAckParameters { id: 2, channel: StringAtom::from("!test1@localhost") }
  );

  // Verify that the server sent the proper event message to the joined user.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Event,
    EventParameters {
      kind: MemberJoined.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_2@localhost")),
      owner: Some(false),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_join_existing_channel() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Join to the existing channel.
  suite
    .write_message(
      TEST_USER_2,
      Message::JoinChannel(JoinChannelParameters { id: 2, channel: "!test1@localhost".into(), on_behalf: None }),
    )
    .await?;

  // Verify that the server sent the proper join channel ack message.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::JoinChannelAck,
    JoinChannelAckParameters { id: 2, channel: StringAtom::from("!test1@localhost") }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_join_full_channel() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Configure channel to allow only one client.
  suite.configure_channel(TEST_USER_1, "!test1@localhost", 1, 8192).await?;

  // Join to the existing channel.
  suite
    .write_message(
      TEST_USER_2,
      Message::JoinChannel(JoinChannelParameters { id: 2, channel: "!test1@localhost".into(), on_behalf: None }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(2), reason: narwhal_protocol::ErrorReason::ChannelIsFull.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_join_more_than_once() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Join to the same channel twice.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  suite
    .write_message(
      TEST_USER_1,
      Message::JoinChannel(JoinChannelParameters { id: 2, channel: "!test1@localhost".into(), on_behalf: None }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters { id: Some(2), reason: narwhal_protocol::ErrorReason::UserInChannel.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_leave() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Join to a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Leave from the channel.
  suite
    .write_message(
      TEST_USER_1,
      Message::LeaveChannel(LeaveChannelParameters {
        id: 2,
        channel: StringAtom::from("!test1@localhost"),
        on_behalf: None,
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::LeaveChannelAck,
    LeaveChannelAckParameters { id: 2 }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_leave_on_behalf() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Join users to the same channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_2, "!test1@localhost", None).await?;

  // Ignore new member EVENT message...
  suite.ignore_reply(TEST_USER_1).await?;

  // Leave on behalf of another user (test_user_2@localhost).
  suite
    .write_message(
      TEST_USER_1,
      Message::LeaveChannel(LeaveChannelParameters {
        id: 1,
        channel: StringAtom::from("!test1@localhost"),
        on_behalf: Some(StringAtom::from("test_user_2@localhost")),
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::LeaveChannelAck,
    LeaveChannelAckParameters { id: 1 }
  );

  // Verify that the server sent the proper event message to the left user.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Event,
    EventParameters {
      kind: MemberLeft.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_2@localhost")),
      owner: Some(false),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_leave_as_owner() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify a users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Join users to the same channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_2@localhost")).await?;

  // Ignore new member EVENT message...
  suite.ignore_reply(TEST_USER_2).await?;

  // Leave from the channel as owner.
  suite.leave_channel(TEST_USER_1, "!test1@localhost").await?;

  // Verify that the server sent the proper event messages.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Event,
    EventParameters {
      kind: MemberLeft.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_1@localhost")),
      owner: Some(true)
    }
  );

  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Event,
    EventParameters {
      kind: MemberJoined.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: Some(StringAtom::from("test_user_2@localhost")),
      owner: Some(true)
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_non_member_leave() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Join to a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Leave from the channel.
  suite
    .write_message(
      TEST_USER_2,
      Message::LeaveChannel(LeaveChannelParameters {
        id: 1,
        channel: StringAtom::from("!test1@localhost"),
        on_behalf: None,
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(1), reason: narwhal_protocol::ErrorReason::UserNotInChannel.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_list_members() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;
  suite.identify(TEST_USER_3).await?;

  // Join users to a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_2, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_3, "!test1@localhost", None).await?;

  // Ignore new member EVENT messages...
  suite.ignore_reply(TEST_USER_1).await?;
  suite.ignore_reply(TEST_USER_1).await?;

  // List members of the channel.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters {
        id: 1,
        channel: StringAtom::from("!test1@localhost"),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  // Verify that the server sent the proper list members ack message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListMembersAck,
    ListMembersAckParameters {
      id: 1,
      channel: StringAtom::from("!test1@localhost"),
      members: Vec::from(
        [
          StringAtom::from("test_user_1@localhost"),
          StringAtom::from("test_user_2@localhost"),
          StringAtom::from("test_user_3@localhost")
        ]
        .as_slice()
      ),
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_list_members_paginated() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify 5 test users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;
  suite.identify(TEST_USER_3).await?;
  suite.identify(TEST_USER_4).await?;
  suite.identify(TEST_USER_5).await?;

  // Create a channel with user 1 and join all other users to it.
  suite.join_channel(TEST_USER_1, "!testchannel@localhost", None).await?;
  suite.join_channel(TEST_USER_2, "!testchannel@localhost", None).await?;
  suite.join_channel(TEST_USER_3, "!testchannel@localhost", None).await?;
  suite.join_channel(TEST_USER_4, "!testchannel@localhost", None).await?;
  suite.join_channel(TEST_USER_5, "!testchannel@localhost", None).await?;

  // Drain events that TEST_USER_1 received when others joined
  for _ in 0..4 {
    let msg = suite.read_message(TEST_USER_1).await?;
    assert!(matches!(msg, Message::Event(_)));
  }

  // Request page 1 with page_size of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters {
        id: 1,
        channel: StringAtom::from("!testchannel@localhost"),
        page: Some(1),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 1 contains first 2 members.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListMembersAck,
    ListMembersAckParameters {
      id: 1,
      channel: StringAtom::from("!testchannel@localhost"),
      members: Vec::from(
        [StringAtom::from("test_user_1@localhost"), StringAtom::from("test_user_2@localhost")].as_slice()
      ),
      page: Some(1),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 2 with page_size of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters {
        id: 2,
        channel: StringAtom::from("!testchannel@localhost"),
        page: Some(2),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 2 contains next 2 members.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListMembersAck,
    ListMembersAckParameters {
      id: 2,
      channel: StringAtom::from("!testchannel@localhost"),
      members: Vec::from(
        [StringAtom::from("test_user_3@localhost"), StringAtom::from("test_user_4@localhost")].as_slice()
      ),
      page: Some(2),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 3 with page_size of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters {
        id: 3,
        channel: StringAtom::from("!testchannel@localhost"),
        page: Some(3),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 3 contains last member.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListMembersAck,
    ListMembersAckParameters {
      id: 3,
      channel: StringAtom::from("!testchannel@localhost"),
      members: Vec::from([StringAtom::from("test_user_5@localhost")].as_slice()),
      page: Some(3),
      page_size: Some(2),
      total_count: Some(5)
    }
  );

  // Request page 4 with page_size of 2 (beyond available data).
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters {
        id: 4,
        channel: StringAtom::from("!testchannel@localhost"),
        page: Some(4),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 4 is empty.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListMembersAck,
    ListMembersAckParameters {
      id: 4,
      channel: StringAtom::from("!testchannel@localhost"),
      members: Vec::new(),
      page: Some(4),
      page_size: Some(2),
      total_count: Some(5)
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_list_channels() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Join user to several channels.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_2, "!test2@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!test2@localhost", None).await?;

  // List all channels a user is in.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListChannels(ListChannelsParameters { id: 1, page_size: None, page: None, owner: false }),
    )
    .await?;

  // Verify that the server sent the proper list members ack message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListChannelsAck,
    ListChannelsAckParameters {
      id: 1,
      channels: Vec::from([StringAtom::from("!test1@localhost"), StringAtom::from("!test2@localhost")].as_slice()),
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_list_channels_as_owner() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Join user to several channels.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_2, "!test2@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!test2@localhost", None).await?;

  // List all channels a user is in (as owner).
  suite
    .write_message(
      TEST_USER_1,
      Message::ListChannels(ListChannelsParameters { id: 1, page_size: None, page: None, owner: true }),
    )
    .await?;

  // Verify that the server sent the proper list members ack message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListChannelsAck,
    ListChannelsAckParameters {
      id: 1,
      channels: Vec::from([StringAtom::from("!test1@localhost")].as_slice()),
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_list_channels_paginated() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify user.
  suite.identify(TEST_USER_1).await?;

  // Join user to multiple channels (create 5 channels).
  suite.join_channel(TEST_USER_1, "!channel1@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!channel2@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!channel3@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!channel4@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!channel5@localhost", None).await?;

  // Request page 1 with count of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListChannels(ListChannelsParameters { id: 1, page_size: Some(2), page: Some(1), owner: false }),
    )
    .await?;

  // Verify page 1 contains first 2 channels.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListChannelsAck,
    ListChannelsAckParameters {
      id: 1,
      channels: Vec::from(
        [StringAtom::from("!channel1@localhost"), StringAtom::from("!channel2@localhost")].as_slice()
      ),
      page: Some(1),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 2 with count of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListChannels(ListChannelsParameters { id: 2, page_size: Some(2), page: Some(2), owner: false }),
    )
    .await?;

  // Verify page 2 contains next 2 channels.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListChannelsAck,
    ListChannelsAckParameters {
      id: 2,
      channels: Vec::from(
        [StringAtom::from("!channel3@localhost"), StringAtom::from("!channel4@localhost")].as_slice()
      ),
      page: Some(2),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 3 with count of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListChannels(ListChannelsParameters { id: 3, page_size: Some(2), page: Some(3), owner: false }),
    )
    .await?;

  // Verify page 3 contains last channel.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListChannelsAck,
    ListChannelsAckParameters {
      id: 3,
      channels: Vec::from([StringAtom::from("!channel5@localhost")].as_slice()),
      page: Some(3),
      page_size: Some(2),
      total_count: Some(5)
    }
  );

  // Request page 4 with count of 2 (beyond available data).
  suite
    .write_message(
      TEST_USER_1,
      Message::ListChannels(ListChannelsParameters { id: 4, page_size: Some(2), page: Some(4), owner: false }),
    )
    .await?;

  // Verify page 4 is empty.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListChannelsAck,
    ListChannelsAckParameters { id: 4, channels: Vec::new(), page: Some(4), page_size: Some(2), total_count: Some(5) }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_leave_from_non_existing_channel() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify a user.
  suite.identify(TEST_USER_1).await?;

  // Leave from the channel.
  suite
    .write_message(
      TEST_USER_1,
      Message::LeaveChannel(LeaveChannelParameters {
        id: 2,
        channel: StringAtom::from("!test1@localhost"),
        on_behalf: None,
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters { id: Some(2), reason: narwhal_protocol::ErrorReason::ChannelNotFound.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_configuration() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Set the channel configuration.
  suite
    .write_message(
      TEST_USER_1,
      Message::SetChannelConfiguration(SetChannelConfigurationParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        max_clients: 25,
        max_payload_size: 8192,
      }),
    )
    .await?;

  // Verify that the server sent the proper channel configuration ack message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::SetChannelConfigurationAck,
    SetChannelConfigurationAckParameters { id: 1234 }
  );

  // Get the channel configuration.
  // Set the channel configuration again to test idempotency.
  suite
    .write_message(
      TEST_USER_1,
      Message::SetChannelConfiguration(SetChannelConfigurationParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        max_clients: 25,
        max_payload_size: 8192,
      }),
    )
    .await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::SetChannelConfigurationAck,
    SetChannelConfigurationAckParameters { id: 1234 }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_unauthorized_channel_configuration() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Set the channel configuration as an unauthorized user.
  suite
    .write_message(
      TEST_USER_2,
      Message::SetChannelConfiguration(SetChannelConfigurationParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        max_clients: 25,
        max_payload_size: 8192,
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(1234), reason: narwhal_protocol::ErrorReason::Forbidden.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_max_clients_configuration_limit() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Set the channel configuration.
  suite
    .write_message(
      TEST_USER_1,
      Message::SetChannelConfiguration(SetChannelConfigurationParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        max_clients: 200,
        max_payload_size: 8192,
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters {
      id: Some(1234),
      reason: narwhal_protocol::ErrorReason::BadRequest.into(),
      detail: Some(StringAtom::from("max_clients exceeds server established limit")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_max_payload_configuration_limit() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Set the channel configuration.
  suite
    .write_message(
      TEST_USER_1,
      Message::SetChannelConfiguration(SetChannelConfigurationParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        max_clients: 25,
        max_payload_size: 1_000 * 1024,
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters {
      id: Some(1234),
      reason: narwhal_protocol::ErrorReason::BadRequest.into(),
      detail: Some(StringAtom::from("max_payload_size exceeds server established limit")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_acl() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Join users to a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_2, "!test1@localhost", None).await?;

  // Ignore new member EVENT message...
  suite.ignore_reply(TEST_USER_1).await?;

  // Set the channel ACL for join permissions
  let set_channel_acl_join = SetChannelAclParameters {
    id: 1234,
    channel: StringAtom::from("!test1@localhost"),
    r#type: StringAtom::from("join"),
    action: StringAtom::from("add"),
    nids: Vec::from([StringAtom::from("test_user_2@localhost")].as_slice()),
  };
  suite.write_message(TEST_USER_1, Message::SetChannelAcl(set_channel_acl_join)).await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::SetChannelAclAck,
    SetChannelAclAckParameters { id: 1234 }
  );

  // Set the channel ACL for read permissions
  let set_channel_acl_read = SetChannelAclParameters {
    id: 1235,
    channel: StringAtom::from("!test1@localhost"),
    r#type: StringAtom::from("read"),
    action: StringAtom::from("add"),
    nids: Vec::from([StringAtom::from("example.com")].as_slice()),
  };
  suite.write_message(TEST_USER_1, Message::SetChannelAcl(set_channel_acl_read)).await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::SetChannelAclAck,
    SetChannelAclAckParameters { id: 1235 }
  );

  // Test that TEST_USER_2 cannot set ACL (not the owner)
  let set_channel_acl_unauthorized = SetChannelAclParameters {
    id: 1236,
    channel: StringAtom::from("!test1@localhost"),
    r#type: StringAtom::from("publish"),
    action: StringAtom::from("add"),
    nids: Vec::from([StringAtom::from("test_user_2@localhost")].as_slice()),
  };
  suite.write_message(TEST_USER_2, Message::SetChannelAcl(set_channel_acl_unauthorized)).await?;

  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(1236), reason: narwhal_protocol::ErrorReason::Forbidden.into(), detail: None }
  );

  // Retrieve the channel ACL for join type
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 1237,
        channel: StringAtom::from("!test1@localhost"),
        r#type: StringAtom::from("join"),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 1237,
      channel: StringAtom::from("!test1@localhost"),
      r#type: StringAtom::from("join"),
      nids: Vec::from([StringAtom::from("test_user_2@localhost")].as_slice()),
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  // Retrieve the channel ACL for read type
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 1238,
        channel: StringAtom::from("!test1@localhost"),
        r#type: StringAtom::from("read"),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 1238,
      channel: StringAtom::from("!test1@localhost"),
      r#type: StringAtom::from("read"),
      nids: Vec::from([StringAtom::from("example.com")].as_slice()),
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_acl_paginated() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify test users.
  suite.identify(TEST_USER_1).await?;

  // Create a channel with TEST_USER_1.
  suite.join_channel(TEST_USER_1, "!testchannel@localhost", None).await?;

  // Add multiple nids to the channel ACL (join type).
  let set_channel_acl = SetChannelAclParameters {
    id: 1,
    channel: StringAtom::from("!testchannel@localhost"),
    r#type: StringAtom::from("join"),
    action: StringAtom::from("add"),
    nids: Vec::from(
      [
        StringAtom::from("user1@example.com"),
        StringAtom::from("user2@example.com"),
        StringAtom::from("user3@example.com"),
        StringAtom::from("user4@example.com"),
        StringAtom::from("user5@example.com"),
      ]
      .as_slice(),
    ),
  };
  suite.write_message(TEST_USER_1, Message::SetChannelAcl(set_channel_acl)).await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::SetChannelAclAck,
    SetChannelAclAckParameters { id: 1 }
  );

  // Request page 1 with page_size of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 2,
        channel: StringAtom::from("!testchannel@localhost"),
        r#type: StringAtom::from("join"),
        page: Some(1),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 1 contains first 2 nids.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 2,
      channel: StringAtom::from("!testchannel@localhost"),
      r#type: StringAtom::from("join"),
      nids: Vec::from([StringAtom::from("user1@example.com"), StringAtom::from("user2@example.com"),].as_slice()),
      page: Some(1),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 2 with page_size of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 3,
        channel: StringAtom::from("!testchannel@localhost"),
        r#type: StringAtom::from("join"),
        page: Some(2),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 2 contains next 2 nids.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 3,
      channel: StringAtom::from("!testchannel@localhost"),
      r#type: StringAtom::from("join"),
      nids: Vec::from([StringAtom::from("user3@example.com"), StringAtom::from("user4@example.com"),].as_slice()),
      page: Some(2),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 3 with page_size of 2.
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 4,
        channel: StringAtom::from("!testchannel@localhost"),
        r#type: StringAtom::from("join"),
        page: Some(3),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 3 contains last nid.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 4,
      channel: StringAtom::from("!testchannel@localhost"),
      r#type: StringAtom::from("join"),
      nids: Vec::from([StringAtom::from("user5@example.com")].as_slice()),
      page: Some(3),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request page 4 with page_size of 2 (beyond available data).
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 5,
        channel: StringAtom::from("!testchannel@localhost"),
        r#type: StringAtom::from("join"),
        page: Some(4),
        page_size: Some(2),
      }),
    )
    .await?;

  // Verify page 4 is empty.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 5,
      channel: StringAtom::from("!testchannel@localhost"),
      r#type: StringAtom::from("join"),
      nids: Vec::new(),
      page: Some(4),
      page_size: Some(2),
      total_count: Some(5),
    }
  );

  // Request without pagination to verify we get all nids.
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 6,
        channel: StringAtom::from("!testchannel@localhost"),
        r#type: StringAtom::from("join"),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  // Verify we get all nids when no pagination is specified.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ChannelAcl,
    ChannelAclParameters {
      id: 6,
      channel: StringAtom::from("!testchannel@localhost"),
      r#type: StringAtom::from("join"),
      nids: Vec::from(
        [
          StringAtom::from("user1@example.com"),
          StringAtom::from("user2@example.com"),
          StringAtom::from("user3@example.com"),
          StringAtom::from("user4@example.com"),
          StringAtom::from("user5@example.com"),
        ]
        .as_slice()
      ),
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_acl_max_entries() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Set channels max clients to 2.
  suite.configure_channel(TEST_USER_1, "!test1@localhost", 2, 8192).await?;

  // Set the channel ACL (exceeding the maximum number of entries).
  suite
    .write_message(
      TEST_USER_1,
      Message::SetChannelAcl(SetChannelAclParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        r#type: StringAtom::from("join"),
        action: StringAtom::from("add"),
        nids: Vec::from(
          [
            StringAtom::from("test_user_1@localhost"),
            StringAtom::from("test_user_2@localhost"),
            StringAtom::from("test_user_3@localhost"),
          ]
          .as_slice(),
        ),
      }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters {
      id: Some(1234),
      reason: narwhal_protocol::ErrorReason::PolicyViolation.into(),
      detail: Some(StringAtom::from("ACL allow list exceeds max entries")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_acl_join_deny() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Set the channel ACL for join permissions.
  suite
    .set_channel_acl(
      TEST_USER_1,
      "!test1@localhost",
      AclType::Join,
      AclAction::Add,
      Vec::from([StringAtom::from("test_user_99@localhost")].as_slice()),
    )
    .await?;

  // Join to the channel.
  suite
    .write_message(
      TEST_USER_2,
      Message::JoinChannel(JoinChannelParameters { id: 2, channel: "!test1@localhost".into(), on_behalf: None }),
    )
    .await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(2), reason: narwhal_protocol::ErrorReason::NotAllowed.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_broadcast() -> anyhow::Result<()> {
  const CONTENT_LENGTH: u32 = 12;

  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;
  suite.identify(TEST_USER_3).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;
  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_2@localhost")).await?;

  // Ignore new member EVENT message.
  suite.ignore_reply(TEST_USER_2).await?;

  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_3@localhost")).await?;

  // Ignore new member EVENT messages...
  suite.ignore_reply(TEST_USER_2).await?;
  suite.ignore_reply(TEST_USER_3).await?;

  // Broadcast a message to the channel...
  suite.broadcast(TEST_USER_1, "!test1@localhost", "Hello world!").await?;

  // Verify that the server sent the proper message to the other users.
  match suite.read_message(TEST_USER_2).await? {
    Message::Message(params) => {
      assert_eq!(params.from, StringAtom::from("test_user_1@localhost"));
      assert_eq!(params.channel, StringAtom::from("!test1@localhost"));
      assert_eq!(params.length, CONTENT_LENGTH);
      assert_eq!(params.seq, 1);
      assert!(params.timestamp > 0, "timestamp must be non-zero");
    },
    msg => panic!("expected Message::Message, got {:?}", msg),
  }

  let mut payload: [u8; CONTENT_LENGTH as usize] = Default::default();
  suite.read_raw_bytes(TEST_USER_2, payload.as_mut_slice()).await?;

  match suite.read_message(TEST_USER_3).await? {
    Message::Message(params) => {
      assert_eq!(params.from, StringAtom::from("test_user_1@localhost"));
      assert_eq!(params.channel, StringAtom::from("!test1@localhost"));
      assert_eq!(params.length, CONTENT_LENGTH);
      assert_eq!(params.seq, 1);
      assert!(params.timestamp > 0, "timestamp must be non-zero");
    },
    msg => panic!("expected Message::Message, got {:?}", msg),
  }

  let mut payload: [u8; CONTENT_LENGTH as usize] = Default::default();
  suite.read_raw_bytes(TEST_USER_3, payload.as_mut_slice()).await?;

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_broadcast_invalid_payload() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;

  // Create a channel.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Broadcast a message to the channel...
  suite
    .write_message(
      TEST_USER_1,
      Message::Broadcast(BroadcastParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        qos: None,
        length: 1,
      }),
    )
    .await?;

  // ... along with an invalid payload.
  suite.write_raw_bytes(TEST_USER_1, b"AAAA\n").await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters {
      id: Some(1234),
      reason: narwhal_protocol::ErrorReason::BadRequest.into(),
      detail: Some(StringAtom::from("invalid payload format"))
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_acl_publish_deny() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // Create a channel and join users to it.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_2@localhost")).await?;

  // Ignore new member EVENT message.
  suite.ignore_reply(TEST_USER_2).await?;

  // Set the channel ACL for publish permissions.
  suite
    .set_channel_acl(
      TEST_USER_1,
      "!test1@localhost",
      AclType::Publish,
      AclAction::Add,
      Vec::from([StringAtom::from("test_user_99@localhost")].as_slice()),
    )
    .await?;

  // Broadcast a message to the channel...
  suite
    .write_message(
      TEST_USER_2,
      Message::Broadcast(BroadcastParameters {
        id: 1234,
        channel: StringAtom::from("!test1@localhost"),
        qos: None,
        length: 1,
      }),
    )
    .await?;

  let buffer = b"0\n".as_slice();
  suite.write_raw_bytes(TEST_USER_2, buffer).await?;

  // Verify that the server sent the proper error message.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(1234), reason: narwhal_protocol::ErrorReason::NotAllowed.into(), detail: None }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_channel_acl_read_deny() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;
  suite.identify(TEST_USER_3).await?;

  // Create a channel and join users to it.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_2@localhost")).await?;

  // Ignore new member EVENT message.
  suite.ignore_reply(TEST_USER_2).await?;

  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_3@localhost")).await?;

  // Ignore new member EVENT messages.
  suite.ignore_reply(TEST_USER_2).await?;
  suite.ignore_reply(TEST_USER_3).await?;

  // Set the channel ACL for read permissions.
  suite
    .set_channel_acl(
      TEST_USER_1,
      "!test1@localhost",
      AclType::Read,
      AclAction::Add,
      Vec::from([StringAtom::from("test_user_2@localhost")].as_slice()),
    )
    .await?;

  // Broadcast a message to the channel...
  suite.broadcast(TEST_USER_1, "!test1@localhost", "Hello world!").await?;

  // Verify that the server sent the proper message to the other users.
  match suite.read_message(TEST_USER_2).await? {
    Message::Message(params) => {
      assert_eq!(params.from, StringAtom::from("test_user_1@localhost"));
      assert_eq!(params.channel, StringAtom::from("!test1@localhost"));
      assert_eq!(params.length, 12);
      assert_eq!(params.seq, 1);
      assert!(params.timestamp > 0, "timestamp must be non-zero");
    },
    msg => panic!("expected Message::Message, got {:?}", msg),
  }

  // Expect a read timeout for the other user.
  suite.expect_read_timeout(TEST_USER_3, Duration::from_secs(1)).await?;

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_response_too_large() -> anyhow::Result<()> {
  let mut config = default_c2s_config();
  config.limits.max_message_size = 512; // Set a max message size to trigger RESPONSE_TOO_LARGE error.
  config.limits.max_clients_per_channel = 500;

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  // Identify test user.
  suite.identify(TEST_USER_1).await?;

  // Create a channel with TEST_USER_1.
  suite.join_channel(TEST_USER_1, "!testchannel@localhost", None).await?;

  // Add NIDs to the channel ACL to definitely exceed the 512 byte message size limit.
  for batch in 0..50 {
    let mut nids = Vec::new();
    for i in 1..=5 {
      let nid_num = batch * 5 + i;
      nids.push(StringAtom::from(format!("user{}@example.com", nid_num)));
    }

    let set_channel_acl = SetChannelAclParameters {
      id: batch + 1,
      channel: StringAtom::from("!testchannel@localhost"),
      r#type: StringAtom::from("join"),
      action: StringAtom::from("add"),
      nids,
    };
    suite.write_message(TEST_USER_1, Message::SetChannelAcl(set_channel_acl)).await?;

    assert_message!(
      suite.read_message(TEST_USER_1).await?,
      Message::SetChannelAclAck,
      SetChannelAclAckParameters { id: batch + 1 }
    );
  }

  // Request the ACL without pagination - this should exceed the message size limit.
  suite
    .write_message(
      TEST_USER_1,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 100,
        channel: StringAtom::from("!testchannel@localhost"),
        r#type: StringAtom::from("join"),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  // Verify that the server sent RESPONSE_TOO_LARGE error.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::Error,
    ErrorParameters {
      id: Some(100),
      reason: narwhal_protocol::ErrorReason::ResponseTooLarge.into(),
      detail: Some(StringAtom::from("response exceeded maximum message size")),
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_delete_channel_as_owner() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // TEST_USER_1 joins the channel (becomes owner).
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // TEST_USER_1 joins TEST_USER_2 on behalf.
  suite.join_channel(TEST_USER_1, "!test1@localhost", Some("test_user_2@localhost")).await?;

  // Ignore the MemberJoined event on TEST_USER_2.
  suite.ignore_reply(TEST_USER_2).await?;

  // Owner deletes the channel.
  suite
    .write_message(
      TEST_USER_1,
      Message::DeleteChannel(DeleteChannelParameters { id: 1, channel: StringAtom::from("!test1@localhost") }),
    )
    .await?;

  // Verify that the owner receives the ack.
  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::DeleteChannelAck,
    DeleteChannelAckParameters { id: 1 }
  );

  // Verify that TEST_USER_2 receives the CHANNEL_DELETED event.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Event,
    EventParameters {
      kind: ChannelDeleted.into(),
      channel: Some(StringAtom::from("!test1@localhost")),
      nid: None,
      owner: None,
    }
  );

  // Verify the channel no longer exists by re-joining (should succeed as new owner).
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_delete_channel_as_non_owner() -> anyhow::Result<()> {
  let mut suite = C2sSuite::new(default_c2s_config()).await?;
  suite.setup().await?;

  // Identify users.
  suite.identify(TEST_USER_1).await?;
  suite.identify(TEST_USER_2).await?;

  // TEST_USER_1 joins the channel (becomes owner).
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // TEST_USER_2 joins the channel (not the owner).
  suite.join_channel(TEST_USER_2, "!test1@localhost", None).await?;

  // Ignore the MemberJoined event on TEST_USER_1.
  suite.ignore_reply(TEST_USER_1).await?;

  // Non-owner attempts to delete the channel.
  suite
    .write_message(
      TEST_USER_2,
      Message::DeleteChannel(DeleteChannelParameters { id: 1, channel: StringAtom::from("!test1@localhost") }),
    )
    .await?;

  // Verify that the non-owner receives a Forbidden error.
  assert_message!(
    suite.read_message(TEST_USER_2).await?,
    Message::Error,
    ErrorParameters { id: Some(1), reason: narwhal_protocol::ErrorReason::Forbidden.into(), detail: None }
  );

  // Verify that the channel still exists by listing members.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters {
        id: 2,
        channel: StringAtom::from("!test1@localhost"),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  assert_message!(
    suite.read_message(TEST_USER_1).await?,
    Message::ListMembersAck,
    ListMembersAckParameters {
      id: 2,
      channel: StringAtom::from("!test1@localhost"),
      members: vec![StringAtom::from("test_user_1@localhost"), StringAtom::from("test_user_2@localhost")],
      page: None,
      page_size: None,
      total_count: None,
    }
  );

  suite.teardown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_max_channels_reached() -> anyhow::Result<()> {
  let mut config = default_c2s_config();
  config.limits.max_channels = 1;

  let mut suite = C2sSuite::new(config).await?;
  suite.setup().await?;

  suite.identify(TEST_USER_1).await?;

  // Create the first channel — should succeed.
  suite.join_channel(TEST_USER_1, "!test1@localhost", None).await?;

  // Try to create a second channel — should fail with ResourceLimitReached.
  suite
    .write_message(
      TEST_USER_1,
      Message::JoinChannel(JoinChannelParameters { id: 1, channel: "!test2@localhost".into(), on_behalf: None }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  assert_message!(
    reply,
    Message::Error,
    ErrorParameters {
      id: Some(1),
      reason: narwhal_protocol::ErrorReason::ResourceLimitReached.into(),
      detail: Some(StringAtom::from("maximum channels reached")),
    }
  );

  // Verify the client is still connected — send a valid request.
  suite.leave_channel(TEST_USER_1, "!test1@localhost").await?;

  suite.teardown().await?;

  Ok(())
}
