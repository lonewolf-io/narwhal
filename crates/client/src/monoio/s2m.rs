// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;
use std::time::Duration;

use super::common::{Client, Handshaker, ResponseHandle};
use super::dialer::{Dialer, Stream, TcpDialer};
use crate::config::{Config, S2mConfig, S2mSessionExtraInfo, SessionInfo, TCP_NETWORK, UNIX_NETWORK};

#[cfg(unix)]
use super::dialer::UnixDialer;

use narwhal_common::service::S2mService;
use narwhal_protocol::{DEFAULT_MESSAGE_BUFFER_SIZE, Message, S2mConnectParameters};
use narwhal_util::pool::{Pool, PoolBuffer};
use narwhal_util::string_atom::StringAtom;

#[derive(Clone, Debug, Default)]
struct S2mHandshaker {
  shared_secret: Option<StringAtom>,
  heartbeat_interval: Duration,
}

#[async_trait::async_trait(?Send)]
impl Handshaker<Stream> for S2mHandshaker {
  type SessionExtraInfo = S2mSessionExtraInfo;

  async fn handshake(&self, stream: &mut Stream) -> anyhow::Result<(SessionInfo, S2mSessionExtraInfo)> {
    let pool = Pool::new(1, DEFAULT_MESSAGE_BUFFER_SIZE);
    let message_buff = pool.acquire_buffer().await;

    let connect_msg = Message::S2mConnect(S2mConnectParameters {
      protocol_version: 1,
      secret: self.shared_secret.clone(),
      heartbeat_interval: self.heartbeat_interval.as_millis() as u32,
    });

    let reply_msg = super::common::request(connect_msg, stream, message_buff).await?;

    match reply_msg {
      Message::S2mConnectAck(params) => {
        let session_info = SessionInfo {
          heartbeat_interval: params.heartbeat_interval,
          max_inflight_requests: params.max_inflight_requests,
          max_message_size: params.max_message_size,
          max_payload_size: params.max_payload_size,
        };

        let session_extra_info =
          S2mSessionExtraInfo { protocol_name: params.application_protocol, operations: params.operations };

        Ok((session_info, session_extra_info))
      },
      Message::Error(e) => anyhow::bail!("an error occurred during handshake: {}", e.reason),
      _ => anyhow::bail!("unexpected message type during handshake"),
    }
  }
}

/// S2M client for connecting to a modulator server,
/// using monoio-based I/O.
#[derive(Clone, Debug)]
pub struct S2mClient {
  client: Client<Stream, S2mHandshaker, S2mService>,
}

// === impl S2mClient ===

impl S2mClient {
  /// Creates a new S2M client with the provided configuration.
  ///
  /// This method initializes a client that enables servers to connect to modulators,
  /// facilitating bidirectional communication through the S2M protocol. The client
  /// handles authentication, payload forwarding, and event management.
  ///
  /// # Arguments
  ///
  /// * `config` - The client configuration containing network settings, timeouts,
  ///   heartbeat intervals, connection pool settings, and an optional shared secret.
  ///
  /// # Returns
  ///
  /// Returns `Ok(S2mClient)` if the client is successfully created, or an error if:
  /// - The configuration validation fails
  /// - The underlying client creation fails
  pub fn new(config: S2mConfig) -> crate::Result<Self> {
    config.validate()?;

    let dialer: Arc<dyn Dialer<Stream = Stream>> = match config.network.as_str() {
      TCP_NETWORK => Arc::new(TcpDialer::new(config.address.clone())),
      #[cfg(unix)]
      UNIX_NETWORK => Arc::new(UnixDialer::new(config.socket_path.clone())),
      _ => unreachable!("network type already validated"),
    };

    let shared_secret: Option<StringAtom> =
      if !config.shared_secret.is_empty() { Some(config.shared_secret.as_str().into()) } else { None };

    let handshaker = S2mHandshaker { shared_secret, heartbeat_interval: config.heartbeat_interval };

    let client = Client::new("s2m:client", Config::from(&config), dialer, handshaker)?;

    Ok(Self { client })
  }

  /// Retrieves the current session information negotiated with the modulator server.
  ///
  /// Returns both the core session info and the S2M-specific extra info (protocol name
  /// and supported operations).
  ///
  /// # Returns
  ///
  /// Returns `Ok((SessionInfo, S2mSessionExtraInfo))` if successful, or an error if:
  /// - The client is not connected
  /// - The session has been terminated
  /// - Network communication fails
  pub async fn session_info(&self) -> crate::Result<(SessionInfo, S2mSessionExtraInfo)> {
    self.client.session_info().await.map_err(Into::into)
  }

  /// Generates the next unique correlation ID for message tracking.
  pub async fn next_id(&self) -> u32 {
    self.client.next_id().await
  }

  /// Sends a message to the modulator server and returns a handle to await the response.
  ///
  /// # Arguments
  ///
  /// * `message` - A `Message` instance that must contain a correlation ID.
  /// * `payload_opt` - An optional `PoolBuffer` containing payload data.
  ///
  /// # Returns
  ///
  /// A [`ResponseHandle`] that resolves to the response `Message` and an optional payload
  /// when `.response().await` is called.
  pub async fn send_message(
    &self,
    message: Message,
    payload_opt: Option<PoolBuffer>,
  ) -> anyhow::Result<ResponseHandle> {
    self.client.send_message(message, payload_opt).await
  }

  /// Gracefully shuts down the S2M client and releases all resources.
  ///
  /// This method performs a clean shutdown by signalling cancellation to all
  /// active connections and background I/O tasks. After calling this method,
  /// the client instance should not be used for further operations.
  pub async fn shutdown(&self) -> crate::Result<()> {
    self.client.shutdown().await.map_err(Into::into)
  }
}
