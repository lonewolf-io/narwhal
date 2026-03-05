// A modulator that validates that payloads are in JSON format.
//
// SPDX-License-Identifier: BSD-3-Clause

use std::fs;
use std::io::Cursor;

use async_trait::async_trait;
use clap::Parser;
use csv::Reader;

use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

use narwhal_modulator::config::S2mServerConfig;
use narwhal_modulator::modulator::{
  AuthRequest, AuthResponse, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResponse,
  ForwardBroadcastPayloadResult, ForwardEventRequest, ForwardEventResponse, Operation, Operations,
  ReceivePrivatePayloadRequest, ReceivePrivatePayloadResponse, SendPrivatePayloadRequest, SendPrivatePayloadResponse,
};
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_modulator::{create_s2m_listener, Modulator};
use narwhal_util::string_atom::StringAtom;
use prometheus_client::registry::Registry;

const MODULATOR_PROTOCOL_NAME: &str = "broadcast-payload-csv-validator/1.0";

/// A simple CSV payload validation modulator.
///
/// This modulator implements the `Modulator` trait and validates payloads to ensure they are in valid CSV format.
/// It supports only the ForwardPayload operation.
///
/// When a payload is received, it attempts to parse it as CSV using the `csv` crate. If all records are valid,
/// the payload is considered valid. Otherwise, it is rejected.
///
/// This modulator does not support authentication or payload alteration.
///
/// Example usage:
///   - Send a payload containing valid CSV data to be accepted.
///   - Invalid or empty payloads will be rejected.
#[derive(Debug)]
struct CsvPayloadValidator {}

#[async_trait(?Send)]
impl narwhal_modulator::Modulator for CsvPayloadValidator {
  /// Returns the unique name of this modulator.
  async fn protocol_name(&self) -> anyhow::Result<StringAtom> {
    Ok(MODULATOR_PROTOCOL_NAME.into())
  }

  /// Declares that this modulator supports only the ForwardBroadcastPayload operation.
  async fn operations(&self) -> anyhow::Result<Operations> {
    Ok(Operations::new().with(Operation::ForwardBroadcastPayload))
  }

  /// This modulator does not support authentication.
  async fn authenticate(&self, _request: AuthRequest) -> anyhow::Result<AuthResponse> {
    unreachable!()
  }

  /// Validates the provided payload as CSV.
  ///
  /// The payload is parsed using the `csv` crate. If all records are valid, returns `Ok(true)`.
  /// If the payload is empty or any record is invalid, returns `Ok(false)`.
  ///
  /// # Arguments
  ///
  /// * `request` - The forward broadcast payload request containing the payload to validate
  ///
  /// # Returns
  ///
  /// `Ok(ForwardBroadcastPayloadResponse)` containing:
  /// - `result`: `Valid` if the payload is valid CSV, `Invalid` otherwise
  async fn forward_broadcast_payload(
    &self,
    request: ForwardBroadcastPayloadRequest,
  ) -> anyhow::Result<ForwardBroadcastPayloadResponse> {
    if request.payload.is_empty() {
      return Ok(ForwardBroadcastPayloadResponse { result: ForwardBroadcastPayloadResult::Invalid });
    }
    let data: &[u8] = request.payload.as_ref();

    let cursor = Cursor::new(data);
    let mut reader = Reader::from_reader(cursor);

    // Try to read all CSV records
    for result in reader.records() {
      if result.is_err() {
        return Ok(ForwardBroadcastPayloadResponse { result: ForwardBroadcastPayloadResult::Invalid });
      }
    }
    Ok(ForwardBroadcastPayloadResponse { result: ForwardBroadcastPayloadResult::Valid })
  }

  /// This modulator does not support event forwarding.
  async fn forward_event(&self, _request: ForwardEventRequest) -> anyhow::Result<ForwardEventResponse> {
    unreachable!()
  }

  /// This modulator does not support sending private payloads.
  async fn send_private_payload(
    &self,
    _request: SendPrivatePayloadRequest,
  ) -> anyhow::Result<SendPrivatePayloadResponse> {
    unreachable!()
  }

  /// This modulator does not support receiving private payloads.
  async fn receive_private_payload(
    &self,
    _request: ReceivePrivatePayloadRequest,
  ) -> anyhow::Result<ReceivePrivatePayloadResponse> {
    unreachable!()
  }
}

/// Command line arguments
#[derive(Parser, Debug)]
#[command(name = "broadcast-payload-csv-validator")]
#[command(about = "S2M example server -- broadcast payload CSV validator", long_about = None)]
struct Cli {
  /// Path to configuration file
  #[arg(short, long, value_name = "FILE")]
  config: Option<String>,
}

fn main() {
  let env_filter = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy();

  tracing_subscriber::fmt().with_env_filter(env_filter).init();

  let cli = Cli::parse();

  let config = load_config(cli.config).unwrap_or_else(|err| {
    eprintln!("error: {}", err);
    std::process::exit(1);
  });

  let modulator = CsvPayloadValidator {};

  let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new().enable_all().build().unwrap();

  rt.block_on(async {
    match run_s2m_server(config, modulator).await {
      Ok(_) => {},
      Err(e) => eprintln!("error: {}", e),
    }
  });
}

async fn run_s2m_server<M>(config: S2mServerConfig, modulator: M) -> anyhow::Result<()>
where
  M: Modulator,
{
  let protocol_name = modulator.protocol_name().await?;

  let mut core_dispatcher = CoreDispatcher::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
  core_dispatcher.bootstrap().await?;

  let mut ln = create_s2m_listener(config, modulator, core_dispatcher.clone(), &mut Registry::default()).await?;

  info!(protocol_name = protocol_name.as_ref(), "📡 starting s2m server...");

  // Bootstrap the listener.
  ln.bootstrap().await?;

  // Wait for stop signal.
  info!("waiting for stop signal... (press Ctrl+C to stop the server)");
  wait_for_stop_signal().await?;
  info!("received stop signal... gracefully shutting down...");

  // Stop listener.
  ln.shutdown().await?;
  core_dispatcher.shutdown().await?;

  info!(protocol_name = protocol_name.as_ref(), "🌙 that's all, folks!");

  Ok(())
}

fn load_config(config_file: Option<String>) -> anyhow::Result<S2mServerConfig> {
  let toml_file = config_file.unwrap_or("config.toml".to_string());

  // Read and parse the TOML file
  let config_content = fs::read_to_string(&toml_file)
    .map_err(|err| anyhow::anyhow!("failed to read config file: {}, {}", toml_file, err))?;

  let config: S2mServerConfig = toml::from_str(&config_content)
    .map_err(|err| anyhow::anyhow!("failed to parse config file: {}, {}", toml_file, err))?;

  Ok(config)
}

async fn wait_for_stop_signal() -> anyhow::Result<()> {
  let (tx, rx) = async_channel::bounded::<()>(1);

  for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
    let tx = tx.clone();
    unsafe {
      signal_hook::low_level::register(sig, move || {
        let _ = tx.try_send(());
      })?;
    }
  }

  let _ = rx.recv().await;
  Ok(())
}
