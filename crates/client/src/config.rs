// SPDX-License-Identifier: BSD-3-Clause

use std::time::Duration;

use narwhal_protocol::Nid;
use narwhal_util::string_atom::StringAtom;
use serde::{Deserialize, Serialize};

/// TCP network type identifier.
pub const TCP_NETWORK: &str = "tcp";

/// Unix domain socket network type identifier.
pub const UNIX_NETWORK: &str = "unix";

/// Internal client configuration derived from the public per-service configs.
#[derive(Clone, Debug)]
pub struct Config {
  /// Maximum number of idle connections to keep in the pool.
  pub max_idle_connections: usize,

  /// Heartbeat interval to negotiate with the server.
  pub heartbeat_interval: Duration,

  /// Connection timeout.
  pub connect_timeout: Duration,

  /// Read / write timeout for individual requests.
  pub timeout: Duration,

  /// Timeout for reading a payload from the server.
  pub payload_read_timeout: Duration,

  /// Initial delay for the exponential-backoff retry strategy.
  pub backoff_initial_delay: Duration,

  /// Maximum delay for the exponential-backoff retry strategy.
  pub backoff_max_delay: Duration,

  /// Maximum number of retry attempts.
  pub backoff_max_retries: usize,
}

/// Session parameters negotiated during the handshake.
#[derive(Copy, Clone, Debug)]
pub struct SessionInfo {
  /// The heartbeat interval negotiated with the server (ms).
  pub heartbeat_interval: u32,

  /// Maximum number of inflight requests allowed by the server.
  pub max_inflight_requests: u32,

  /// Maximum message size allowed by the server (bytes).
  pub max_message_size: u32,

  /// Maximum payload size allowed by the server (bytes).
  pub max_payload_size: u32,
}

/// Configuration for C2S connections.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct C2sConfig {
  /// The address of the server to connect to.
  #[serde(default)]
  pub address: String,

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

  /// The timeout for reading a payload from the server.
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

impl From<C2sConfig> for Config {
  fn from(val: C2sConfig) -> Self {
    Config {
      max_idle_connections: 1,
      heartbeat_interval: val.heartbeat_interval,
      connect_timeout: val.connect_timeout,
      timeout: val.timeout,
      payload_read_timeout: val.payload_read_timeout,
      backoff_initial_delay: val.backoff_initial_delay,
      backoff_max_delay: val.backoff_max_delay,
      backoff_max_retries: val.backoff_max_retries,
    }
  }
}

/// Session information returned after a successful C2S handshake.
#[derive(Clone, Debug)]
pub struct C2sSessionExtraInfo {
  pub nid: Nid,

  /// Maximum number of persisted messages per channel allowed by the server.
  pub max_persist_messages: u32,
}

/// S2M client configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct S2mConfig {
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

impl S2mConfig {
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

impl Default for S2mConfig {
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

impl From<&S2mConfig> for Config {
  fn from(config: &S2mConfig) -> Self {
    Config {
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

/// Extra session information returned after a successful S2M handshake.
#[derive(Debug, Clone)]
pub struct S2mSessionExtraInfo {
  /// The modulator's protocol identifier.
  pub protocol_name: StringAtom,

  /// Bitmask of operations supported by the modulator server.
  /// See `docs/PROTOCOL.md` "Operation Bit Assignments" for bit semantics.
  pub operation_mask: u64,
}

/// M2S client configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct M2sConfig {
  /// The network type. Either "tcp" or "unix".
  /// Default is "tcp".
  #[serde(default = "default_network")]
  pub network: String,

  /// The address of the server.
  /// This should be in the format "host:port".
  #[serde(default)]
  pub address: String,

  /// The unix domain socket path.
  /// This is used when the network type is "unix".
  #[serde(default)]
  pub socket_path: String,

  /// The shared secret for authentication.
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

impl M2sConfig {
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

impl Default for M2sConfig {
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

impl From<&M2sConfig> for Config {
  fn from(config: &M2sConfig) -> Self {
    Config {
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

/// Extra session information returned after a successful M2S handshake.
#[derive(Debug, Clone)]
pub struct M2sSessionExtraInfo {}

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
