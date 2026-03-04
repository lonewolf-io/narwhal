// SPDX-License-Identifier: BSD-3-Clause

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// TCP network type.
pub const TCP_NETWORK: &str = "tcp";

/// Unix domain socket network type.
pub const UNIX_NETWORK: &str = "unix";

// Modulator client configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientConfig {
  /// The network type. Either "tcp" or "unix".
  /// Default is "tcp".
  #[serde(default = "default_network")]
  pub network: String,

  /// The address of the modulator server.
  /// This should be in the format "host:port".
  #[serde(default)]
  pub address: String,

  /// The unix domain socket path.
  /// This is used when the network type is "unix".
  #[serde(default)]
  pub socket_path: String,

  /// The shared secret for authentication.
  /// This is used when the network type is "tcp".
  #[serde(default)]
  pub shared_secret: String,

  /// The maximum number of idle connections to keep in the pool.
  #[serde(default = "default_max_idle_connections")]
  pub max_idle_connections: usize,

  /// The heartbeat interval for the client that should be negotiated
  /// with the server.
  #[serde(default = "default_heartbeat_interval", with = "humantime_serde")]
  pub heartbeat_interval: Duration,

  /// The client connection timeout.
  /// This is the timeout for establishing a connection to the server.
  #[serde(default = "default_connect_timeout", with = "humantime_serde")]
  pub connect_timeout: Duration,

  /// The client read/write timeout.
  #[serde(default = "default_timeout", with = "humantime_serde")]
  pub timeout: Duration,

  /// The client read payload timeout.
  #[serde(default = "default_payload_read_timeout", with = "humantime_serde")]
  pub payload_read_timeout: Duration,

  /// The initial delay for the backoff strategy.
  #[serde(default = "default_backoff_initial_delay", with = "humantime_serde")]
  pub backoff_initial_delay: Duration,

  /// The maximum delay for the backoff strategy.
  #[serde(default = "default_backoff_max_delay", with = "humantime_serde")]
  pub backoff_max_delay: Duration,

  /// The maximum number of retries for the backoff strategy.
  #[serde(default = "default_backoff_max_retries")]
  pub backoff_max_retries: usize,
}

// === impl ClientConfig ===

impl ClientConfig {
  /// Validates the configuration.
  pub fn validate(&self) -> anyhow::Result<()> {
    if self.network != TCP_NETWORK && self.network != UNIX_NETWORK {
      return Err(anyhow::anyhow!("invalid network type: {}", self.network));
    }

    if self.network == UNIX_NETWORK && self.socket_path.is_empty() {
      return Err(anyhow::anyhow!("socket path must be specified for unix network type"));
    }

    if self.address.is_empty() && self.network == TCP_NETWORK {
      return Err(anyhow::anyhow!("address must be specified for tcp network type"));
    }

    if self.network == TCP_NETWORK && !self.shared_secret.is_empty() && self.shared_secret.trim().is_empty() {
      return Err(anyhow::anyhow!("shared_secret cannot be only whitespace"));
    }

    if self.max_idle_connections == 0 {
      return Err(anyhow::anyhow!("max idle connections must be greater than 0"));
    }

    if self.heartbeat_interval.is_zero() {
      return Err(anyhow::anyhow!("heartbeat interval must be greater than 0"));
    }

    if self.connect_timeout.is_zero() {
      return Err(anyhow::anyhow!("connect timeout must be greater than 0"));
    }

    if self.timeout.is_zero() {
      return Err(anyhow::anyhow!("timeout must be greater than 0"));
    }

    Ok(())
  }

  /// Checks if the configuration has been properly set (not just default values).
  pub fn is_configured(&self) -> bool {
    match self.network.as_str() {
      TCP_NETWORK => !self.address.trim().is_empty(),
      UNIX_NETWORK => !self.socket_path.trim().is_empty(),
      _ => false,
    }
  }
}

impl Default for ClientConfig {
  fn default() -> Self {
    Self {
      network: default_network(),
      address: String::default(),
      socket_path: String::default(),
      shared_secret: String::default(),
      max_idle_connections: default_max_idle_connections(),
      heartbeat_interval: default_heartbeat_interval(),
      connect_timeout: default_connect_timeout(),
      timeout: default_timeout(),
      payload_read_timeout: default_payload_read_timeout(),
      backoff_initial_delay: default_backoff_initial_delay(),
      backoff_max_delay: default_backoff_max_delay(),
      backoff_max_retries: default_backoff_max_retries(),
    }
  }
}

/// Listener configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListenerConfig {
  /// The network type. Either "tcp" or "unix".
  /// Default is "tcp".
  #[serde(default = "default_network")]
  pub network: String,

  /// The address to listen on.
  /// This should look like "host:port".
  /// This is used when the network type is "tcp".
  #[serde(default)]
  pub bind_address: String,

  /// The unix domain socket path.
  /// This is used when the network type is "unix".
  #[serde(default)]
  pub socket_path: String,
}

// === impl ListenerConfig ===

impl Default for ListenerConfig {
  fn default() -> Self {
    Self { network: default_network(), bind_address: String::new(), socket_path: String::new() }
  }
}

impl ListenerConfig {
  /// Validates the configuration.
  pub fn validate(&self) -> anyhow::Result<()> {
    match self.network.as_str() {
      TCP_NETWORK => {
        if self.bind_address.trim().is_empty() {
          return Err(anyhow::anyhow!("bind_address must be specified for TCP network"));
        }
        // Basic validation for host:port format
        if !self.bind_address.contains(':') {
          return Err(anyhow::anyhow!("bind_address must be in host:port format"));
        }
      },
      UNIX_NETWORK => {
        if self.socket_path.trim().is_empty() {
          return Err(anyhow::anyhow!("socket_path must be specified for Unix domain socket network"));
        }
      },
      _ => {
        return Err(anyhow::anyhow!(
          "invalid network type '{}' (expected '{}', or '{}')",
          self.network,
          TCP_NETWORK,
          UNIX_NETWORK
        ));
      },
    }

    Ok(())
  }

  /// Checks if the configuration has been properly set (not just default values).
  pub fn is_configured(&self) -> bool {
    match self.network.as_str() {
      TCP_NETWORK => !self.bind_address.trim().is_empty(),
      UNIX_NETWORK => !self.socket_path.trim().is_empty(),
      _ => false,
    }
  }
}

/// Modulator server configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
  /// The listener configuration.
  #[serde(default)]
  pub listener: ListenerConfig,

  /// The shared secret for authentication.
  /// This is used when the network type is "tcp".
  #[serde(default)]
  pub shared_secret: String,

  /// The timeout for the connection.
  #[serde(default = "default_connect_timeout", with = "humantime_serde")]
  pub connect_timeout: Duration,

  /// The interval for keep alive messages.
  #[serde(default = "default_keep_alive_interval", with = "humantime_serde")]
  pub keep_alive_interval: Duration,

  /// The minimum interval for keep alive messages.
  #[serde(default = "default_min_keep_alive_interval", with = "humantime_serde")]
  pub min_keep_alive_interval: Duration,

  /// The timeout for the request.
  #[serde(default = "default_request_timeout", with = "humantime_serde")]
  pub request_timeout: Duration,

  /// The timeout for reading a broadcast payload.
  #[serde(default = "default_payload_read_timeout", with = "humantime_serde")]
  pub payload_read_timeout: Duration,

  /// The limits.
  #[serde(default)]
  pub limits: Limits,
}

// ===== impl ServerConfig =====

impl Default for ServerConfig {
  fn default() -> Self {
    Self {
      listener: ListenerConfig::default(),
      shared_secret: String::default(),
      connect_timeout: default_connect_timeout(),
      min_keep_alive_interval: default_min_keep_alive_interval(),
      keep_alive_interval: default_keep_alive_interval(),
      request_timeout: default_request_timeout(),
      payload_read_timeout: default_payload_read_timeout(),
      limits: Limits::default(),
    }
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
  /// The maximum number of connections to accept.
  pub max_connections: u32,

  /// The maximum message size allowed.
  pub max_message_size: u32,

  /// The maximum payload size allowed.
  pub max_payload_size: u32,

  /// The maximum payload pool memory budget allowed.
  pub payload_pool_memory_budget: u64,

  /// The maximum number of inflight requests per client.
  pub max_inflight_requests: u32,

  /// The maximum number of outbound messages that can be enqueued
  /// before disconnecting the client.
  pub outgoing_message_queue_size: u32,

  /// The maximum number of bytes that can be sent per second.
  pub rate_limit: u32,
}

// ===== impl Limits =====

impl Default for Limits {
  fn default() -> Self {
    Self {
      max_connections: 250,
      max_message_size: 8 * 1024,                    // 8KB
      max_payload_size: 256 * 1024,                  // 256KB
      payload_pool_memory_budget: 256 * 1024 * 1024, // 256MB
      max_inflight_requests: 100,
      outgoing_message_queue_size: 2048,
      rate_limit: 512 * 1024, // 512KB
    }
  }
}

impl From<&ServerConfig> for narwhal_common::conn::Config {
  fn from(config: &ServerConfig) -> Self {
    narwhal_common::conn::Config {
      max_connections: config.limits.max_connections,
      max_message_size: config.limits.max_message_size,
      max_payload_size: config.limits.max_payload_size,
      payload_pool_memory_budget: config.limits.payload_pool_memory_budget, // 256MB default
      connect_timeout: config.connect_timeout,
      authenticate_timeout: Default::default(),
      payload_read_timeout: config.payload_read_timeout,
      outbound_message_queue_size: config.limits.outgoing_message_queue_size,
      request_timeout: config.request_timeout,
      max_inflight_requests: config.limits.max_inflight_requests,
      rate_limit: config.limits.rate_limit,
    }
  }
}

impl ServerConfig {
  /// Validates the configuration.
  pub fn validate(&self) -> anyhow::Result<()> {
    self.listener.validate()?;

    // Validate shared_secret for TCP
    if self.listener.network == TCP_NETWORK && self.shared_secret.trim().is_empty() {
      return Err(anyhow::anyhow!("shared_secret must be specified for TCP network"));
    }

    // Validate timeouts are positive
    if self.connect_timeout.is_zero() {
      return Err(anyhow::anyhow!("connect_timeout must be greater than zero"));
    }
    if self.request_timeout.is_zero() {
      return Err(anyhow::anyhow!("request_timeout must be greater than zero"));
    }
    if self.keep_alive_interval.is_zero() {
      return Err(anyhow::anyhow!("keep_alive_interval must be greater than zero"));
    }
    if self.min_keep_alive_interval.is_zero() {
      return Err(anyhow::anyhow!("min_keep_alive_interval must be greater than zero"));
    }

    // Validate keep alive interval relationship
    if self.min_keep_alive_interval > self.keep_alive_interval {
      return Err(anyhow::anyhow!(
        "min_keep_alive_interval ({:?}) cannot be greater than keep_alive_interval ({:?})",
        self.min_keep_alive_interval,
        self.keep_alive_interval
      ));
    }

    Ok(())
  }

  /// Checks if the configuration has been properly set (not just default values).
  pub fn is_configured(&self) -> bool {
    self.listener.is_configured()
  }
}

fn default_network() -> String {
  TCP_NETWORK.to_string()
}

fn default_max_idle_connections() -> usize {
  16
}

fn default_heartbeat_interval() -> Duration {
  Duration::from_secs(60)
}

fn default_connect_timeout() -> Duration {
  Duration::from_secs(5)
}

fn default_timeout() -> Duration {
  Duration::from_secs(5)
}

fn default_payload_read_timeout() -> Duration {
  Duration::from_secs(5)
}

fn default_backoff_initial_delay() -> Duration {
  Duration::from_millis(100)
}

fn default_backoff_max_delay() -> Duration {
  Duration::from_secs(30)
}

fn default_backoff_max_retries() -> usize {
  5
}

fn default_keep_alive_interval() -> Duration {
  Duration::from_secs(30)
}

fn default_min_keep_alive_interval() -> Duration {
  Duration::from_secs(10)
}

fn default_request_timeout() -> Duration {
  Duration::from_secs(10)
}

/// S2M server configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct S2mServerConfig {
  /// The S2M server configuration.
  #[serde(default)]
  pub server: ServerConfig,

  /// M2S client configuration.
  /// This configuration is used to connect to the M2S server only when
  /// modulator implements ReceivePrivatePayload operation.
  #[serde(rename = "m2s-client", default)]
  pub m2s_client: M2sClientConfig,
}

// === impl S2mServerConfig ===

impl S2mServerConfig {
  /// Validates the configuration.
  pub fn validate(&self) -> anyhow::Result<()> {
    self.server.validate()?;

    if self.m2s_client.is_configured() {
      self.m2s_client.validate()?;
    }
    Ok(())
  }

  /// Checks if the configuration is configured.
  pub fn is_configured(&self) -> bool {
    self.server.is_configured()
  }
}

/// S2M client configuration.
pub type S2mClientConfig = ClientConfig;

/// M2s server configuration.
pub type M2sServerConfig = ServerConfig;

/// M2s client configuration.
pub type M2sClientConfig = ClientConfig;

impl From<ClientConfig> for narwhal_client::S2mConfig {
  fn from(config: ClientConfig) -> Self {
    Self {
      network: config.network,
      address: config.address,
      socket_path: config.socket_path,
      shared_secret: config.shared_secret,
      max_idle_connections: config.max_idle_connections,
      heartbeat_interval: config.heartbeat_interval,
      connect_timeout: config.connect_timeout,
      timeout: config.timeout,
      payload_read_timeout: config.payload_read_timeout,
      backoff_initial_delay: config.backoff_initial_delay,
      backoff_max_delay: config.backoff_max_delay,
      backoff_max_retries: config.backoff_max_retries,
    }
  }
}

impl From<ClientConfig> for narwhal_client::M2sConfig {
  fn from(config: ClientConfig) -> Self {
    Self {
      network: config.network,
      address: config.address,
      socket_path: config.socket_path,
      shared_secret: config.shared_secret,
      max_idle_connections: config.max_idle_connections,
      heartbeat_interval: config.heartbeat_interval,
      connect_timeout: config.connect_timeout,
      timeout: config.timeout,
      payload_read_timeout: config.payload_read_timeout,
      backoff_initial_delay: config.backoff_initial_delay,
      backoff_max_delay: config.backoff_max_delay,
      backoff_max_retries: config.backoff_max_retries,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_is_configured() {
    // Test default config is not configured
    let config = ServerConfig::default();
    assert!(!config.is_configured());

    // Test TCP config
    let config = ServerConfig {
      listener: ListenerConfig { network: TCP_NETWORK.to_string(), ..Default::default() },
      ..Default::default()
    };
    assert!(!config.is_configured()); // No bind_address yet

    let config = ServerConfig {
      listener: ListenerConfig {
        network: TCP_NETWORK.to_string(),
        bind_address: "127.0.0.1:8080".to_string(),
        ..Default::default()
      },
      ..Default::default()
    };
    assert!(config.is_configured()); // bind_address is set, so it's configured

    // Test Unix socket config
    let config = ServerConfig {
      listener: ListenerConfig { network: UNIX_NETWORK.to_string(), ..Default::default() },
      ..Default::default()
    };
    assert!(!config.is_configured()); // No socket_path yet

    let config = ServerConfig {
      listener: ListenerConfig {
        network: UNIX_NETWORK.to_string(),
        socket_path: "/tmp/socket.sock".to_string(),
        ..Default::default()
      },
      ..Default::default()
    };
    assert!(config.is_configured());

    // Test invalid network type
    let config = ServerConfig {
      listener: ListenerConfig {
        network: "invalid".to_string(),
        bind_address: "127.0.0.1:8080".to_string(),
        ..Default::default()
      },
      ..Default::default()
    };
    assert!(!config.is_configured());
  }
}
