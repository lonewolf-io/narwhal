// SPDX-License-Identifier: BSD-3-Clause

use std::io::stdout;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use tracing::metadata::LevelFilter;
use tracing_subscriber::fmt;

/// Configuration for the telemetry system
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
  /// the logging configuration
  #[serde(default)]
  logging: LoggingConfig,
}

/// Configuration for the logging system
#[derive(Debug, Serialize, Deserialize)]
pub struct LoggingConfig {
  /// the logging level
  #[serde(default = "default_level")]
  level: String,

  /// the logging format
  #[serde(default = "default_format")]
  format: String,
}

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

/// Initializes the telemetry system based on the provided configuration.
///
/// # Arguments
/// * `config` - The configuration for the telemetry system
///
/// # Returns
/// An error if the telemetry system could not be initialized
pub fn init(config: Config) -> anyhow::Result<()> {
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
