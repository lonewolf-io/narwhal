// SPDX-License-Identifier: BSD-3-Clause

use narwhal_client::S2mConfig;
use narwhal_client::compio::s2m::S2mClient;
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_modulator::create_s2m_listener;
use narwhal_modulator::modulator::AuthResult;
use narwhal_protocol::{ListChannelsParameters, Message};
use narwhal_server::channel::NoopMessageLogFactory;
use narwhal_server::channel::file_store::FileChannelStore;
use narwhal_test_util::{C2sSuite, TestModulator, default_c2s_config, default_s2m_config};
use narwhal_util::string_atom::StringAtom;
use prometheus_client::registry::Registry;

const TEST_USER_1: &str = "test_user_1";
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

/// Verifies that a channel persisted to disk via `FileChannelStore` survives a server restart.
#[compio::test]
async fn test_file_channel_store_persists_and_restores_channel() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  // --- First boot: create a persistent channel ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = NoopMessageLogFactory;

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;
    suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
    suite.configure_channel(TEST_USER_1, CHANNEL, None, None, None, Some(true)).await?;

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  // --- Second boot: verify the channel was restored from disk ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = NoopMessageLogFactory;

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;

    suite
      .write_message(
        TEST_USER_1,
        Message::ListChannels(ListChannelsParameters { id: 1, page: None, page_size: None, owner: false }),
      )
      .await?;

    let reply = suite.read_message(TEST_USER_1).await?;
    match reply {
      Message::ListChannelsAck(params) => {
        assert!(
          params.channels.contains(&StringAtom::from(CHANNEL)),
          "persisted channel should be restored from disk, got: {:?}",
          params.channels
        );
      },
      other => panic!("expected ListChannelsAck, got: {:?}", other),
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  Ok(())
}

/// Verifies that deleting a persistent channel removes it from disk,
/// so it is not restored on the next boot.
#[compio::test]
async fn test_file_channel_store_deleted_channel_not_restored() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  // --- First boot: create a persistent channel, then delete it ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = NoopMessageLogFactory;

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;
    suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
    suite.configure_channel(TEST_USER_1, CHANNEL, None, None, None, Some(true)).await?;

    // Toggle persistence off — should clean up disk storage.
    suite.configure_channel(TEST_USER_1, CHANNEL, None, None, None, Some(false)).await?;

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  // --- Second boot: verify the channel is NOT restored ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = NoopMessageLogFactory;

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;

    suite
      .write_message(
        TEST_USER_1,
        Message::ListChannels(ListChannelsParameters { id: 1, page: None, page_size: None, owner: false }),
      )
      .await?;

    let reply = suite.read_message(TEST_USER_1).await?;
    match reply {
      Message::ListChannelsAck(params) => {
        assert!(
          !params.channels.contains(&StringAtom::from(CHANNEL)),
          "deleted channel should NOT be restored, got: {:?}",
          params.channels
        );
      },
      other => panic!("expected ListChannelsAck, got: {:?}", other),
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  Ok(())
}
