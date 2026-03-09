// SPDX-License-Identifier: BSD-3-Clause

use std::fmt;

use narwhal_protocol::ErrorParameters;

/// Errors returned by narwhal client operations.
#[derive(Debug)]
pub enum Error {
  /// The server responded with an explicit protocol-level ERROR message.
  Protocol {
    /// The error reason code.
    reason: String,
    /// Optional human-readable detail.
    detail: Option<String>,
  },

  /// Any other error (transport, serialization, timeout, etc.).
  Other(anyhow::Error),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Error::Protocol { reason, detail } => {
        write!(f, "protocol error: {reason}")?;
        if let Some(detail) = detail {
          write!(f, " ({detail})")?;
        }
        Ok(())
      },
      Error::Other(err) => write!(f, "{err}"),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Error::Protocol { .. } => None,
      Error::Other(err) => Some(err.as_ref()),
    }
  }
}

impl From<anyhow::Error> for Error {
  fn from(err: anyhow::Error) -> Self {
    Error::Other(err)
  }
}

impl From<ErrorParameters> for Error {
  fn from(params: ErrorParameters) -> Self {
    Error::Protocol { reason: params.reason.to_string(), detail: params.detail.map(|d| d.to_string()) }
  }
}

/// A type alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;
