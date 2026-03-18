// SPDX-License-Identifier: BSD-3-Clause

use std::time::Duration;

use narwhal_client::S2mConfig;
use narwhal_client::monoio::s2m::S2mClient;
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_modulator::create_s2m_listener;
use narwhal_modulator::modulator::AuthResult;
use narwhal_protocol::{
  AclAction, AclType, BroadcastParameters, ChannelConfigurationParameters, GetChannelAclParameters,
  GetChannelConfigurationParameters, ListMembersParameters, Message, SetChannelAclParameters,
  SetChannelConfigurationParameters,
};
use narwhal_test_util::{
  C2sSuite, FailingChannelStore, FailingMessageLogFactory, TestModulator, assert_message, default_c2s_config,
  default_s2m_config,
};
use narwhal_util::string_atom::StringAtom;
use prometheus_client::registry::Registry;

const TEST_USER_1: &str = "test_user_1";
const TEST_USER_2: &str = "test_user_2";
const TEST_USER_3: &str = "test_user_3";

const SHARED_SECRET: &str = "test_secret";
const CHANNEL: &str = "!persisttest@localhost";

fn make_auth_modulator() -> TestModulator {
  TestModulator::new()
    .with_auth_handler(|token| async move { Ok(AuthResult::Success { username: StringAtom::from(token.as_ref()) }) })
}

async fn bootstrap_s2m(
  modulator: TestModulator,
) -> anyhow::Result<(S2mClient, narwhal_modulator::S2mListener<TestModulator>, CoreDispatcher)> {
  let mut core_dispatcher = CoreDispatcher::new(1);
  core_dispatcher.bootstrap().await?;

  let mut s2m_ln = create_s2m_listener(
    default_s2m_config(SHARED_SECRET),
    modulator,
    core_dispatcher.clone(),
    &mut Registry::default(),
  )
  .await?;
  s2m_ln.bootstrap().await?;

  let s2m_client = S2mClient::new(S2mConfig {
    address: s2m_ln.local_address().unwrap().to_string(),
    shared_secret: SHARED_SECRET.to_string(),
    ..Default::default()
  })?;

  Ok((s2m_client, s2m_ln, core_dispatcher))
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_join_channel_fails_when_store_save_fails() -> anyhow::Result<()> {
  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;

  let store = FailingChannelStore::new();
  let mlf = FailingMessageLogFactory::new();

  let mut suite =
    C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store.clone(), mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel(TEST_USER_1, CHANNEL, None, None, Some(true), None).await?;

  store.set_fail(true);

  suite.auth(TEST_USER_2, TEST_USER_2).await?;

  suite
    .write_message(
      TEST_USER_2,
      Message::JoinChannel(narwhal_protocol::JoinChannelParameters { id: 1, channel: CHANNEL.into(), on_behalf: None }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_2).await?;
  assert!(
    matches!(&reply, Message::Error(p) if p.reason.as_ref() == "INTERNAL_SERVER_ERROR"),
    "expected InternalServerError, got: {:?}",
    reply
  );

  store.set_fail(false);

  // Verify from owner's POV: TEST_USER_2 is NOT a member.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters { id: 2, channel: CHANNEL.into(), page: None, page_size: None }),
    )
    .await?;

  let members_reply = suite.read_message(TEST_USER_1).await?;
  match members_reply {
    Message::ListMembersAck(params) => {
      assert!(
        !params.members.iter().any(|m| m.as_ref().starts_with("test_user_2@")),
        "test_user_2 should NOT be a member, but found in: {:?}",
        params.members
      );
    },
    other => panic!("expected ListMembersAck, got: {:?}", other),
  }

  // Verify slot was released: TEST_USER_2 can join another channel.
  suite.auth(TEST_USER_2, TEST_USER_2).await?;
  suite.join_channel(TEST_USER_2, "!otherchannel@localhost", None).await?;

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_broadcast_fails_when_message_log_append_fails() -> anyhow::Result<()> {
  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;

  let store = FailingChannelStore::new();
  let mlf = FailingMessageLogFactory::new();

  let mut suite =
    C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf.clone()).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel(TEST_USER_1, CHANNEL, None, None, Some(true), Some(100)).await?;

  suite.auth(TEST_USER_2, TEST_USER_2).await?;
  suite.join_channel(TEST_USER_2, CHANNEL, None).await?;

  // TEST_USER_1 receives event about TEST_USER_2 joining.
  suite.ignore_reply(TEST_USER_1).await?;

  mlf.set_fail(true);

  let payload = "hello world!";
  suite
    .write_message(
      TEST_USER_1,
      Message::Broadcast(BroadcastParameters {
        id: 1,
        channel: StringAtom::from(CHANNEL),
        qos: None,
        length: payload.len() as u32,
      }),
    )
    .await?;
  suite.write_raw_bytes(TEST_USER_1, payload.as_bytes()).await?;
  suite.write_raw_bytes(TEST_USER_1, b"\n").await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  assert!(
    matches!(&reply, Message::Error(p) if p.reason.as_ref() == "INTERNAL_SERVER_ERROR"),
    "expected InternalServerError, got: {:?}",
    reply
  );

  // TEST_USER_2 will receive events from TEST_USER_1's disconnection cleanup:
  // 1) MemberLeft event for TEST_USER_1
  // 2) MemberJoined event for TEST_USER_2 as new owner (ownership transfer)
  // But TEST_USER_2 must NOT receive the broadcast payload (Message::Message).
  let msg1 = suite.read_message(TEST_USER_2).await?;
  assert!(matches!(&msg1, Message::Event(_)), "expected event, got: {:?}", msg1);

  let msg2 = suite.read_message(TEST_USER_2).await?;
  assert!(matches!(&msg2, Message::Event(_)), "expected event, got: {:?}", msg2);

  suite.expect_read_timeout(TEST_USER_2, Duration::from_secs(2)).await?;

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_set_acl_fails_when_store_save_fails() -> anyhow::Result<()> {
  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;

  let store = FailingChannelStore::new();
  let mlf = FailingMessageLogFactory::new();

  let mut suite =
    C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store.clone(), mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel(TEST_USER_1, CHANNEL, Some(10), None, Some(true), None).await?;

  // Set ACL successfully once.
  suite
    .set_channel_acl(TEST_USER_1, CHANNEL, AclType::Join, AclAction::Add, vec![StringAtom::from("carol@localhost")])
    .await?;

  store.set_fail(true);

  // Auth a fresh connection since the failing one will be closed.
  suite.auth(TEST_USER_2, TEST_USER_1).await?;

  suite
    .write_message(
      TEST_USER_2,
      Message::SetChannelAcl(SetChannelAclParameters {
        id: 3,
        channel: StringAtom::from(CHANNEL),
        r#type: StringAtom::from(AclType::Join.as_str()),
        action: StringAtom::from(AclAction::Add.as_str()),
        nids: vec![StringAtom::from("dave@localhost")],
      }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_2).await?;
  assert!(
    matches!(&reply, Message::Error(p) if p.reason.as_ref() == "INTERNAL_SERVER_ERROR"),
    "expected InternalServerError, got: {:?}",
    reply
  );

  store.set_fail(false);

  // Verify ACL unchanged from a fresh connection.
  suite.auth(TEST_USER_3, TEST_USER_1).await?;

  suite
    .write_message(
      TEST_USER_3,
      Message::GetChannelAcl(GetChannelAclParameters {
        id: 4,
        channel: StringAtom::from(CHANNEL),
        r#type: StringAtom::from(AclType::Join.as_str()),
        page: None,
        page_size: None,
      }),
    )
    .await?;

  let acl_reply = suite.read_message(TEST_USER_3).await?;
  match acl_reply {
    Message::ChannelAcl(params) => {
      assert!(
        params.nids.contains(&StringAtom::from("carol@localhost")),
        "carol should be in ACL, got: {:?}",
        params.nids
      );
      assert!(
        !params.nids.contains(&StringAtom::from("dave@localhost")),
        "dave should NOT be in ACL, got: {:?}",
        params.nids
      );
    },
    other => panic!("expected ChannelAcl, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_set_config_fails_when_store_save_fails() -> anyhow::Result<()> {
  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;

  let store = FailingChannelStore::new();
  let mlf = FailingMessageLogFactory::new();

  let mut suite =
    C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store.clone(), mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel(TEST_USER_1, CHANNEL, Some(5), Some(1024), Some(true), None).await?;

  store.set_fail(true);

  // Auth a fresh connection since the failing one will be closed.
  suite.auth(TEST_USER_2, TEST_USER_1).await?;

  suite
    .write_message(
      TEST_USER_2,
      Message::SetChannelConfiguration(SetChannelConfigurationParameters {
        id: 5,
        channel: StringAtom::from(CHANNEL),
        max_clients: Some(99),
        max_payload_size: Some(4096),
        persist: None,
        max_persist_messages: None,
        ..Default::default()
      }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_2).await?;
  assert!(
    matches!(&reply, Message::Error(p) if p.reason.as_ref() == "INTERNAL_SERVER_ERROR"),
    "expected InternalServerError, got: {:?}",
    reply
  );

  store.set_fail(false);

  // Verify config unchanged.
  suite.auth(TEST_USER_3, TEST_USER_1).await?;

  suite
    .write_message(
      TEST_USER_3,
      Message::GetChannelConfiguration(GetChannelConfigurationParameters { id: 6, channel: StringAtom::from(CHANNEL) }),
    )
    .await?;

  let config_reply = suite.read_message(TEST_USER_3).await?;
  assert_message!(
    config_reply,
    Message::ChannelConfiguration,
    ChannelConfigurationParameters {
      id: 6,
      channel: StringAtom::from(CHANNEL),
      max_clients: 5,
      max_payload_size: 1024,
      max_persist_messages: 0,
      persist: true,
    }
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

#[monoio::test(enable_timer = true)]
async fn test_c2s_leave_channel_fails_when_store_save_fails() -> anyhow::Result<()> {
  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;

  let store = FailingChannelStore::new();
  let mlf = FailingMessageLogFactory::new();

  let mut suite =
    C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store.clone(), mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel(TEST_USER_1, CHANNEL, None, None, Some(true), None).await?;

  suite.auth(TEST_USER_2, TEST_USER_2).await?;
  suite.join_channel(TEST_USER_2, CHANNEL, None).await?;

  // TEST_USER_1 receives event about TEST_USER_2 joining.
  suite.ignore_reply(TEST_USER_1).await?;

  store.set_fail(true);

  suite
    .write_message(
      TEST_USER_2,
      Message::LeaveChannel(narwhal_protocol::LeaveChannelParameters {
        id: 7,
        channel: StringAtom::from(CHANNEL),
        on_behalf: None,
      }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_2).await?;
  assert!(
    matches!(&reply, Message::Error(p) if p.reason.as_ref() == "INTERNAL_SERVER_ERROR"),
    "expected InternalServerError, got: {:?}",
    reply
  );

  // Wait for async connection cleanup to run.
  // Keep store failing so the cleanup's do_leave also fails, preserving membership.
  monoio::time::sleep(Duration::from_secs(1)).await;

  // Verify TEST_USER_2 is still in the channel.
  suite
    .write_message(
      TEST_USER_1,
      Message::ListMembers(ListMembersParameters { id: 8, channel: CHANNEL.into(), page: None, page_size: None }),
    )
    .await?;

  let members_reply = suite.read_message(TEST_USER_1).await?;
  match members_reply {
    Message::ListMembersAck(params) => {
      assert!(
        params.members.iter().any(|m| m.as_ref().starts_with("test_user_2@")),
        "test_user_2 should still be a member, got: {:?}",
        params.members
      );
    },
    other => panic!("expected ListMembersAck, got: {:?}", other),
  }

  store.set_fail(false);

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}
