// A modulator that validates that payloads are in JSON format.
//
// SPDX-License-Identifier: BSD-3-Clause

use std::fs;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use clap::Parser;

use narwhal_modulator::modulator::{
  AuthRequest, AuthResponse, AuthResult, ForwardBroadcastPayloadRequest, ForwardBroadcastPayloadResponse,
  ForwardEventRequest, ForwardEventResponse, Operation, Operations, ReceivePrivatePayloadRequest,
  ReceivePrivatePayloadResponse, SendPrivatePayloadRequest, SendPrivatePayloadResponse,
};
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

use narwhal_modulator::config::S2mServerConfig;
use narwhal_common::core_dispatcher::CoreDispatcher;
use narwhal_modulator::{create_s2m_listener, Modulator};
use narwhal_util::string_atom::StringAtom;

const MODULATOR_PROTOCOL_NAME: &str = "plain-authenticator/1.0";

const USERNAME: &str = "user";
const PASSWORD: &str = "pass";

/// A simple PLAIN authentication modulator.
///
/// This modulator implements the `Modulator` trait and performs authentication using the PLAIN mechanism.
/// It expects the authentication token to be a base64-encoded string of the form `\0username\0password`.
///
/// The username and password are checked against hardcoded values for demonstration purposes:
///   - Username: `user`
///   - Password: `pass`
///
/// If the credentials match, authentication succeeds and the username is returned. Otherwise, authentication fails.
///
/// Example token for username `user` and password `pass`:
///   - Raw: `\0user\0pass`
///   - Base64: `AHVzZXIAcGFzcw==`
#[derive(Debug)]
struct PlainAuthenticator {}

#[async_trait(?Send)]
impl narwhal_modulator::Modulator for PlainAuthenticator {
  /// Returns the unique name of this modulator.
  async fn protocol_name(&self) -> anyhow::Result<StringAtom> {
    Ok(MODULATOR_PROTOCOL_NAME.into())
  }

  /// Declares that this modulator supports only the Auth operation.
  async fn operations(&self) -> anyhow::Result<Operations> {
    Ok(Operations::new().with(Operation::Auth))
  }

  /// Authenticates a client using the PLAIN mechanism.
  ///
  /// The `token` parameter must be a base64-encoded string of the form `\0username\0password`.
  /// If the username and password match the hardcoded values, authentication succeeds and the username is returned.
  /// Otherwise, authentication fails.
  ///
  /// # Arguments
  /// * `token` - A base64-encoded PLAIN authentication token.
  ///
  /// # Returns
  /// * `AuthResponse` with `AuthResult::Success` containing the username if credentials are valid.
  /// * `AuthResponse` with `AuthResult::Failure` if credentials are invalid or the token is malformed.
  async fn authenticate(&self, request: AuthRequest) -> anyhow::Result<AuthResponse> {
    // Decode base64 token
    let decoded = match BASE64_STANDARD.decode(request.token.as_ref()) {
      Ok(bytes) => bytes,
      Err(e) => {
        tracing::warn!("base64 decode failed: {}", e);
        return Ok(AuthResponse { result: AuthResult::Failure });
      },
    };

    // PLAIN: \0username\0password
    let mut parts = decoded.split(|&b| b == 0);
    let _ = parts.next(); // skip authzid (can be empty)

    let username = parts.next().and_then(|s| std::str::from_utf8(s).ok());
    let password = parts.next().and_then(|s| std::str::from_utf8(s).ok());

    match (username, password) {
      (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => {
        if u == USERNAME && p == PASSWORD {
          Ok(AuthResponse { result: AuthResult::Success { username: StringAtom::from(u) } })
        } else {
          tracing::warn!("authentication failed: invalid username or password");
          Ok(AuthResponse { result: AuthResult::Failure })
        }
      },
      _ => {
        tracing::warn!("invalid PLAIN token: username or password missing");
        Ok(AuthResponse { result: AuthResult::Failure })
      },
    }
  }

  /// This modulator does not support payload forwarding.
  async fn forward_broadcast_payload(
    &self,
    _request: ForwardBroadcastPayloadRequest,
  ) -> anyhow::Result<ForwardBroadcastPayloadResponse> {
    unreachable!()
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
#[command(name = "plain-authenticator")]
#[command(about = "S2M example server -- Plain authenticator", long_about = None)]
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

  let modulator = PlainAuthenticator {};

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

  let mut ln = create_s2m_listener(config, modulator, core_dispatcher.clone()).await?;

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
