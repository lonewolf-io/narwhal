// SPDX-License-Identifier: BSD-3-Clause

pub mod c2s_suite;
pub mod m2s_suite;
pub mod mock;
pub mod modulator;
pub mod s2m_suite;
pub mod tls;

pub use c2s_suite::{C2sSuite, default_c2s_config};
pub use m2s_suite::{M2sSuite, default_m2s_config};
pub use mock::{FailingChannelStore, FailingMessageLogFactory, InMemoryChannelStore};
pub use modulator::{TestModulator, default_s2m_config};
pub use s2m_suite::{S2mSuite, default_s2m_config_with_secret};

use std::cell::UnsafeCell;
use std::io::Cursor;
use std::rc::Rc;
use std::time::Duration;

use anyhow::anyhow;
use narwhal_common::runtime;
use narwhal_protocol::{Message, deserialize, serialize};
use narwhal_util::pool::MutablePoolBuffer;

/// A testing macro for asserting that a message matches an expected type and parameters.
///
/// This macro is designed to simplify testing of message-based protocols where messages
/// are typically represented as enums with associated data. It performs two levels of
/// validation:
/// 1. Ensures the message is of the expected variant/type
/// 2. Validates that the message parameters match the expected values
///
/// If either check fails, the macro will panic with a descriptive error message,
/// making it ideal for use in unit tests and integration tests.
///
/// # Parameters
///
/// * `$msg` - An expression that evaluates to the message to be tested
/// * `$message_type` - The path to the expected message variant (e.g., `Message::Connect`)
/// * `$expected_params` - The expected parameters/data associated with the message
///
/// # Panics
///
/// This macro will panic if:
/// - The message is not of the expected type/variant
/// - The message parameters don't match the expected values (using `assert_eq!`)
#[macro_export]
macro_rules! assert_message {
  ($msg:expr, $message_type:path, $expected_params:expr) => {
    match $msg {
      $message_type(params) => {
        assert_eq!(params, $expected_params);
      },
      _ => panic!("expected {} message", stringify!($message_type)),
    }
  };
}

/// A lock-free shared stream wrapper for single-threaded completion-based runtimes.
///
/// Multiple clones of a `LocalStream` share the same underlying stream via
/// `Rc<UnsafeCell<S>>`.
///
/// # Safety
///
/// Only safe within a **single-threaded**, cooperative async runtime.
/// Because only one task ever executes at a time, the two halves
/// never actually access the inner stream concurrently.
struct LocalStream<S>(Rc<UnsafeCell<S>>);

impl<S> LocalStream<S> {
  fn new(stream: S) -> Self {
    Self(Rc::new(UnsafeCell::new(stream)))
  }
}

impl<S> Clone for LocalStream<S> {
  fn clone(&self) -> Self {
    LocalStream(Rc::clone(&self.0))
  }
}

mod compio_impls {
  use super::*;
  use compio::buf::{IoBuf, IoBufMut};
  use compio::io::{AsyncRead, AsyncWrite};

  impl<S: AsyncRead> AsyncRead for LocalStream<S> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
      let stream = unsafe { &mut *self.0.get() };
      stream.read(buf).await
    }
  }

  impl<S: AsyncWrite> AsyncWrite for LocalStream<S> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
      let stream = unsafe { &mut *self.0.get() };
      stream.write(buf).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
      let stream = unsafe { &mut *self.0.get() };
      stream.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
      let stream = unsafe { &mut *self.0.get() };
      stream.shutdown().await
    }
  }
}

type TestStreamReader<S> = narwhal_util::codec_compio::StreamReader<LocalStream<S>>;

/// A test connection wrapper for async streams that provides message-based communication.
///
/// Uses a `LocalStream` wrapper to share the underlying stream between reader and writer
/// halves without splitting.
///
/// # Type Parameters
///
/// * `S` - Any type that implements the runtime's async read/write traits,
///   typically network streams like `TcpStream` or TLS-wrapped streams
pub struct TestConn<S> {
  /// The writer handle (a clone of the shared stream).
  writer: LocalStream<S>,

  /// The reader handle, wrapped in a runtime-specific `StreamReader`.
  reader: TestStreamReader<S>,

  /// Buffer for serialising outbound messages.
  write_buffer: Vec<u8>,
}

// === impl TestConn ===

impl<S: compio::io::AsyncRead + compio::io::AsyncWrite> TestConn<S> {
  /// Creates a new `TestConn` instance from an async stream.
  pub fn new(stream: S, read_buffer: MutablePoolBuffer, max_message_size: usize) -> Self {
    let local_stream = LocalStream::new(stream);
    let reader_stream = local_stream.clone();

    let write_buffer = vec![0u8; max_message_size];
    let stream_reader = narwhal_util::codec_compio::StreamReader::with_pool_buffer(reader_stream, read_buffer);

    Self { writer: local_stream, reader: stream_reader, write_buffer }
  }

  /// Serializes and sends a message over the connection.
  pub async fn write_message(&mut self, message: Message) -> anyhow::Result<()> {
    use compio::io::{AsyncWrite, AsyncWriteExt};

    let n = serialize(&message, &mut self.write_buffer)?;
    let data = self.write_buffer[..n].to_vec();

    let compio::buf::BufResult(result, _) = self.writer.write_all(data).await;
    result.map_err(|e| anyhow!(e))?;
    self.writer.flush().await?;

    Ok(())
  }

  /// Reads and deserializes a message from the connection with a 10-second timeout.
  pub async fn read_message(&mut self) -> anyhow::Result<Message> {
    self.try_read_message(Duration::from_secs(10)).await?.ok_or_else(|| anyhow!("timeout waiting for message"))
  }

  /// Reads and deserializes a message from the connection with a custom timeout.
  pub async fn try_read_message(&mut self, timeout: Duration) -> anyhow::Result<Option<Message>> {
    let reader = &mut self.reader;

    let line = match runtime::timeout(timeout, reader.next()).await {
      Ok(Ok(true)) => reader.get_line().unwrap(),
      Ok(Ok(false)) => return Err(anyhow!("no message received")),
      Ok(Err(e)) => return Err(anyhow!("error reading from stream: {}", e)),
      Err(_) => return Ok(None),
    };

    let msg = deserialize(Cursor::new(line))?;
    Ok(Some(msg))
  }

  /// Writes raw bytes directly to the connection without any encoding or framing.
  pub async fn write_raw_bytes(&mut self, data: &[u8]) -> anyhow::Result<()> {
    use compio::io::{AsyncWrite, AsyncWriteExt};

    let data = data.to_vec();
    let compio::buf::BufResult(result, _) = self.writer.write_all(data).await;
    result.map_err(|e| anyhow!(e))?;
    self.writer.flush().await?;

    Ok(())
  }

  /// Reads raw bytes directly from the connection into the provided buffer.
  pub async fn read_raw_bytes(&mut self, data: &mut [u8]) -> anyhow::Result<()> {
    let len = data.len();
    let buf = vec![0u8; len];

    let result = self.reader.read_raw(buf, len).await?;
    data.copy_from_slice(&result[..len]);

    Ok(())
  }

  /// Gracefully shuts down the connection's write half.
  pub async fn shutdown(&mut self) -> anyhow::Result<()> {
    use compio::io::AsyncWrite;

    self.writer.shutdown().await?;
    Ok(())
  }

  /// Asserts that no message is received within the given timeout.
  pub async fn expect_read_timeout(&mut self, timeout: Duration) -> anyhow::Result<()> {
    let reader = &mut self.reader;

    match runtime::timeout(timeout, reader.next()).await {
      Ok(Ok(true)) => Err(anyhow!("expected timeout, but received a message")),
      Ok(Ok(false)) => Err(anyhow!("no message received")),
      Ok(Err(e)) => Err(anyhow!("error reading from stream: {}", e)),
      Err(_) => Ok(()),
    }
  }
}
