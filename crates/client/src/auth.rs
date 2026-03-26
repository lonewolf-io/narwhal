// SPDX-License-Identifier: BSD-3-Clause

//! Authentication types shared across tokio and compio clients.

use std::sync::Arc;

/// A trait for implementing multi-step authentication mechanisms.
///
/// This trait supports SASL-style authentication protocols where the client
/// and server exchange multiple tokens/challenges until authentication completes.
/// Implementations should maintain internal state across the authentication flow.
#[async_trait::async_trait]
pub trait Authenticator: Send {
  /// Initiates the authentication process.
  ///
  /// Returns the initial authentication token to send to the server.
  /// This is called once at the beginning of the authentication flow.
  ///
  /// # Errors
  ///
  /// Returns an error if the authenticator fails to generate the initial token.
  async fn start(&mut self) -> anyhow::Result<String>;

  /// Processes a server challenge and generates the next response.
  ///
  /// This method is called for each challenge received from the server during
  /// the authentication flow. The implementation should process the challenge
  /// and return an appropriate response token.
  ///
  /// # Arguments
  ///
  /// * `challenge` - The challenge token received from the server
  ///
  /// # Errors
  ///
  /// Returns an error if the challenge cannot be processed or if the
  /// authentication flow fails.
  async fn next(&mut self, challenge: String) -> anyhow::Result<String>;
}

/// A factory trait for creating new `Authenticator` instances.
///
/// This trait is used to create fresh authenticator instances for each connection
/// attempt. Since `Authenticator` implementations are stateful (maintaining state
/// across the multi-step authentication flow), a factory is needed to produce new
/// instances for reconnections or retry attempts.
pub trait AuthenticatorFactory: Send + Sync + 'static {
  /// Creates a new `Authenticator` instance.
  ///
  /// This method is called each time a new authentication flow needs to begin,
  /// ensuring that each attempt starts with a fresh authenticator state.
  ///
  /// # Returns
  ///
  /// Returns a boxed `Authenticator` ready to begin the authentication process.
  fn create(&self) -> Box<dyn Authenticator>;
}

/// Authentication method to use when connecting to the server.
///
/// Supports two authentication approaches:
/// - Simple username-based identification (IDENTIFY command)
/// - Multi-step authentication with custom authenticator (AUTH command)
#[derive(Clone)]
pub enum AuthMethod {
  /// Simple username-based authentication using the IDENTIFY command.
  ///
  /// This is a single-step authentication where only a username is required.
  Identify { username: String },

  /// Multi-step authentication using a custom authenticator factory.
  ///
  /// This supports SASL-style authentication protocols where multiple
  /// challenge-response exchanges may occur. The factory creates fresh
  /// authenticator instances for each connection attempt, ensuring that
  /// reconnections start with clean state.
  Auth { authenticator_factory: Arc<dyn AuthenticatorFactory> },
}

impl std::fmt::Debug for AuthMethod {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      AuthMethod::Identify { username } => write!(f, "Identify({})", username),
      AuthMethod::Auth { .. } => write!(f, "Auth"),
    }
  }
}
