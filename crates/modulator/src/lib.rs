// SPDX-License-Identifier: BSD-3-Clause

pub mod config;

mod client;
pub mod conn;
pub mod listener;
pub mod modulator;

use std::sync::Arc;

use anyhow::Ok;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use narwhal_common::conn::ConnRuntime;
use narwhal_common::service::{M2sService, S2mService};

pub use crate::config::*;
pub use crate::listener::{M2sListener, S2mListener};
pub use crate::modulator::{Modulator, OutboundPrivatePayload};
use narwhal_client::monoio::s2m::S2mClient;

/// The S2M modulator type.
pub const S2M_CLIENT_MODULATOR: &str = "s2m";

const M2S_SERVER_QUEUE_SIZE: usize = 128 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
  /// The type of the modulator.
  #[serde(default)]
  pub r#type: String,

  /// The configuration for the S2M client.
  #[serde(rename = "s2m-client", default)]
  pub s2m_client: S2mClientConfig,

  #[serde(rename = "m2s-server", default)]
  pub m2s_server: M2sServerConfig,
}

// === impl Config ===

impl Config {
  fn validate(&self) -> anyhow::Result<()> {
    match self.r#type.as_str() {
      S2M_CLIENT_MODULATOR => self.s2m_client.validate()?,
      _ => anyhow::bail!("unrecognized modulator type: {}", self.r#type.as_str()),
    }
    Ok(())
  }
}

#[derive(Default)]
pub struct ModulatorService {
  /// The modulator instance
  pub modulator: Option<Arc<dyn Modulator>>,

  /// The M2S private payload receiver, if applicable
  pub m2s_payload_rx: Option<async_broadcast::Receiver<OutboundPrivatePayload>>,

  /// The maximum size of a message that can be sent to the modulator.
  pub adjusted_max_message_size: u32,

  /// The maximum size of a payload that can be sent to the modulator.
  pub adjusted_max_payload_size: u32,

  /// The S2M client instance, if applicable
  s2m_client: Option<S2mClient>,

  /// The M2S listener instance, if applicable
  m2s_listener: Option<M2sListener>,
}

// ===== impl ModulatorService =====

impl ModulatorService {
  /// Initializes and starts the modulator service components.
  ///
  /// This method bootstraps any configured M2S listener, preparing it to accept
  /// incoming connections. This should be called after creating the service but
  /// before using it for message processing.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the bootstrap process completes successfully.
  ///
  /// # Errors
  ///
  /// Returns an error if the M2S listener fails to bootstrap.
  pub async fn bootstrap(&mut self) -> anyhow::Result<()> {
    if let Some(m2s_ln) = &mut self.m2s_listener {
      m2s_ln.bootstrap().await?;
    }
    Ok(())
  }

  /// Gracefully shuts down all modulator service components.
  ///
  /// This method performs an orderly shutdown of all active components:
  /// 1. Shuts down the M2S listener if present
  /// 2. Closes the S2M client connection if established
  ///
  /// This should be called before dropping the service to ensure clean resource cleanup.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if all components shut down successfully.
  ///
  /// # Errors
  ///
  /// Returns an error if any component fails to shut down gracefully.
  /// Even if an error occurs, the shutdown process will attempt to continue
  /// with other components.
  pub async fn shutdown(&mut self) -> anyhow::Result<()> {
    if let Some(m2s_ln) = &mut self.m2s_listener {
      m2s_ln.shutdown().await?;
    }
    if let Some(s2m_client) = &mut self.s2m_client {
      s2m_client.shutdown().await?;
    }
    Ok(())
  }
}

/// Initializes a modulator service based on the provided configuration.
///
/// This function sets up the modulator infrastructure, including establishing connections
/// to S2M servers and optionally creating M2S listeners. It handles message and payload
/// size negotiation between C2S and S2M servers to ensure compatibility across the system.
///
/// # Arguments
///
/// * `config` - The modulator configuration specifying the type and connection parameters.
///   If the type is empty, returns a default ModulatorService with the provided size limits.
/// * `c2s_max_message_size` - The maximum message size limit for client-to-server communication (in bytes).
/// * `c2s_max_payload_size` - The maximum payload size limit for client-to-server communication (in bytes).
///
/// # Returns
///
/// Returns a configured `ModulatorService` containing:
/// - The modulator instance (if type is configured)
/// - Adjusted message and payload size limits (negotiated between C2S and S2M limits)
/// - S2M client connection
/// - Optional M2S listener and payload receiver (if m2s_server is configured)
///
/// # Errors
///
/// This function will return an error if:
/// - The configuration validation fails
/// - Connection to the S2M server cannot be established
/// - The M2S listener cannot be created (if configured)
pub async fn init_modulator(
  config: Config,
  c2s_max_message_size: u32,
  c2s_max_payload_size: u32,
) -> anyhow::Result<ModulatorService> {
  if config.r#type.is_empty() {
    return Ok(ModulatorService {
      adjusted_max_message_size: c2s_max_message_size,
      adjusted_max_payload_size: c2s_max_payload_size,
      ..Default::default()
    });
  }

  config.validate().map_err(|e| anyhow::anyhow!("failed to validate modulator configuration: {}", e))?;

  match config.r#type.as_str() {
    S2M_CLIENT_MODULATOR => {
      let s2m_client = S2mClient::new(config.s2m_client.clone().into())?;

      // Test the connection to the modulator.
      let (session_info, _) =
        s2m_client.session_info().await.map_err(|e| anyhow::anyhow!("failed to connect to s2m server: {}", e))?;

      let is_tcp_socket = config.s2m_client.network == narwhal_client::TCP_NETWORK;

      if is_tcp_socket {
        info!(network = config.s2m_client.network, address = config.s2m_client.address, "s2m connection established");
      } else {
        info!(
          network = config.s2m_client.network,
          socket_path = config.s2m_client.socket_path,
          "s2m connection established"
        );
      }

      // Adjust the max_message_size and max_payload_size limits to ensure compatibility with both sides.
      let adjusted_max_message_size = session_info.max_message_size.min(c2s_max_message_size);
      let adjusted_max_payload_size = session_info.max_payload_size.min(c2s_max_payload_size);

      // Log if limits are being adjusted
      if adjusted_max_message_size < c2s_max_message_size {
        warn!(
          configured = c2s_max_message_size,
          adjusted = adjusted_max_message_size,
          c2s_max_message_size_limit = c2s_max_message_size,
          s2m_max_message_size_limit = session_info.max_message_size,
          "adjusted max_message_size limit based on s2m session info"
        );
      }

      if adjusted_max_payload_size < c2s_max_payload_size {
        warn!(
          configured = c2s_max_payload_size,
          adjusted = adjusted_max_payload_size,
          c2s_max_payload_size_limit = c2s_max_payload_size,
          s2m_max_payload_size_limit = session_info.max_payload_size,
          "adjusted max_payload_size limit based on s2m session info"
        );
      }

      let mut m2s_ln: Option<M2sListener> = None;
      let mut m2s_payload_rx: Option<async_broadcast::Receiver<OutboundPrivatePayload>> = None;

      if config.m2s_server.is_configured() {
        let mut m2s_config = config.m2s_server.clone();

        m2s_config.limits.max_message_size = m2s_config.limits.max_message_size.min(adjusted_max_message_size);
        m2s_config.limits.max_payload_size = m2s_config.limits.max_payload_size.min(adjusted_max_payload_size);

        let (payload_tx, payload_rx) = async_broadcast::broadcast(M2S_SERVER_QUEUE_SIZE);

        m2s_ln = Some(create_m2s_listener(m2s_config, payload_tx).await?);
        m2s_payload_rx = Some(payload_rx);
      }

      let modulator: Option<Arc<dyn Modulator>> = Some(Arc::new(s2m_client.clone()));

      Ok(ModulatorService {
        s2m_client: Some(s2m_client.clone()),
        m2s_listener: m2s_ln,
        adjusted_max_message_size,
        adjusted_max_payload_size,
        m2s_payload_rx,
        modulator,
      })
    },
    _ => unreachable!(),
  }
}

/// Creates an S2M listener.
///
/// This function creates an S2M listener with the provided modulator.
///
/// # Type Parameters
///
/// * `M` - A type that implements the `Modulator` trait
///
/// # Arguments
///
/// * `config` - The S2M server configuration containing connection settings and parameters
/// * `modulator` - The modulator instance that will handle incoming connections and protocol logic
///
/// # Returns
///
/// Returns an `S2mListener` on success, or an error if:
/// - Configuration validation fails
pub async fn create_s2m_listener<M>(config: S2mServerConfig, modulator: M) -> anyhow::Result<S2mListener<M>>
where
  M: Modulator,
{
  config.validate().map_err(|e| anyhow::anyhow!("failed to validate s2m server configuration: {}", e))?;

  let arc_config = Arc::new(config);

  let dispatcher_factory = conn::S2mDispatcherFactory::new(arc_config.clone(), Arc::from(modulator));

  let server_config = arc_config.server.clone();

  let conn_rt = ConnRuntime::<S2mService>::new(&server_config).await;

  Ok(S2mListener::new(arc_config.server.listener.clone(), conn_rt, dispatcher_factory))
}

/// Creates an M2S listener.
///
/// This function creates an M2S listener that allows a modulator to connect to a host service.
///
/// # Arguments
///
/// * `config` - The server configuration containing network settings, timeouts, and connection limits
///
/// # Returns
///
/// Returns an `M2sListener` on success, or an error if:
/// - Configuration validation fails
pub async fn create_m2s_listener(
  config: M2sServerConfig,
  payload_tx: async_broadcast::Sender<OutboundPrivatePayload>,
) -> anyhow::Result<M2sListener> {
  config.validate().map_err(|e| anyhow::anyhow!("failed to validate m2s server configuration: {}", e))?;

  let arc_config = Arc::new(config);

  let dispatcher_factory = conn::M2sDispatcherFactory::new(arc_config.clone(), payload_tx);

  let conn_rt = ConnRuntime::<M2sService>::new(arc_config.as_ref()).await;

  Ok(M2sListener::new(arc_config.listener.clone(), conn_rt, dispatcher_factory))
}
