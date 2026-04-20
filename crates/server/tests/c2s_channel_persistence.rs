// SPDX-License-Identifier: BSD-3-Clause

use narwhal_client::S2mConfig;
use narwhal_client::compio::s2m::S2mClient;
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_modulator::create_s2m_listener;
use narwhal_modulator::modulator::{AuthResult, ForwardBroadcastPayloadResult};
use narwhal_protocol::{
  ChannelSeqParameters, ErrorParameters, ErrorReason, HistoryParameters, ListChannelsParameters, Message,
};
use narwhal_server::channel::NoopMessageLogFactory;
use narwhal_server::channel::file_message_log::{FileMessageLogFactory, MessageLogMetrics};
use narwhal_server::channel::file_store::FileChannelStore;
use narwhal_test_util::{C2sSuite, TestModulator, default_c2s_config, default_s2m_config};
use narwhal_util::string_atom::StringAtom;
use prometheus_client::registry::Registry;

const TEST_USER_1: &str = "test_user_1";
const TEST_USER_2: &str = "test_user_2";
const SHARED_SECRET: &str = "test_secret";
const CHANNEL: &str = "!persisttest@localhost";

fn make_auth_modulator() -> TestModulator {
  TestModulator::new()
    .with_auth_handler(|token| async move { Ok(AuthResult::Success { username: StringAtom::from(token.as_ref()) }) })
    .with_forward_message_payload_handler(|_payload, _from, _channel_handler| async {
      Ok(ForwardBroadcastPayloadResult::Valid)
    })
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
async fn test_c2s_file_channel_store_persists_and_restores_channel() -> anyhow::Result<()> {
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
async fn test_c2s_file_channel_store_deleted_channel_not_restored() -> anyhow::Result<()> {
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

/// Verifies that HISTORY retrieves messages persisted by the `FileMessageLog`.
#[compio::test]
async fn test_c2s_history_retrieves_persisted_messages() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;

  // Enable persistence with immediate flush and generous limits.
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  // Broadcast 5 messages. The sender is excluded from delivery, but they are persisted.
  let payloads: Vec<String> = (1..=5).map(|i| format!("hello_{i}")).collect();
  for payload in &payloads {
    suite.broadcast(TEST_USER_1, CHANNEL, payload).await?;
  }

  // Request history: all 5 messages starting from seq 1.
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 42,
        history_id: StringAtom::from("hist_001"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  // Read back the 5 MESSAGE frames (each followed by payload bytes).
  let mut received_payloads = Vec::new();
  for _ in 0..5 {
    let msg = suite.read_message(TEST_USER_1).await?;
    match msg {
      Message::Message(params) => {
        assert_eq!(params.channel, StringAtom::from(CHANNEL));
        assert_eq!(params.history_id, Some(StringAtom::from("hist_001")));
        assert!(params.seq > 0);
        assert!(params.timestamp > 0);

        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
        // Consume the trailing newline after the payload.
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
        received_payloads.push(String::from_utf8(buf)?);
      },
      other => panic!("expected Message::Message, got: {:?}", other),
    }
  }

  // Verify payloads match what was broadcast.
  assert_eq!(received_payloads, payloads);

  // Read HISTORY_ACK (sent after all MESSAGE frames).
  let ack = suite.read_message(TEST_USER_1).await?;
  match ack {
    Message::HistoryAck(params) => {
      assert_eq!(params.id, 42);
      assert_eq!(params.history_id, StringAtom::from("hist_001"));
      assert_eq!(params.channel, StringAtom::from(CHANNEL));
      assert_eq!(params.count, 5);
    },
    other => panic!("expected HistoryAck, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that CHAN_SEQ returns the correct first/last sequence numbers.
#[compio::test]
async fn test_c2s_chan_seq_returns_sequence_range() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;

  // Enable persistence with immediate flush.
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  // Broadcast 3 messages.
  for i in 1..=3 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("msg_{i}")).await?;
  }

  // Request CHAN_SEQ.
  suite
    .write_message(
      TEST_USER_1,
      Message::ChannelSeq(ChannelSeqParameters { id: 99, channel: StringAtom::from(CHANNEL) }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  match reply {
    Message::ChannelSeqAck(params) => {
      assert_eq!(params.id, 99);
      assert_eq!(params.channel, StringAtom::from(CHANNEL));
      assert_eq!(params.first_seq, 1);
      assert_eq!(params.last_seq, 3);
    },
    other => panic!("expected ChannelSeqAck, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that HISTORY with a partial range returns only the requested messages.
#[compio::test]
async fn test_c2s_history_partial_range() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;

  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  // Broadcast 10 messages.
  for i in 1..=10 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("msg_{i}")).await?;
  }

  // Request history starting from seq 6, limit 3.
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 7,
        history_id: StringAtom::from("partial"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 6,
        limit: 3,
        direction: None,
      }),
    )
    .await?;

  // Should receive exactly 3 messages (seq 6, 7, 8).
  let mut seqs = Vec::new();
  for _ in 0..3 {
    let msg = suite.read_message(TEST_USER_1).await?;
    match msg {
      Message::Message(params) => {
        seqs.push(params.seq);
        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
        // Consume the trailing newline after the payload.
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
      },
      other => panic!("expected Message::Message, got: {:?}", other),
    }
  }
  assert_eq!(seqs, vec![6, 7, 8]);

  let ack = suite.read_message(TEST_USER_1).await?;
  match ack {
    Message::HistoryAck(params) => {
      assert_eq!(params.count, 3);
    },
    other => panic!("expected HistoryAck, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that persisted messages survive a full server restart.
#[compio::test]
async fn test_c2s_history_survives_restart() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  // --- First boot: create channel, broadcast messages, shut down ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;
    suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
    suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

    for i in 1..=3 {
      suite.broadcast(TEST_USER_1, CHANNEL, &format!("persist_{i}")).await?;
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  // --- Second boot: request HISTORY from a fresh log instance ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;

    suite
      .write_message(
        TEST_USER_1,
        Message::History(HistoryParameters {
          id: 1,
          history_id: StringAtom::from("restart"),
          channel: StringAtom::from(CHANNEL),
          from_seq: 1,
          limit: 100,
          direction: None,
        }),
      )
      .await?;

    let mut payloads = Vec::new();
    for _ in 0..3 {
      let msg = suite.read_message(TEST_USER_1).await?;
      match msg {
        Message::Message(params) => {
          let mut buf = vec![0u8; params.length as usize];
          suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
          let mut nl = [0u8; 1];
          suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
          payloads.push(String::from_utf8(buf)?);
        },
        other => panic!("expected Message::Message, got: {:?}", other),
      }
    }
    assert_eq!(payloads, vec!["persist_1", "persist_2", "persist_3"]);

    let ack = suite.read_message(TEST_USER_1).await?;
    match ack {
      Message::HistoryAck(params) => assert_eq!(params.count, 3),
      other => panic!("expected HistoryAck, got: {:?}", other),
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  Ok(())
}

/// Verifies that HISTORY on an empty persistent channel returns count=0.
#[compio::test]
async fn test_c2s_history_empty_channel() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 10,
        history_id: StringAtom::from("empty"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  let ack = suite.read_message(TEST_USER_1).await?;
  match ack {
    Message::HistoryAck(params) => {
      assert_eq!(params.id, 10);
      assert_eq!(params.count, 0);
    },
    other => panic!("expected HistoryAck, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that CHAN_SEQ on an empty persistent channel returns first_seq=0, last_seq=0.
#[compio::test]
async fn test_c2s_chan_seq_empty_channel() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  suite
    .write_message(
      TEST_USER_1,
      Message::ChannelSeq(ChannelSeqParameters { id: 20, channel: StringAtom::from(CHANNEL) }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  match reply {
    Message::ChannelSeqAck(params) => {
      assert_eq!(params.id, 20);
      assert_eq!(params.first_seq, 0);
      assert_eq!(params.last_seq, 0);
    },
    other => panic!("expected ChannelSeqAck, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that CHAN_SEQ reflects eviction: first_seq advances past evicted messages.
#[compio::test]
async fn test_c2s_chan_seq_after_eviction() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  // Use tiny segments (256 bytes) so eviction kicks in with small messages.
  let mlf = FileMessageLogFactory::with_segment_max(data_dir.clone(), 4096, 256, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;

  // max_persist_messages=5 so older messages get evicted.
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(5), Some(true), Some(0)).await?;

  // Broadcast 20 messages — only the last ~5 should be retained.
  for i in 1..=20 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("evict_{i}")).await?;
  }

  suite
    .write_message(
      TEST_USER_1,
      Message::ChannelSeq(ChannelSeqParameters { id: 30, channel: StringAtom::from(CHANNEL) }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  match reply {
    Message::ChannelSeqAck(params) => {
      assert_eq!(params.id, 30);
      assert!(params.first_seq > 1, "first_seq should advance after eviction, got {}", params.first_seq);
      assert_eq!(params.last_seq, 20);
    },
    other => panic!("expected ChannelSeqAck, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that HISTORY starting from seq 1 after eviction returns from the first retained seq.
#[compio::test]
async fn test_c2s_history_after_eviction() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  // Use tiny segments (256 bytes) so eviction kicks in with small messages.
  let mlf = FileMessageLogFactory::with_segment_max(data_dir.clone(), 4096, 256, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;

  // max_persist_messages=5.
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(5), Some(true), Some(0)).await?;

  for i in 1..=20 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("evict_{i}")).await?;
  }

  // Request history from seq 1 with a large limit.
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 40,
        history_id: StringAtom::from("after_evict"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  // Read all returned MESSAGE frames.
  let mut seqs = Vec::new();
  loop {
    let msg = suite.read_message(TEST_USER_1).await?;
    match msg {
      Message::Message(params) => {
        seqs.push(params.seq);
        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
      },
      Message::HistoryAck(params) => {
        assert_eq!(params.id, 40);
        assert_eq!(params.count, seqs.len() as u32);
        break;
      },
      other => panic!("expected Message or HistoryAck, got: {:?}", other),
    }
  }

  // Evicted messages should not appear.
  assert!(!seqs.is_empty(), "should have some retained messages");
  assert!(seqs[0] > 1, "first returned seq should be past evicted range, got {}", seqs[0]);
  assert_eq!(*seqs.last().unwrap(), 20);

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that user B can retrieve messages broadcast by user A via HISTORY.
#[compio::test]
async fn test_c2s_history_cross_user() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  // User 1 creates the channel and enables persistence.
  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  // User 2 joins.
  suite.auth(TEST_USER_2, TEST_USER_2).await?;
  suite.join_channel(TEST_USER_2, CHANNEL, None).await?;
  // Drain the MEMBER_JOINED event that user 1 receives.
  suite.ignore_reply(TEST_USER_1).await?;

  // User 1 broadcasts 3 messages (user 2 receives them live — drain those).
  for i in 1..=3 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("from_u1_{i}")).await?;
    // User 2 receives the live MESSAGE frame.
    let live = suite.read_message(TEST_USER_2).await?;
    assert!(matches!(live, Message::Message { .. }));
    if let Message::Message(params) = live {
      let mut buf = vec![0u8; params.length as usize];
      suite.read_raw_bytes(TEST_USER_2, &mut buf).await?;
      let mut nl = [0u8; 1];
      suite.read_raw_bytes(TEST_USER_2, &mut nl).await?;
    }
  }

  // User 2 requests HISTORY.
  suite
    .write_message(
      TEST_USER_2,
      Message::History(HistoryParameters {
        id: 50,
        history_id: StringAtom::from("cross"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  let mut from_fields = Vec::new();
  let mut payloads = Vec::new();
  for _ in 0..3 {
    let msg = suite.read_message(TEST_USER_2).await?;
    match msg {
      Message::Message(params) => {
        from_fields.push(params.from.to_string());
        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_2, &mut buf).await?;
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_2, &mut nl).await?;
        payloads.push(String::from_utf8(buf)?);
      },
      other => panic!("expected Message::Message, got: {:?}", other),
    }
  }

  let ack = suite.read_message(TEST_USER_2).await?;
  match ack {
    Message::HistoryAck(params) => assert_eq!(params.count, 3),
    other => panic!("expected HistoryAck, got: {:?}", other),
  }

  // Verify the `from` field shows user 1.
  for from in &from_fields {
    assert!(from.contains("test_user_1"), "expected from to be test_user_1, got: {}", from);
  }
  assert_eq!(payloads, vec!["from_u1_1", "from_u1_2", "from_u1_3"]);

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

const CHANNEL_B: &str = "!otherchan@localhost";

/// Verifies that message log data survives a restart even after eviction has occurred.
#[compio::test]
async fn test_c2s_history_survives_restart_after_eviction() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  // --- First boot: create channel, broadcast enough to trigger eviction, shut down ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = FileMessageLogFactory::with_segment_max(data_dir.clone(), 4096, 256, MessageLogMetrics::noop());

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;
    suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
    suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(5), Some(true), Some(0)).await?;

    for i in 1..=20 {
      suite.broadcast(TEST_USER_1, CHANNEL, &format!("evict_{i}")).await?;
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  // --- Second boot: verify eviction state survived the restart ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = FileMessageLogFactory::with_segment_max(data_dir.clone(), 4096, 256, MessageLogMetrics::noop());

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;

    // Check CHAN_SEQ: first_seq should still be > 1 after restart.
    suite
      .write_message(
        TEST_USER_1,
        Message::ChannelSeq(ChannelSeqParameters { id: 60, channel: StringAtom::from(CHANNEL) }),
      )
      .await?;

    let reply = suite.read_message(TEST_USER_1).await?;
    let (first_seq, last_seq) = match reply {
      Message::ChannelSeqAck(params) => {
        assert_eq!(params.id, 60);
        assert!(
          params.first_seq > 1,
          "first_seq should still reflect eviction after restart, got {}",
          params.first_seq
        );
        assert_eq!(params.last_seq, 20);
        (params.first_seq, params.last_seq)
      },
      other => panic!("expected ChannelSeqAck, got: {:?}", other),
    };

    // Request HISTORY for all retained messages.
    suite
      .write_message(
        TEST_USER_1,
        Message::History(HistoryParameters {
          id: 61,
          history_id: StringAtom::from("restart_evict"),
          channel: StringAtom::from(CHANNEL),
          from_seq: first_seq,
          limit: 100,
          direction: None,
        }),
      )
      .await?;

    let mut seqs = Vec::new();
    loop {
      let msg = suite.read_message(TEST_USER_1).await?;
      match msg {
        Message::Message(params) => {
          seqs.push(params.seq);
          let mut buf = vec![0u8; params.length as usize];
          suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
          let mut nl = [0u8; 1];
          suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
        },
        Message::HistoryAck(params) => {
          assert_eq!(params.id, 61);
          assert_eq!(params.count, seqs.len() as u32);
          break;
        },
        other => panic!("expected Message or HistoryAck, got: {:?}", other),
      }
    }

    assert!(!seqs.is_empty(), "should have retained messages after restart");
    assert_eq!(seqs[0], first_seq);
    assert_eq!(*seqs.last().unwrap(), last_seq);

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  Ok(())
}

/// Verifies that HISTORY with from_seq beyond last_seq returns count=0.
#[compio::test]
async fn test_c2s_history_from_seq_beyond_last() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  // Broadcast 5 messages (last_seq = 5).
  for i in 1..=5 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("msg_{i}")).await?;
  }

  // Request history starting from seq 999 — well beyond last_seq.
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 70,
        history_id: StringAtom::from("beyond"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 999,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  let ack = suite.read_message(TEST_USER_1).await?;
  match ack {
    Message::HistoryAck(params) => {
      assert_eq!(params.id, 70);
      assert_eq!(params.count, 0);
    },
    other => panic!("expected HistoryAck with count=0, got: {:?}", other),
  }

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that two channels have independent message logs.
#[compio::test]
async fn test_c2s_history_independent_channels() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;

  // Create and configure two independent channels.
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  suite.join_channel(TEST_USER_1, CHANNEL_B, None).await?;
  suite.configure_channel_full(TEST_USER_1, CHANNEL_B, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  // Broadcast different messages to each channel.
  for i in 1..=3 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("chan_a_{i}")).await?;
  }
  for i in 1..=2 {
    suite.broadcast(TEST_USER_1, CHANNEL_B, &format!("chan_b_{i}")).await?;
  }

  // Verify CHANNEL history: 3 messages.
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 80,
        history_id: StringAtom::from("ch_a"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  let mut payloads_a = Vec::new();
  for _ in 0..3 {
    let msg = suite.read_message(TEST_USER_1).await?;
    match msg {
      Message::Message(params) => {
        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
        payloads_a.push(String::from_utf8(buf)?);
      },
      other => panic!("expected Message, got: {:?}", other),
    }
  }
  let ack_a = suite.read_message(TEST_USER_1).await?;
  match ack_a {
    Message::HistoryAck(params) => assert_eq!(params.count, 3),
    other => panic!("expected HistoryAck, got: {:?}", other),
  }
  assert_eq!(payloads_a, vec!["chan_a_1", "chan_a_2", "chan_a_3"]);

  // Verify CHANNEL_B history: 2 messages (completely independent).
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 81,
        history_id: StringAtom::from("ch_b"),
        channel: StringAtom::from(CHANNEL_B),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  let mut payloads_b = Vec::new();
  for _ in 0..2 {
    let msg = suite.read_message(TEST_USER_1).await?;
    match msg {
      Message::Message(params) => {
        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
        payloads_b.push(String::from_utf8(buf)?);
      },
      other => panic!("expected Message, got: {:?}", other),
    }
  }
  let ack_b = suite.read_message(TEST_USER_1).await?;
  match ack_b {
    Message::HistoryAck(params) => assert_eq!(params.count, 2),
    other => panic!("expected HistoryAck, got: {:?}", other),
  }
  assert_eq!(payloads_b, vec!["chan_b_1", "chan_b_2"]);

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that HISTORY and CHAN_SEQ return PERSISTENCE_NOT_ENABLED on a non-persistent channel.
#[compio::test]
async fn test_c2s_history_and_chan_seq_on_non_persistent_channel() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
  // Channel is NOT persistent (default).

  // HISTORY should fail with PERSISTENCE_NOT_ENABLED.
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 90,
        history_id: StringAtom::from("nope"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 10,
        direction: None,
      }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  assert!(
    matches!(&reply, Message::Error(ErrorParameters { reason, .. }) if *reason == StringAtom::from(ErrorReason::PersistenceNotEnabled)),
    "expected PersistenceNotEnabled error for HISTORY, got: {:?}",
    reply
  );

  // CHAN_SEQ should also fail with PERSISTENCE_NOT_ENABLED.
  suite
    .write_message(
      TEST_USER_1,
      Message::ChannelSeq(ChannelSeqParameters { id: 91, channel: StringAtom::from(CHANNEL) }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  assert!(
    matches!(&reply, Message::Error(ErrorParameters { reason, .. }) if *reason == StringAtom::from(ErrorReason::PersistenceNotEnabled)),
    "expected PersistenceNotEnabled error for CHAN_SEQ, got: {:?}",
    reply
  );

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that toggling persistence off and back on starts a fresh message log.
#[compio::test]
async fn test_c2s_toggle_persistence_off_then_on() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
  let store = FileChannelStore::new(data_dir.clone()).await?;
  let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

  let mut suite = C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
  suite.setup().await?;

  suite.auth(TEST_USER_1, TEST_USER_1).await?;
  suite.join_channel(TEST_USER_1, CHANNEL, None).await?;

  // Enable persistence and broadcast some messages.
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  for i in 1..=5 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("old_{i}")).await?;
  }

  // Disable persistence — should clean up the log.
  suite.configure_channel(TEST_USER_1, CHANNEL, None, None, None, Some(false)).await?;

  // Re-enable persistence and broadcast new messages.
  suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

  for i in 1..=3 {
    suite.broadcast(TEST_USER_1, CHANNEL, &format!("new_{i}")).await?;
  }

  // CHAN_SEQ should reflect only the new messages.
  suite
    .write_message(
      TEST_USER_1,
      Message::ChannelSeq(ChannelSeqParameters { id: 100, channel: StringAtom::from(CHANNEL) }),
    )
    .await?;

  let reply = suite.read_message(TEST_USER_1).await?;
  match reply {
    Message::ChannelSeqAck(params) => {
      assert_eq!(params.id, 100);
      // Seq counter is per-channel lifetime (not per-log), so last_seq = 8 (5 old + 3 new).
      // But old messages were deleted, so first_seq should reflect the fresh log.
      // The in-memory seq counter is NOT reset, so first_seq of the new log starts at 6.
      assert_eq!(params.last_seq, 8);
    },
    other => panic!("expected ChannelSeqAck, got: {:?}", other),
  }

  // HISTORY from seq 1 should only return the new messages (old log was deleted).
  suite
    .write_message(
      TEST_USER_1,
      Message::History(HistoryParameters {
        id: 101,
        history_id: StringAtom::from("toggle"),
        channel: StringAtom::from(CHANNEL),
        from_seq: 1,
        limit: 100,
        direction: None,
      }),
    )
    .await?;

  let mut payloads = Vec::new();
  loop {
    let msg = suite.read_message(TEST_USER_1).await?;
    match msg {
      Message::Message(params) => {
        let mut buf = vec![0u8; params.length as usize];
        suite.read_raw_bytes(TEST_USER_1, &mut buf).await?;
        let mut nl = [0u8; 1];
        suite.read_raw_bytes(TEST_USER_1, &mut nl).await?;
        payloads.push(String::from_utf8(buf)?);
      },
      Message::HistoryAck(params) => {
        assert_eq!(params.id, 101);
        assert_eq!(params.count, payloads.len() as u32);
        break;
      },
      other => panic!("expected Message or HistoryAck, got: {:?}", other),
    }
  }

  assert_eq!(payloads, vec!["new_1", "new_2", "new_3"]);

  suite.teardown().await?;
  s2m_ln.shutdown().await?;
  s2m_dispatcher.shutdown().await?;

  Ok(())
}

/// Verifies that CHAN_SEQ returns the correct range after a restart (without eviction).
#[compio::test]
async fn test_c2s_chan_seq_survives_restart() -> anyhow::Result<()> {
  let tmp = tempfile::tempdir()?;
  let data_dir = tmp.path().to_path_buf();

  // --- First boot: create channel, broadcast messages, shut down ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;
    suite.join_channel(TEST_USER_1, CHANNEL, None).await?;
    suite.configure_channel_full(TEST_USER_1, CHANNEL, None, Some(4096), Some(100), Some(true), Some(0)).await?;

    for i in 1..=7 {
      suite.broadcast(TEST_USER_1, CHANNEL, &format!("seq_{i}")).await?;
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  // --- Second boot: verify CHAN_SEQ ---
  {
    let (s2m_client, mut s2m_ln, mut s2m_dispatcher) = bootstrap_s2m(make_auth_modulator()).await?;
    let store = FileChannelStore::new(data_dir.clone()).await?;
    let mlf = FileMessageLogFactory::new(data_dir.clone(), 4096, MessageLogMetrics::noop());

    let mut suite =
      C2sSuite::with_modulator_and_stores(default_c2s_config(), Some(s2m_client), None, store, mlf).await?;
    suite.setup().await?;

    suite.auth(TEST_USER_1, TEST_USER_1).await?;

    suite
      .write_message(
        TEST_USER_1,
        Message::ChannelSeq(ChannelSeqParameters { id: 110, channel: StringAtom::from(CHANNEL) }),
      )
      .await?;

    let reply = suite.read_message(TEST_USER_1).await?;
    match reply {
      Message::ChannelSeqAck(params) => {
        assert_eq!(params.id, 110);
        assert_eq!(params.first_seq, 1);
        assert_eq!(params.last_seq, 7);
      },
      other => panic!("expected ChannelSeqAck, got: {:?}", other),
    }

    suite.teardown().await?;
    s2m_ln.shutdown().await?;
    s2m_dispatcher.shutdown().await?;
  }

  Ok(())
}
