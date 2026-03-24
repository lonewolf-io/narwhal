// SPDX-License-Identifier: BSD-3-Clause

use anyhow::anyhow;
use monoio::io::AsyncWriteRentExt;
use monoio::net::TcpListener;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use std::io::stdout;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::metadata::LevelFilter;
use tracing::warn;
use tracing_subscriber::fmt;

use narwhal_common::runtime;

/// A metrics registry that supports dynamic metric registration.
pub type MetricsRegistry = Arc<async_lock::Mutex<Registry>>;

/// Configuration for the telemetry system
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
  /// the logging configuration
  #[serde(default)]
  logging: LoggingConfig,

  /// the metrics configuration
  #[serde(default)]
  pub metrics: MetricsConfig,
}

/// Configuration for the logging system
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggingConfig {
  /// the logging level
  #[serde(default = "default_level")]
  level: String,

  /// the logging format
  #[serde(default = "default_format")]
  format: String,
}

// === impl LoggingConfig ===

impl Default for LoggingConfig {
  fn default() -> Self {
    Self { level: default_level(), format: default_format() }
  }
}

fn default_level() -> String {
  "info".to_string()
}

fn default_format() -> String {
  "text".to_string()
}

/// Configuration for the metrics system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricsConfig {
  /// Whether metrics collection is enabled.
  #[serde(default = "default_metrics_enabled")]
  pub enabled: bool,

  /// The bind address for the Prometheus scrape endpoint.
  #[serde(default = "default_metrics_bind_address")]
  pub bind_address: String,

  /// The port for the Prometheus scrape endpoint.
  #[serde(default = "default_metrics_port")]
  pub port: u16,
}

// === impl MetricsConfig ===

impl Default for MetricsConfig {
  fn default() -> Self {
    Self {
      enabled: default_metrics_enabled(),
      bind_address: default_metrics_bind_address(),
      port: default_metrics_port(),
    }
  }
}

fn default_metrics_enabled() -> bool {
  true
}

fn default_metrics_bind_address() -> String {
  "0.0.0.0".to_string()
}

fn default_metrics_port() -> u16 {
  9090
}

/// Initializes the telemetry subsystem (logging + metrics).
pub fn init(config: &Config) -> anyhow::Result<MetricsRegistry> {
  // Convert the string level to a tracing Level
  let level_filter = match config.logging.level.to_lowercase().as_str() {
    "trace" => LevelFilter::TRACE,
    "debug" => LevelFilter::DEBUG,
    "info" => LevelFilter::INFO,
    "warn" => LevelFilter::WARN,
    "error" => LevelFilter::ERROR,
    _ => return Err(anyhow!("invalid logging level: {}", config.logging.level)),
  };

  // Set up the tracing subscriber based on the format
  match config.logging.format.as_str() {
    "json" => init_json_logger(level_filter),
    "text" => init_text_logger(level_filter),
    _ => return Err(anyhow!("invalid logging format: {}", config.logging.format)),
  };

  let registry: MetricsRegistry = Arc::new(async_lock::Mutex::new(Registry::default()));

  // Start the scrape endpoint (if enabled).
  start_scrape_endpoint(&config.metrics, registry.clone())?;

  Ok(registry)
}

/// Spawns the Prometheus scrape endpoint on the current async runtime.
///
/// If metrics are disabled in the configuration, this is a no-op.
fn start_scrape_endpoint(config: &MetricsConfig, registry: MetricsRegistry) -> anyhow::Result<()> {
  if !config.enabled {
    return Ok(());
  }

  let addr: SocketAddr = format!("{}:{}", config.bind_address, config.port)
    .parse()
    .map_err(|e| anyhow!("invalid metrics bind address: {}", e))?;

  let listener = {
    let std_listener =
      std::net::TcpListener::bind(addr).map_err(|e| anyhow!("failed to bind metrics endpoint on {}: {}", addr, e))?;
    std_listener.set_nonblocking(true)?;

    TcpListener::from_std(std_listener)?
  };

  runtime::spawn_detached(async move {
    loop {
      match listener.accept().await {
        Ok((mut stream, _)) => {
          let mut body = String::new();
          let registry = registry.lock().await;
          if prometheus_client::encoding::text::encode(&mut body, &registry).is_ok() {
            drop(registry);
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
              body.len(),
              body
            );
            let _ = stream.write_all(response.into_bytes()).await;
          }
        },
        Err(e) => {
          warn!(error = %e, "metrics scrape endpoint accept error");
        },
      }
    }
  });

  Ok(())
}

fn init_json_logger(level_filter: LevelFilter) {
  use tracing_subscriber::prelude::*;

  let fmt_layer =
    fmt::Layer::new().json().with_target(false).with_timer(fmt::time::UtcTime::rfc_3339()).with_writer(stdout);

  let registry = tracing_subscriber::registry().with(fmt_layer.with_filter(level_filter));

  registry.init();
}

fn init_text_logger(level_filter: LevelFilter) {
  use tracing_subscriber::prelude::*;

  let fmt_layer = fmt::Layer::new()
    .pretty()
    .with_target(false)
    .with_file(false)
    .with_line_number(false)
    .with_writer(stdout)
    .compact();

  let registry = tracing_subscriber::registry().with(fmt_layer.with_filter(level_filter));

  registry.init();
}
