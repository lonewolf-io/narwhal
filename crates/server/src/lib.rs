// SPDX-License-Identifier: BSD-3-Clause

mod util;

pub mod c2s;
pub mod channel;
pub mod notifier;
pub mod router;
pub mod telemetry;
pub mod transmitter;
pub mod version;

use std::env;
use std::fs;
use std::sync::Arc;

use crate::channel::ChannelManager;
use crate::notifier::Notifier;
use crate::router::GlobalRouter;
use crate::version::{GIT_BRANCH_NAME, GIT_COMMIT_HASH, VERSION};

use futures::{FutureExt, select};
use rlimit::Resource;
use serde::{Deserialize, Serialize};
use tracing::info;

use narwhal_util::string_atom::StringAtom;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
  #[serde(default)]
  telemetry: telemetry::Config,

  #[serde(rename = "c2s-server", default)]
  c2s_server: c2s::Config,

  #[serde(default)]
  modulator: narwhal_modulator::Config,
}

/// Runs the narwhal server with the specified number of worker threads.
///
/// This is the main entry point for the narwhal server.
///
/// # Arguments
///
///
/// # Returns
///
/// Returns `Ok(())` on successful shutdown, or an error if any step fails during
/// startup or shutdown.
pub async fn run(config_file: Option<String>) -> anyhow::Result<()> {
  // Parse the configuration file.
  let mut cfg = load_config(config_file)?;

  // Initialize the telemetry subscriber.
  telemetry::init(cfg.telemetry)?;

  info!(version = VERSION, branch = GIT_BRANCH_NAME, commit = GIT_COMMIT_HASH, "narwhal server is starting...");

  // Set file descriptor limit based on configuration.
  let max_connections = cfg.c2s_server.limits.max_connections;

  set_file_descriptor_limit(max_connections)?;

  // Initialize the modulator.
  let mut modulator_service = narwhal_modulator::init_modulator(
    cfg.modulator.clone(),
    cfg.c2s_server.limits.max_message_size,
    cfg.c2s_server.limits.max_payload_size,
  )
  .await?;

  // Adjust message and payload limits based on the modulator's capabilities.
  cfg.c2s_server.limits.max_message_size = modulator_service.adjusted_max_message_size;
  cfg.c2s_server.limits.max_payload_size = modulator_service.adjusted_max_payload_size;

  let c2s_config = Arc::new(cfg.c2s_server);

  let max_channels = c2s_config.limits.max_channels;
  let max_clients_per_channel = c2s_config.limits.max_clients_per_channel;
  let max_channels_per_client = c2s_config.limits.max_channels_per_client;
  let max_payload_size = c2s_config.limits.max_payload_size;

  let local_domain = StringAtom::from(c2s_config.listener.domain.as_str());
  let c2s_router = c2s::Router::new(local_domain.clone());

  let global_router = GlobalRouter::new(c2s_router.clone());

  let notifier = Notifier::new(global_router.clone(), modulator_service.modulator.clone());

  let channel_mng = ChannelManager::new(
    global_router.clone(),
    notifier.clone(),
    max_channels,
    max_clients_per_channel,
    max_channels_per_client,
    max_payload_size,
  );

  let c2s_dispatcher_factory = c2s::conn::C2sDispatcherFactory::new(
    c2s_config.clone(),
    channel_mng.clone(),
    c2s_router.clone(),
    modulator_service.modulator.clone(),
  )
  .await?;

  let c2s_conn_rt = c2s::conn::C2sConnRuntime::new(c2s_config.as_ref()).await;

  let mut c2s_ln = c2s::C2sListener::new(c2s_config.listener.clone(), c2s_conn_rt, c2s_dispatcher_factory);

  // Start routing task for modulator private payloads.
  let mut route_m2s_payload_handle = Option::<(monoio::task::JoinHandle<()>, async_channel::Sender<()>)>::None;

  if let Some(m2s_payload_rx) = modulator_service.m2s_payload_rx.take() {
    route_m2s_payload_handle = Some(c2s::route_m2s_private_payload(m2s_payload_rx, c2s_router));
  }

  modulator_service.bootstrap().await?;
  c2s_ln.bootstrap().await?;

  // Wait for stop signal.
  info!("waiting for stop signal... (press Ctrl+C to stop the server)");
  wait_for_stop_signal().await?;
  info!("received stop signal... gracefully shutting down...");

  c2s_ln.shutdown().await?;
  modulator_service.shutdown().await?;

  if let Some((handle, shutdown_tx)) = route_m2s_payload_handle {
    shutdown_tx.close();
    let _ = handle.await;
  }

  info!("hasta la vista, baby");

  Ok(())
}

/// Sets up a custom panic hook that provides helpful debugging information when the server panics.
///
/// This should be called early in the application's initialization to ensure
/// all panics are caught and reported with the enhanced debugging information.
pub fn setup_panic_hook() {
  let orig_hook = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |panic_info| {
    eprintln!("\n===========================================================");
    eprintln!("                😱 Oops! something went wrong                ");
    eprintln!("===========================================================\n");
    eprintln!("Narwhal server has panicked. This is a bug. Please report this");
    eprintln!("at https://github.com/narwhal-io/narwhal/issues/new.");
    eprintln!("If you can reliably reproduce this panic, include the");
    eprintln!("reproduction steps and re-run with the RUST_BACKTRACE=1 env");
    eprintln!("var set and include the backtrace in your report.");
    eprintln!();
    eprintln!("Platform: {} {}", env::consts::OS, env::consts::ARCH);
    eprintln!("Version: {}", version::VERSION);
    eprintln!("Branch: {}", version::GIT_BRANCH_NAME);
    eprintln!("Commit: {}", version::GIT_COMMIT_HASH);
    eprintln!("Args: {:?}", env::args().collect::<Vec<_>>());
    eprintln!();

    orig_hook(panic_info);

    std::process::exit(1);
  }));
}

fn load_config(config_file: Option<String>) -> anyhow::Result<Config> {
  let toml_file = config_file.unwrap_or("config.toml".to_string());

  // Read and parse the TOML file
  let config: Config = match fs::read_to_string(&toml_file) {
    Ok(config_content) => toml::from_str(&config_content)
      .map_err(|err| anyhow::anyhow!("failed to parse config file: {}, {}", toml_file, err))?,
    Err(_) => Config::default(),
  };

  Ok(config)
}

async fn wait_for_stop_signal() -> anyhow::Result<()> {
  let sig_term = monoio::utils::CtrlC::new().expect("failed to create SIGTERM handler");

  select! {
    _ = sig_term.fuse() => Ok(()),
  }
}

fn set_file_descriptor_limit(max_connections: u32) -> anyhow::Result<()> {
  // Calculate desired fd limit: max_connections + overhead (25% of max) + 32 (internal usage)
  let overhead = max_connections / 4; // 25% overhead
  let desired_fd_limit = max_connections + overhead + 32;

  let (soft, hard) = rlimit::getrlimit(Resource::NOFILE)?;

  let new_soft_limit = std::cmp::min(desired_fd_limit as u64, hard);

  rlimit::setrlimit(Resource::NOFILE, new_soft_limit, hard)
    .map_err(|err| anyhow::anyhow!("failed to set file descriptor limit: {}", err))?;

  info!(new_soft_limit, soft_limit = soft, hard_limit = hard, "set file descriptor limit");

  Ok(())
}
