// SPDX-License-Identifier: BSD-3-Clause

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;

use async_channel::Receiver;
use narwhal_common::service::C2sService;
use narwhal_protocol::{
  AclAction, AclType, AuthParameters, ConnectParameters, DEFAULT_MESSAGE_BUFFER_SIZE, IdentifyParameters, Message, Nid,
  QoS,
};
use narwhal_util::pool::{Pool, PoolBuffer};
use narwhal_util::string_atom::StringAtom;

use super::common::{self, Handshaker};
use super::dialer::{TlsDialer, TlsStream};
use crate::auth::{AuthMethod, AuthenticatorFactory};
use crate::config::{C2sConfig, C2sSessionExtraInfo, SessionInfo};

/// Handshaker implementation for C2S client.
#[derive(Clone)]
struct C2sHandshaker {
  /// The requested heartbeat interval.
  heartbeat_interval: Duration,

  /// The username to use for identification.
  username: Option<String>,

  /// The authenticator factory to use for authentication (when required).
  authenticator_factory: Option<Arc<dyn AuthenticatorFactory>>,
}

impl std::fmt::Debug for C2sHandshaker {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("C2sHandshaker")
      .field("heartbeat_interval", &self.heartbeat_interval)
      .field("username", &self.username.is_some())
      .field("authenticator_factory", &self.authenticator_factory.is_some())
      .finish()
  }
}

#[async_trait::async_trait(?Send)]
impl Handshaker<TlsStream> for C2sHandshaker {
  type SessionExtraInfo = C2sSessionExtraInfo;

  async fn handshake(&self, stream: &mut TlsStream) -> anyhow::Result<(SessionInfo, C2sSessionExtraInfo)> {
    let mut pool = Pool::new(1, DEFAULT_MESSAGE_BUFFER_SIZE);
    let mut message_buff = pool.acquire_buffer().await;

    let connect_msg = Message::Connect(ConnectParameters {
      protocol_version: 1,
      heartbeat_interval: self.heartbeat_interval.as_millis() as u32,
    });

    let connect_ack_msg = common::request(connect_msg, stream, message_buff).await?;

    let (auth_required, max_persist_messages, session_info) = match connect_ack_msg {
      Message::ConnectAck(params) => {
        let session_info = SessionInfo {
          heartbeat_interval: params.heartbeat_interval,
          max_inflight_requests: params.max_inflight_requests,
          max_message_size: params.max_message_size,
          max_payload_size: params.max_payload_size,
        };

        (params.auth_required, params.max_persist_messages, session_info)
      },
      Message::Error(err) => {
        return Err(anyhow!("connection rejected: {:?}", err.reason));
      },
      _ => {
        return Err(anyhow!("unexpected message during handshake: expected ConnectAck"));
      },
    };

    pool = Pool::new(1, session_info.max_message_size as usize);
    message_buff = pool.acquire_buffer().await;

    let nid: Nid;

    if !auth_required && let Some(username) = self.username.as_ref() {
      let identify_msg = Message::Identify(IdentifyParameters { username: username.as_str().into() });

      match common::request(identify_msg, stream, message_buff).await? {
        Message::IdentifyAck(params) => nid = Nid::try_from(params.nid)?,
        Message::Error(err) => return Err(anyhow!("error during handshake: {:?}", err.reason)),
        _ => return Err(anyhow!("unexpected message during handshake: expected IdentifyAck")),
      }
    } else if let Some(auth_factory) = self.authenticator_factory.as_ref() {
      let mut authenticator = auth_factory.create();

      // Start authentication flow
      let mut token = authenticator.start().await?;

      loop {
        let auth_msg = Message::Auth(AuthParameters { token: token.as_str().into() });

        match common::request(auth_msg, stream, message_buff).await? {
          Message::AuthAck(params) => {
            if let Some(succeded) = params.succeeded
              && let Some(nid_str) = params.nid
              && succeded
            {
              // Authentication succeeded
              nid = Nid::try_from(nid_str)?;
              break;
            } else if let Some(challenge) = params.challenge {
              // Continue with challenge-response
              token = authenticator.next(challenge.to_string()).await?;
              message_buff = pool.acquire_buffer().await;
            } else {
              // Authentication failed
              return Err(anyhow!("authentication failed"));
            }
          },
          Message::Error(err) => {
            return Err(anyhow!("error during authentication: {:?}", err.reason));
          },
          _ => {
            return Err(anyhow!("unexpected message during authentication: expected AuthAck"));
          },
        }
      }
    } else {
      return Err(anyhow!("no proper authentication method provided"));
    }

    Ok((session_info, C2sSessionExtraInfo { nid, max_persist_messages: max_persist_messages.unwrap_or(0) }))
  }
}

impl C2sHandshaker {
  /// Creates a new handshaker with the specified heartbeat interval and authentication method.
  ///
  /// # Arguments
  ///
  /// * `heartbeat_interval` - The heartbeat interval to negotiate with the server
  /// * `auth_method` - The authentication method to use during handshake
  ///
  /// # Returns
  ///
  /// Returns a new `C2sHandshaker` instance configured with the provided parameters.
  fn new(heartbeat_interval: Duration, auth_method: AuthMethod) -> Self {
    match auth_method {
      AuthMethod::Identify { username } => {
        Self { heartbeat_interval, username: Some(username), authenticator_factory: None }
      },
      AuthMethod::Auth { authenticator_factory } => {
        Self { heartbeat_interval, username: None, authenticator_factory: Some(authenticator_factory) }
      },
    }
  }
}

/// C2S client for connecting to the Narwhal server.
#[derive(Clone)]
pub struct C2sClient {
  client: Arc<common::Client<TlsStream, C2sHandshaker, C2sService>>,
}

// === impl C2sClient ===

impl C2sClient {
  /// Creates a new C2S client instance.
  ///
  /// This method initializes a client that connects to the Narwhal server.
  ///
  /// By default, this method enables TLS certificate verification for secure connections.
  /// For testing with self-signed certificates, use `new_with_insecure_tls()` instead.
  ///
  /// # Arguments
  ///
  /// * `config` - The client configuration containing the server address, network settings,
  ///   timeouts, heartbeat intervals, and other connection parameters.
  /// * `auth_method` - The authentication method to use when connecting to the server.
  ///   Can be either simple username-based identification or multi-step authentication.
  ///
  /// # Returns
  ///
  /// Returns a `C2sClient` instance that can be used to communicate with the Narwhal server.
  pub fn new(config: C2sConfig, auth_method: AuthMethod) -> crate::Result<Self> {
    let dialer = Arc::new(TlsDialer::new(config.address.as_str().into())?);

    let handshaker = C2sHandshaker::new(config.heartbeat_interval, auth_method);

    let client = Arc::new(common::Client::new("c2s-client", config.into(), dialer, handshaker)?);

    Ok(Self { client })
  }

  /// Creates a new C2S client instance with insecure TLS.
  ///
  /// # Security Warning
  ///
  /// **DANGER**: This method disables TLS certificate verification, making the connection
  /// vulnerable to man-in-the-middle attacks. This should **ONLY** be used in
  /// development/testing environments with self-signed certificates. **NEVER** use this
  /// in production environments.
  ///
  /// # Arguments
  ///
  /// * `config` - The client configuration containing the server address, network settings,
  ///   timeouts, heartbeat intervals, and other connection parameters.
  /// * `auth_method` - The authentication method to use when connecting to the server.
  ///   Can be either simple username-based identification or multi-step authentication.
  ///
  /// # Returns
  ///
  /// Returns a `C2sClient` instance that can be used to communicate with the Narwhal server.
  pub fn new_with_insecure_tls(config: C2sConfig, auth_method: AuthMethod) -> crate::Result<Self> {
    let dialer = Arc::new(TlsDialer::with_certificate_verification(config.address.as_str().into(), false)?);

    let handshaker = C2sHandshaker::new(config.heartbeat_interval, auth_method);

    let client = Arc::new(common::Client::new("c2s-client", config.into(), dialer, handshaker)?);

    Ok(Self { client })
  }

  /// Retrieves the session information established during the handshake.
  ///
  /// This method returns the session details negotiated with the server during
  /// the connection handshake. The information includes configuration limits and
  /// parameters that govern the client-server communication.
  ///
  /// # Returns
  ///
  /// Returns session information including:
  /// * `heartbeat_interval` - The negotiated heartbeat interval in milliseconds
  /// * `max_inflight_requests` - Maximum number of concurrent in-flight requests
  /// * `max_message_size` - Maximum size of a message in bytes
  /// * `max_payload_size` - Maximum size of a payload in bytes
  /// * `extra` - Additional C2S-specific session info
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// * The client is not connected to the server
  /// * The session information is not available
  pub async fn session_info(&self) -> crate::Result<(SessionInfo, C2sSessionExtraInfo)> {
    self.client.session_info().await.map_err(Into::into)
  }

  /// Joins a channel on the server.
  ///
  /// # Arguments
  ///
  /// * `channel` - The name of the channel to join.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the channel was successfully joined.
  ///
  /// # Errors
  ///
  /// Returns an error if the join operation fails.
  pub async fn join_channel(&self, channel: StringAtom) -> crate::Result<()> {
    use narwhal_protocol::JoinChannelParameters;

    let id = self.client.next_id().await;
    let message = Message::JoinChannel(JoinChannelParameters { id, channel, on_behalf: None });

    let handle = self.client.send_message(message, None).await?;
    let (response, _) = handle.response().await?;

    match response {
      Message::JoinChannelAck(_) => Ok(()),
      Message::Error(err) => Err(err.into()),
      _ => Err(anyhow!("unexpected response to join channel request").into()),
    }
  }

  /// Leaves a channel on the server.
  ///
  /// # Arguments
  ///
  /// * `channel` - The name of the channel to leave.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the channel was successfully left.
  ///
  /// # Errors
  ///
  /// Returns an error if the leave operation fails.
  pub async fn leave_channel(&self, channel: StringAtom) -> crate::Result<()> {
    use narwhal_protocol::LeaveChannelParameters;

    let id = self.client.next_id().await;
    let message = Message::LeaveChannel(LeaveChannelParameters { id, channel, on_behalf: None });

    let handle = self.client.send_message(message, None).await?;
    let (response, _) = handle.response().await?;

    match response {
      Message::LeaveChannelAck(_) => Ok(()),
      Message::Error(err) => Err(err.into()),
      _ => Err(anyhow!("unexpected response to leave channel request").into()),
    }
  }

  /// Configures channel settings such as maximum clients and payload size.
  ///
  /// # Arguments
  ///
  /// * `channel` - The name of the channel to configure.
  /// * `max_clients` - The maximum number of clients allowed in the channel.
  /// * `max_payload_size` - The maximum payload size (in bytes) for messages in the channel.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the channel was successfully configured.
  ///
  /// # Errors
  ///
  /// Returns an error if the configuration operation fails or the user is not
  /// authorized to configure the channel.
  pub async fn configure_channel(
    &self,
    channel: StringAtom,
    max_clients: Option<u32>,
    max_payload_size: Option<u32>,
    max_persist_messages: Option<u32>,
    persist: Option<bool>,
  ) -> crate::Result<()> {
    use narwhal_protocol::SetChannelConfigurationParameters;

    let id = self.client.next_id().await;
    let message = Message::SetChannelConfiguration(SetChannelConfigurationParameters {
      id,
      channel,
      max_clients,
      max_payload_size,
      max_persist_messages,
      persist,
    });

    let handle = self.client.send_message(message, None).await?;
    let (response, _) = handle.response().await?;

    match response {
      Message::SetChannelConfigurationAck(_) => Ok(()),
      Message::Error(err) => Err(err.into()),
      _ => Err(anyhow!("unexpected response to configure channel request").into()),
    }
  }

  /// Sets the access control list (ACL) for a channel.
  ///
  /// This method configures who can join, publish to, and read from a channel.
  /// Only the channel owner or authorized users can set channel ACLs.
  ///
  /// # Arguments
  ///
  /// * `channel` - The name of the channel to configure.
  /// * `acl_type` - The type of ACL to set.
  /// * `acl_action` - The action for this ACL entry.
  /// * `nids` - The list of NIDs this ACL entry applies to.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the ACL was successfully set.
  ///
  /// # Errors
  ///
  /// Returns an error if the ACL operation fails or the user is not
  /// authorized to modify the channel's ACL.
  pub async fn set_channel_acl(
    &self,
    channel: StringAtom,
    acl_type: AclType,
    acl_action: AclAction,
    nids: Vec<Nid>,
  ) -> crate::Result<()> {
    use narwhal_protocol::SetChannelAclParameters;

    let id = self.client.next_id().await;
    let message = Message::SetChannelAcl(SetChannelAclParameters {
      id,
      channel,
      nids: nids.iter().map(|nid| nid.into()).collect(),
      r#type: acl_type.as_str().into(),
      action: acl_action.as_str().into(),
    });

    let handle = self.client.send_message(message, None).await?;
    let (response, _) = handle.response().await?;

    match response {
      Message::SetChannelAclAck(_) => Ok(()),
      Message::Error(err) => Err(err.into()),
      _ => Err(anyhow!("unexpected response to set channel ACL request").into()),
    }
  }

  /// Broadcasts a message to a channel.
  ///
  /// # Arguments
  ///
  /// * `channel` - The name of the channel to broadcast to.
  /// * `qos` - The Quality of Service level for the broadcast.
  /// * `payload` - The payload data to broadcast.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the broadcast was successful.
  ///
  /// # Errors
  ///
  /// Returns an error if the broadcast operation fails.
  pub async fn broadcast(&self, channel: StringAtom, qos: Option<QoS>, payload: PoolBuffer) -> crate::Result<()> {
    use narwhal_protocol::BroadcastParameters;

    let protocol_qos = qos.map(|q| q.as_u8());

    let id = self.client.next_id().await;
    let length = payload.len() as u32;
    let message = Message::Broadcast(BroadcastParameters { id, channel, qos: protocol_qos, length });

    let handle = self.client.send_message(message, Some(payload)).await?;
    let (response, _) = handle.response().await?;

    match response {
      Message::BroadcastAck(_) => Ok(()),
      Message::Error(err) => Err(err.into()),
      _ => Err(anyhow!("unexpected response to broadcast request").into()),
    }
  }

  /// Shuts down the client and closes all connections.
  ///
  /// This method gracefully shuts down the client, signalling cancellation to
  /// all active I/O tasks and cleaning up resources.
  ///
  /// # Errors
  ///
  /// Returns an error if the shutdown process fails.
  pub async fn shutdown(&self) -> crate::Result<()> {
    self.client.shutdown().await.map_err(Into::into)
  }

  /// Returns a stream of inbound messages from the server.
  ///
  /// This method provides access to unsolicited messages sent by the server that are not
  /// responses to client requests. These are typically broadcast messages from channels
  /// the client has joined.
  ///
  /// # Important
  ///
  /// This method can only be called **once** per `Client` instance. Subsequent calls will panic.
  /// This is because the method takes ownership of the internal receiver, ensuring there is only
  /// one consumer of inbound messages.
  ///
  /// # Panics
  ///
  /// Panics if called more than once on the same `Client` instance.
  pub async fn inbound_stream(&self) -> Receiver<(Message, Option<PoolBuffer>)> {
    self.client.inbound_stream().await
  }
}
