// SPDX-License-Identifier: BSD-3-Clause

use std::time::Duration;

use futures::FutureExt;
use narwhal_protocol::{ErrorParameters, Message};
use narwhal_test_util::{M2sSuite, assert_message, default_m2s_config};
use narwhal_util::string_atom::StringAtom;

#[compio::test]
async fn test_m2s_connect_timeout() -> anyhow::Result<()> {
  // Set the connection timeout to 50ms.
  let mut config = default_m2s_config();
  config.connect_timeout = Duration::from_millis(50);

  let mut suite = M2sSuite::with_config(config).await;
  suite.setup().await?;

  // Connect to the server but don't send M2sConnect message.
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
async fn test_m2s_ping_timeout() -> anyhow::Result<()> {
  // Configure keep-alive parameters.
  let mut config = default_m2s_config();
  config.keep_alive_interval = Duration::from_millis(100);
  config.min_keep_alive_interval = Duration::from_millis(100);

  let mut suite = M2sSuite::with_config(config).await;
  suite.setup().await?;

  // Connect to the server.
  let mut conn = suite.connect(None).await?;

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
async fn test_m2s_max_connection_limit_reached() -> anyhow::Result<()> {
  // Set the maximum number of connections to 1.
  let mut config = default_m2s_config();
  config.limits.max_connections = 1;

  let mut suite = M2sSuite::with_config(config).await;
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
async fn test_m2s_max_message_size_exceeded() -> anyhow::Result<()> {
  // Set the maximum message size to 1024 bytes.
  let mut config = default_m2s_config();
  config.limits.max_message_size = 1024;

  let mut suite = M2sSuite::with_config(config).await;
  suite.setup().await?;

  // Connect to the server.
  let mut socket = suite.connect(None).await?;

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
async fn test_m2s_mod_direct_message() -> anyhow::Result<()> {
  let mut suite = M2sSuite::with_config(default_m2s_config()).await;
  suite.setup().await?;

  // Connect to the server
  let mut conn = suite.connect(None).await?;

  // Get the payload receiver to verify broadcast
  let mut payload_rx = suite.take_payload_receiver().expect("payload receiver should exist");

  // Prepare test payload
  const PAYLOAD: &[u8] = b"Hello from modulator";

  // Send M2S_MOD_DIRECT message through the connection
  conn
    .write_message(narwhal_protocol::Message::M2sModDirect(narwhal_protocol::M2sModDirectParameters {
      id: 42,
      targets: vec![StringAtom::from("user1@localhost"), StringAtom::from("user2@localhost")],
      length: PAYLOAD.len() as u32,
    }))
    .await?;

  // Write the payload bytes following the message
  conn.write_raw_bytes(PAYLOAD).await?;
  conn.write_raw_bytes(b"\n").await?;

  // Verify the server sends acknowledgment
  assert_message!(
    conn.read_message().await?,
    narwhal_protocol::Message::M2sModDirectAck,
    narwhal_protocol::M2sModDirectAckParameters { id: 42 }
  );

  // Verify the payload was broadcast to the channel
  let mut recv = std::pin::pin!(payload_rx.recv().fuse());
  let mut timeout = std::pin::pin!(narwhal_common::runtime::sleep(Duration::from_millis(100)).fuse());

  futures::select! {
    result = recv => {
      let received = result?;
      assert_eq!(received.targets.len(), 2);
      assert_eq!(&*received.targets[0], "user1@localhost");
      assert_eq!(&*received.targets[1], "user2@localhost");

      // Verify the payload buffer content
      let payload_data = received.payload.as_slice();
      assert_eq!(payload_data, PAYLOAD);
    }
    _ = timeout => {
      panic!("timeout waiting for outbound payload broadcast");
    }
  }

  suite.teardown().await?;

  Ok(())
}
