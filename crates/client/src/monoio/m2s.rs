// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;
use std::time::Duration;

use super::common::{Client, Handshaker, ResponseHandle};
use super::dialer::{Dialer, Stream, TcpDialer};
use crate::config::{Config, M2sConfig, M2sSessionExtraInfo, SessionInfo, TCP_NETWORK, UNIX_NETWORK};

#[cfg(unix)]
use super::dialer::UnixDialer;

use narwhal_common::service::M2sService;
use narwhal_protocol::{DEFAULT_MESSAGE_BUFFER_SIZE, M2sConnectParameters, M2sModDirectParameters, Message};
use narwhal_util::pool::{Pool, PoolBuffer};
use narwhal_util::string_atom::StringAtom;

// ===== Handshaker =====
#[derive(Clone, Debug, Default)]
struct M2sHandshaker {
  shared_secret: Option<StringAtom>,
  heartbeat_interval: Duration,
}

// ===== M2sHandshaker =====

#[async_trait::async_trait(?Send)]
impl Handshaker<Stream> for M2sHandshaker {
  type SessionExtraInfo = M2sSessionExtraInfo;

  async fn handshake(&self, stream: &mut Stream) -> anyhow::Result<(SessionInfo, M2sSessionExtraInfo)> {
    let pool = Pool::new(1, DEFAULT_MESSAGE_BUFFER_SIZE);
    let message_buff = pool.acquire_buffer().await;

    let connect_msg = Message::M2sConnect(M2sConnectParameters {
      protocol_version: 1,
      secret: self.shared_secret.clone(),
      heartbeat_interval: self.heartbeat_interval.as_millis() as u32,
    });

    let reply_msg = super::common::request(connect_msg, stream, message_buff).await?;

    match reply_msg {
      Message::M2sConnectAck(params) => {
        let session_info = SessionInfo {
          heartbeat_interval: params.heartbeat_interval,
          max_inflight_requests: params.max_inflight_requests,
          max_message_size: params.max_message_size,
          max_payload_size: params.max_payload_size,
        };

        Ok((session_info, M2sSessionExtraInfo {}))
      },
      Message::Error(e) => anyhow::bail!("an error occurred during handshake: {}", e.reason),
      _ => anyhow::bail!("unexpected message type during handshake"),
    }
  }
}

/// M2S client for connecting modulators to servers,
/// using monoio-based I/O.
#[derive(Clone, Debug)]
pub struct M2sClient {
  client: Client<Stream, M2sHandshaker, M2sService>,
}

// === impl M2sClient ===

impl M2sClient {
  /// Creates a new M2S client instance.
  ///
  /// This method initializes a client that connects modulators to servers, handling
  /// the handshake process and establishing communication channels.
  ///
  /// # Arguments
  ///
  /// * `config` - The client configuration containing network settings, timeouts,
  ///   heartbeat intervals, connection pool settings, and an optional shared secret.
  ///
  /// # Returns
  ///
  /// Returns `Ok(M2sClient)` if the client is successfully created, or an error if:
  /// - The configuration validation fails
  /// - The underlying client creation fails
  pub fn new(config: M2sConfig) -> crate::Result<Self> {
    config.validate()?;

    let dialer: Arc<dyn Dialer<Stream = Stream>> = match config.network.as_str() {
      TCP_NETWORK => Arc::new(TcpDialer::new(config.address.clone())),
      #[cfg(unix)]
      UNIX_NETWORK => Arc::new(UnixDialer::new(config.socket_path.clone())),
      _ => unreachable!("network type already validated"),
    };

    let shared_secret: Option<StringAtom> =
      if !config.shared_secret.is_empty() { Some(config.shared_secret.as_str().into()) } else { None };

    let handshaker = M2sHandshaker { shared_secret, heartbeat_interval: config.heartbeat_interval };

    let client = Client::new("m2s:client", Config::from(&config), dialer, handshaker)?;

    Ok(Self { client })
  }

  /// Retrieves the current session information from the connected server.
  ///
  /// # Returns
  ///
  /// Returns `Ok(SessionInfo)` containing the session details if successful,
  /// or an error if the client is not connected or the server is unreachable.
  pub async fn session_info(&self) -> crate::Result<SessionInfo> {
    let (session_info, _) = self.client.session_info().await.map_err(crate::Error::from)?;
    Ok(session_info)
  }

  /// Generates the next unique correlation ID for message tracking.
  pub async fn next_id(&self) -> u32 {
    self.client.next_id().await
  }

  /// Sends a message to the server and returns a handle to await the response.
  ///
  /// # Arguments
  ///
  /// * `message` - A `Message` instance that must contain a correlation ID.
  /// * `payload_opt` - An optional `PoolBuffer` containing payload data.
  ///
  /// # Returns
  ///
  /// A [`ResponseHandle`] that resolves to the response `Message` and an optional
  /// payload when `.response().await` is called.
  pub async fn send_message(
    &self,
    message: Message,
    payload_opt: Option<PoolBuffer>,
  ) -> anyhow::Result<ResponseHandle> {
    self.client.send_message(message, payload_opt).await
  }

  /// Routes a private payload directly to specified target clients.
  ///
  /// This method sends a direct message containing the provided payload to one or more
  /// target clients identified by their usernames.
  ///
  /// # Arguments
  ///
  /// * `payload` - The buffer containing the message data to be sent to the targets.
  /// * `targets` - A vector of target client identifiers that should receive the payload.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the payload was successfully routed and acknowledged by the server,
  /// or an error if:
  /// - The client is not connected
  /// - The server fails to acknowledge the message
  /// - An unexpected message type is received
  /// - The network operation fails
  pub async fn route_private_payload(&self, payload: PoolBuffer, targets: Vec<StringAtom>) -> crate::Result<()> {
    let id = self.client.next_id().await;

    let params = M2sModDirectParameters { id, targets, length: payload.len() as u32 };

    let handle = self.client.send_message(Message::M2sModDirect(params), Some(payload)).await?;
    let (response, _) = handle.response().await?;

    match response {
      Message::M2sModDirectAck(_) => Ok(()),
      Message::Error(err) => Err(err.into()),
      _ => Err(anyhow::anyhow!("unexpected message type during private payload routing").into()),
    }
  }

  /// Gracefully shuts down the client connection.
  ///
  /// This method cleanly terminates the connection to the server, ensuring
  /// all pending operations are completed and resources are properly released.
  ///
  /// After calling this method, the client instance should not be used for
  /// further operations.
  pub async fn shutdown(&self) -> crate::Result<()> {
    self.client.shutdown().await.map_err(Into::into)
  }
}
