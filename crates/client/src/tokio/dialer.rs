// SPDX-License-Identifier: BSD-3-Clause

//! Dialer types for the Tokio-based client.
//!
//! This module contains the [`Dialer`] trait and concrete implementations
//! for TCP, Unix domain socket, and TLS transports built on Tokio and
//! futures-io.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use ::tokio::net::{TcpStream, UnixStream};
use anyhow::anyhow;
use futures::io::{AsyncRead, AsyncWrite};
use rustls::ClientConfig;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// A trait for establishing network connections and returning a stream.
///
/// The `Dialer` trait abstracts the connection logic, allowing different implementations
/// for various transport types (TCP, Unix sockets, etc.) or custom connection strategies.
#[async_trait::async_trait]
pub trait Dialer: Send + Sync + 'static {
  /// The type of stream returned by this dialer.
  ///
  /// Must implement `AsyncRead + AsyncWrite` for bidirectional I/O operations.
  type Stream: AsyncRead + AsyncWrite + Send + Sync + 'static;

  /// Establishes a connection and returns a stream.
  ///
  /// # Returns
  ///
  /// Returns a `Stream` on successful connection, or an error if the connection fails.
  async fn dial(&self) -> anyhow::Result<Self::Stream>;
}

/// A unified stream type that can represent either TCP or Unix domain socket connections.
///
/// This enum abstracts over the underlying transport mechanism, allowing applications
/// to work with both network and local socket connections transparently.
pub enum Stream {
  /// A TCP network connection.
  ///
  /// Used for remote connections over IP networks.
  Tcp(Compat<TcpStream>),

  /// A Unix domain socket connection.
  ///
  /// Used for high-performance local inter-process communication.
  Unix(Compat<UnixStream>),
}

impl Stream {
  /// Wraps a Tokio [`TcpStream`] into a [`Stream::Tcp`].
  pub fn from_tcp(stream: TcpStream) -> Self {
    Stream::Tcp(stream.compat())
  }

  /// Wraps a Tokio [`UnixStream`] into a [`Stream::Unix`].
  pub fn from_unix(stream: UnixStream) -> Self {
    Stream::Unix(stream.compat())
  }
}

impl std::fmt::Debug for Stream {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Stream::Tcp(_) => f.debug_tuple("Stream::Tcp").field(&"<TcpStream>").finish(),
      Stream::Unix(_) => f.debug_tuple("Stream::Unix").field(&"<UnixStream>").finish(),
    }
  }
}

impl AsyncRead for Stream {
  fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
    match self.get_mut() {
      Stream::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
      Stream::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
    }
  }
}

impl AsyncWrite for Stream {
  fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
    match self.get_mut() {
      Stream::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
      Stream::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
    }
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    match self.get_mut() {
      Stream::Tcp(stream) => Pin::new(stream).poll_flush(cx),
      Stream::Unix(stream) => Pin::new(stream).poll_flush(cx),
    }
  }

  fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    match self.get_mut() {
      Stream::Tcp(stream) => Pin::new(stream).poll_close(cx),
      Stream::Unix(stream) => Pin::new(stream).poll_close(cx),
    }
  }
}

/// TCP dialer implementation.
#[derive(Clone, Debug)]
pub struct TcpDialer {
  address: String,
}

impl TcpDialer {
  /// Creates a new TCP dialer with the specified address.
  pub fn new(address: String) -> Self {
    Self { address }
  }
}

// === impl TcpDialer ===

#[async_trait::async_trait]
impl Dialer for TcpDialer {
  type Stream = Stream;

  async fn dial(&self) -> anyhow::Result<Stream> {
    let tcp_stream =
      TcpStream::connect(&self.address).await.map_err(|e| anyhow!("failed to connect to {}: {}", self.address, e))?;

    tcp_stream.set_nodelay(true)?;

    Ok(Stream::Tcp(tcp_stream.compat()))
  }
}

/// Unix domain socket dialer implementation.
#[derive(Clone, Debug)]
pub struct UnixDialer {
  socket_path: String,
}

// === impl UnixDialer ===

impl UnixDialer {
  /// Creates a new Unix dialer with the specified socket path.
  pub fn new(socket_path: String) -> Self {
    Self { socket_path }
  }
}

#[async_trait::async_trait]
impl Dialer for UnixDialer {
  type Stream = Stream;

  async fn dial(&self) -> anyhow::Result<Stream> {
    let unix_stream = UnixStream::connect(&self.socket_path)
      .await
      .map_err(|e| anyhow!("failed to connect to {}: {}", self.socket_path, e))?;

    Ok(Stream::Unix(unix_stream.compat()))
  }
}

/// TLS dialer implementation.
#[derive(Clone)]
pub struct TlsDialer {
  address: String,
  tls_connector: TlsConnector,
  server_name: rustls::pki_types::ServerName<'static>,
}

// === impl TlsDialer ===

impl TlsDialer {
  /// Creates a new TLS dialer with the specified address and certificate verification enabled.
  ///
  /// This method creates a TLS dialer that verifies server certificates using the system's
  /// root certificate store. This is the recommended and secure default for production use.
  ///
  /// # Arguments
  ///
  /// * `address` - The server address to connect to (e.g., "example.com:443")
  ///
  /// # Returns
  ///
  /// Returns a `TlsDialer` instance configured with proper certificate verification.
  ///
  /// # Errors
  ///
  /// Returns an error if the address format is invalid or the server name cannot be parsed.
  pub fn new(address: String) -> anyhow::Result<Self> {
    Self::with_certificate_verification(address, true)
  }

  /// Creates a new TLS dialer with configurable certificate verification.
  ///
  /// # Arguments
  ///
  /// * `address` - The server address to connect to (e.g., "example.com:443")
  /// * `verify_certificates` - Whether to verify server certificates
  ///
  /// # Security Warning
  ///
  /// **DANGER**: Setting `verify_certificates` to `false` disables all certificate validation,
  /// making the connection vulnerable to man-in-the-middle attacks. This should ONLY be used
  /// in development/testing environments with self-signed certificates. **NEVER** use this
  /// setting in production environments.
  ///
  /// When `verify_certificates` is `false`, the dialer will accept:
  /// - Expired certificates
  /// - Self-signed certificates
  /// - Certificates signed by untrusted CAs
  /// - Certificates with incorrect hostnames
  ///
  /// # Returns
  ///
  /// Returns a `TlsDialer` instance configured with the specified certificate verification setting.
  ///
  /// # Errors
  ///
  /// Returns an error if the address format is invalid or the server name cannot be parsed.
  pub fn with_certificate_verification(address: String, verify_certificates: bool) -> anyhow::Result<Self> {
    let tls_config = if verify_certificates {
      // Use the system's root certificate store for proper certificate verification
      ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()))
        .with_no_client_auth()
    } else {
      // DANGEROUS: Accept all certificates without validation
      ClientConfig::builder().dangerous().with_custom_certificate_verifier(Arc::new(NoVerifier)).with_no_client_auth()
    };

    let tls_connector = TlsConnector::from(Arc::new(tls_config));

    // Extract hostname for SNI
    let server_name = address.split(':').next().ok_or_else(|| anyhow!("invalid address format"))?.to_string();

    let server_name =
      rustls::pki_types::ServerName::try_from(server_name).map_err(|_| anyhow!("invalid server name"))?.to_owned();

    Ok(Self { address, tls_connector, server_name })
  }
}

#[async_trait::async_trait]
impl Dialer for TlsDialer {
  type Stream = tokio_util::compat::Compat<TlsStream<TcpStream>>;

  async fn dial(&self) -> anyhow::Result<tokio_util::compat::Compat<TlsStream<TcpStream>>> {
    let tcp_stream =
      TcpStream::connect(&self.address).await.map_err(|e| anyhow!("failed to connect to {}: {}", self.address, e))?;

    tcp_stream.set_nodelay(true)?;

    let tls_stream = self
      .tls_connector
      .connect(self.server_name.clone(), tcp_stream)
      .await
      .map_err(|e| anyhow!("TLS handshake failed: {}", e))?;

    Ok(tls_stream.compat())
  }
}

/// Certificate verifier that accepts all certificates without validation.
///
/// # Security Warning
///
/// **DANGER**: This verifier accepts ALL certificates without any validation, including:
/// - Expired certificates
/// - Self-signed certificates
/// - Certificates signed by untrusted CAs
/// - Certificates with incorrect hostnames
/// - Revoked certificates
///
/// This makes connections vulnerable to man-in-the-middle attacks and should
/// **ONLY** be used in development/testing environments. **NEVER** use this in production.
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
  fn verify_server_cert(
    &self,
    _end_entity: &rustls::pki_types::CertificateDer<'_>,
    _intermediates: &[rustls::pki_types::CertificateDer<'_>],
    _server_name: &rustls::pki_types::ServerName<'_>,
    _ocsp_response: &[u8],
    _now: rustls::pki_types::UnixTime,
  ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
    Ok(rustls::client::danger::ServerCertVerified::assertion())
  }

  fn verify_tls12_signature(
    &self,
    _message: &[u8],
    _cert: &rustls::pki_types::CertificateDer<'_>,
    _dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
  }

  fn verify_tls13_signature(
    &self,
    _message: &[u8],
    _cert: &rustls::pki_types::CertificateDer<'_>,
    _dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
  }

  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    vec![
      rustls::SignatureScheme::RSA_PKCS1_SHA256,
      rustls::SignatureScheme::RSA_PKCS1_SHA384,
      rustls::SignatureScheme::RSA_PKCS1_SHA512,
      rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
      rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
      rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
      rustls::SignatureScheme::RSA_PSS_SHA256,
      rustls::SignatureScheme::RSA_PSS_SHA384,
      rustls::SignatureScheme::RSA_PSS_SHA512,
      rustls::SignatureScheme::ED25519,
    ]
  }
}
