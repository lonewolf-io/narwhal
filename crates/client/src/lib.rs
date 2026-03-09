// SPDX-License-Identifier: BSD-3-Clause

//! # Narwhal Client Library
//!
//! This crate provides client implementations for connecting to Narwhal servers.
//!
//! ## I/O Models
//!
//! Two runtime back-ends are available as public modules — pick the one that
//! matches your application's async runtime:
//!
//! - **[`tokio`]** — poll/readiness-based (Tokio)
//! - **[`monoio`]** — completion-based (monoio)
//!
//! Import the client types you need directly from the corresponding module,
//! e.g. `narwhal_client::tokio::C2sClient` or `narwhal_client::monoio::C2sClient`.
//!
//! ## Client Types
//!
//! - **C2S**: End-user clients connecting to the Narwhal server
//! - **S2M**: Server-initiated connections to modulators
//! - **M2S**: Modulator-initiated connections for sending private messages
//!
//! ## Example
//!
//! ```ignore
//! use narwhal_client::tokio::{C2sClient, TlsDialer};
//! use narwhal_client::{C2sConfig, AuthMethod};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = C2sConfig {
//!     address: "example.com:5555".to_string(),
//!     ..Default::default()
//! };
//! let client = C2sClient::new(config, auth_method)?;
//! # Ok(())
//! # }
//! ```

pub(crate) mod auth;
pub(crate) mod config;
pub(crate) mod conn_state;
pub mod error;
pub(crate) mod object_pool;

pub mod monoio;
pub mod tokio;

pub use auth::{AuthMethod, Authenticator, AuthenticatorFactory};

pub use config::{
  C2sConfig, C2sSessionExtraInfo, Config, M2sConfig, M2sSessionExtraInfo, S2mConfig, S2mSessionExtraInfo, SessionInfo,
  TCP_NETWORK, UNIX_NETWORK,
};

pub use error::{Error, Result};
