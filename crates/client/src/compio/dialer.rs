// SPDX-License-Identifier: BSD-3-Clause

use std::fmt;
use std::sync::Arc;

use ::compio::buf::{BufResult, IoBuf, IoBufMut};
use ::compio::io::{AsyncRead, AsyncWrite};
use ::compio::net::{TcpStream, UnixStream};
use anyhow::anyhow;

/// A trait for establishing network connections using compio's I/O.
///
/// This is the compio counterpart of [`narwhal_util::conn::Dialer`] which is
/// built on futures-io / Tokio.
///
/// The trait itself is `Send + Sync` (so it can live inside an `Arc` shared
/// across threads), but the returned future is `!Send` because compio
/// operations are tied to the calling thread's event loop.
#[async_trait::async_trait(?Send)]
pub trait Dialer: Send + Sync + 'static {
  /// The type of stream produced by this dialer.
  type Stream: AsyncRead + AsyncWrite + Unpin + 'static;

  /// Opens a new connection.
  async fn dial(&self) -> anyhow::Result<Self::Stream>;
}

/// A unified stream type for compio-based TCP and Unix domain socket
/// connections.
pub enum Stream {
  /// A TCP network connection.
  Tcp(TcpStream),

  /// A Unix domain socket connection.
  #[cfg(unix)]
  Unix(UnixStream),
}

impl fmt::Debug for Stream {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Stream::Tcp(_) => f.debug_tuple("Stream::Tcp").field(&"<TcpStream>").finish(),
      #[cfg(unix)]
      Stream::Unix(_) => f.debug_tuple("Stream::Unix").field(&"<UnixStream>").finish(),
    }
  }
}

impl AsyncRead for Stream {
  async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
    match self {
      Stream::Tcp(s) => s.read(buf).await,
      #[cfg(unix)]
      Stream::Unix(s) => s.read(buf).await,
    }
  }
}

impl AsyncWrite for Stream {
  async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
    match self {
      Stream::Tcp(s) => s.write(buf).await,
      #[cfg(unix)]
      Stream::Unix(s) => s.write(buf).await,
    }
  }

  async fn flush(&mut self) -> std::io::Result<()> {
    match self {
      Stream::Tcp(s) => s.flush().await,
      #[cfg(unix)]
      Stream::Unix(s) => s.flush().await,
    }
  }

  async fn shutdown(&mut self) -> std::io::Result<()> {
    match self {
      Stream::Tcp(s) => s.shutdown().await,
      #[cfg(unix)]
      Stream::Unix(s) => s.shutdown().await,
    }
  }
}

/// TCP dialer for compio.
#[derive(Clone, Debug)]
pub struct TcpDialer {
  address: String,
}

impl TcpDialer {
  /// Creates a new TCP dialer targeting `address` (e.g. `"host:port"`).
  pub fn new(address: String) -> Self {
    Self { address }
  }
}

// === impl TcpDialer ===

#[async_trait::async_trait(?Send)]
impl Dialer for TcpDialer {
  type Stream = Stream;

  async fn dial(&self) -> anyhow::Result<Stream> {
    let tcp = ::compio::net::TcpStream::connect(&self.address)
      .await
      .map_err(|e| anyhow!("failed to connect to {}: {}", self.address, e))?;

    tcp.set_nodelay(true)?;

    Ok(Stream::Tcp(tcp))
  }
}

/// Unix domain-socket dialer for compio.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct UnixDialer {
  socket_path: String,
}

#[cfg(unix)]
impl UnixDialer {
  /// Creates a new Unix dialer targeting `socket_path`.
  pub fn new(socket_path: String) -> Self {
    Self { socket_path }
  }
}

// === impl UnixDialer ===

#[cfg(unix)]
#[async_trait::async_trait(?Send)]
impl Dialer for UnixDialer {
  type Stream = Stream;

  async fn dial(&self) -> anyhow::Result<Stream> {
    let unix = ::compio::net::UnixStream::connect(&self.socket_path)
      .await
      .map_err(|e| anyhow!("failed to connect to {}: {}", self.socket_path, e))?;
    Ok(Stream::Unix(unix))
  }
}

/// The concrete stream type produced by [`TlsDialer`].
pub type TlsStream = compio_tls::TlsStream<::compio::net::TcpStream>;

/// TLS dialer for compio — produces [`TlsStream`].
#[derive(Clone)]
pub struct TlsDialer {
  address: String,
  tls_connector: compio_tls::TlsConnector,
  server_name: String,
}

// === impl TlsDialer ===

impl TlsDialer {
  /// Creates a new TLS dialer with proper certificate verification enabled.
  pub fn new(address: String) -> anyhow::Result<Self> {
    Self::with_certificate_verification(address, true)
  }

  /// Creates a new TLS dialer with configurable certificate verification.
  ///
  /// # Security Warning
  ///
  /// **DANGER**: Setting `verify_certificates` to `false` disables all
  /// certificate validation, making the connection vulnerable to
  /// man-in-the-middle attacks.  This should ONLY be used in
  /// development/testing environments.
  pub fn with_certificate_verification(address: String, verify_certificates: bool) -> anyhow::Result<Self> {
    let tls_config = if verify_certificates {
      rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()))
        .with_no_client_auth()
    } else {
      rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth()
    };

    let tls_connector = compio_tls::TlsConnector::from(Arc::new(tls_config));

    let server_name = address.split(':').next().ok_or_else(|| anyhow!("invalid address format"))?.to_string();

    Ok(Self { address, tls_connector, server_name })
  }
}

#[async_trait::async_trait(?Send)]
impl Dialer for TlsDialer {
  type Stream = TlsStream;

  async fn dial(&self) -> anyhow::Result<TlsStream> {
    let tcp_stream = ::compio::net::TcpStream::connect(&self.address)
      .await
      .map_err(|e| anyhow!("failed to connect to {}: {}", self.address, e))?;

    tcp_stream.set_nodelay(true)?;

    let tls_stream = self
      .tls_connector
      .connect(&self.server_name, tcp_stream)
      .await
      .map_err(|e| anyhow!("TLS handshake failed: {}", e))?;

    Ok(tls_stream)
  }
}

/// Certificate verifier that accepts all certificates without validation.
///
/// # Security Warning
///
/// **DANGER**: This verifier accepts ALL certificates without any validation.
/// This makes connections vulnerable to man-in-the-middle attacks and should
/// **ONLY** be used in development/testing environments.  **NEVER** use this
/// in production.
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
