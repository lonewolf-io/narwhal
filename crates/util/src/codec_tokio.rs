// SPDX-License-Identifier: BSD-3-Clause

use futures::io::{AsyncRead, AsyncReadExt};

use crate::pool::MutablePoolBuffer;

/// Error type for stream reading operations.
#[derive(Debug)]
pub enum StreamReaderError {
  /// Occurs when a line exceeds the maximum allowed length.
  MaxLineLengthExceeded,

  /// Wraps IO errors from the underlying reader.
  IoError(std::io::Error),
}

impl std::fmt::Display for StreamReaderError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      StreamReaderError::MaxLineLengthExceeded => write!(f, "max line length exceeded"),
      StreamReaderError::IoError(e) => write!(f, "I/O error: {}", e),
    }
  }
}

impl From<std::io::Error> for StreamReaderError {
  fn from(error: std::io::Error) -> Self {
    StreamReaderError::IoError(error)
  }
}

impl std::error::Error for StreamReaderError {}

/// Asynchronous line reader that efficiently handles buffering and line parsing.
///
/// Reuses a pooled buffer to minimize allocations and efficiently handle partial reads.
/// Lines are split by '\n' characters and returned as Strings.
#[derive(Debug)]
pub struct StreamReader<R> {
  reader: R,
  pool_buffer: MutablePoolBuffer,
  current_pos: usize,
  line_pos: Option<usize>,
}

impl<R: AsyncRead + Unpin> StreamReader<R> {
  /// Creates a new LineReader with the specified async reader and pre-allocated buffer.
  ///
  /// # Arguments
  /// * `reader` - Async reader implementing AsyncRead
  /// * `pool_buffer` - Pre-allocated buffer from a pool, determines maximum line length
  pub fn with_pool_buffer(reader: R, pool_buffer: MutablePoolBuffer) -> Self {
    Self { reader, pool_buffer, current_pos: 0, line_pos: None }
  }

  /// Returns last read line from the buffer.
  ///
  /// # Returns
  /// * `Option<&[u8]>` - A slice of the last read line, or None if no line was read.
  pub fn get_line(&mut self) -> Option<&[u8]> {
    if let Some(pos) = self.line_pos {
      let line = &self.pool_buffer.as_slice()[..pos];
      return Some(line);
    }
    None
  }

  /// Asynchronously reads the next line from the input stream.
  ///
  /// # Returns
  /// * `Ok(true)` - A valid line was read, and is ready to be consumed via `get_line()` method.
  /// * `Ok(false)` - EOF reached
  /// * `Err(_)` - Contains either:
  ///   - `StreamReaderError::MaxLineLengthExceeded` if line exceeds buffer size
  ///   - `StreamReaderError::IoError` if an I/O error occurred
  ///
  /// # Behavior
  /// * Efficiently handles partial reads by preserving unprocessed bytes between calls
  /// * Lines are split by '\n' characters (LF line endings)
  /// * The trailing newline character is not included in returned strings
  /// * Maximum line length is determined by the provided buffer size
  pub async fn next(&mut self) -> anyhow::Result<bool, StreamReaderError> {
    let max_line_length = self.pool_buffer.len();

    // First, compact the buffer to remove any previously processed line.
    self.compact_buffer();

    loop {
      let buffer = &self.pool_buffer.as_slice()[..self.current_pos];

      if let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        self.line_pos = Some(pos);

        return Ok(true);
      }
      if self.current_pos == max_line_length {
        // Max line length exceeded, return an error.
        return Err(StreamReaderError::MaxLineLengthExceeded);
      }

      // Read data into the buffer.
      match self.reader.read(&mut self.pool_buffer.as_mut_slice()[self.current_pos..]).await {
        Ok(bytes_read) => {
          // Check if EOF is reached.
          if bytes_read == 0 {
            return Ok(false);
          }
          self.current_pos = (self.current_pos + bytes_read).min(max_line_length);
        },
        Err(e) => {
          return Err(StreamReaderError::from(e));
        },
      }
    }
  }

  /// Reads a specified number of raw bytes from the stream.
  ///
  /// This method fills the provided buffer with exactly the requested number of bytes.
  /// It first attempts to use any remaining buffered data from previous line reads,
  /// then reads directly from the underlying reader if more data is needed.
  ///
  /// # Arguments
  /// * `buf` - Mutable byte slice to fill with data. The length determines how many bytes to read.
  ///
  /// # Returns
  /// * `Ok(())` - Successfully read exactly `buf.len()` bytes
  /// * `Err(_)` - Contains an error if:
  ///   - The underlying reader encounters an I/O error
  ///   - EOF is reached before reading the requested number of bytes (via `read_exact`)
  pub async fn read_raw(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
    let total_needed = buf.len();
    let mut bytes_filled = 0;

    // First, try to fill the buffer from any remaining bytes in the stream reader.
    if self.remaining_bytes_count() > 0 {
      let bytes_extracted = self.extract_remaining(&mut buf[bytes_filled..], total_needed - bytes_filled);
      bytes_filled += bytes_extracted;
    }

    // If we still need more data, read directly from the underlying reader
    if bytes_filled < total_needed {
      let reader = &mut self.reader;
      reader.read_exact(&mut buf[bytes_filled..]).await?;
    }

    Ok(())
  }

  /// Extract any remaining bytes read beyond the current line.
  fn extract_remaining(&mut self, buf: &mut [u8], max_bytes: usize) -> usize {
    // First compact the buffer to move any remaining data to the beginning
    // This ensures consistent state management with the next() method
    self.compact_buffer();

    // Determine how much data we can extract
    let available_bytes = self.current_pos;
    let max_extract = if max_bytes == 0 { available_bytes } else { std::cmp::min(max_bytes, available_bytes) };
    let bytes_to_copy = std::cmp::min(buf.len(), max_extract);

    if bytes_to_copy > 0 {
      buf[..bytes_to_copy].copy_from_slice(&self.pool_buffer.as_slice()[..bytes_to_copy]);

      // If we extracted less than the total available data, move the rest to the beginning
      if bytes_to_copy < available_bytes {
        let remaining_bytes = available_bytes - bytes_to_copy;
        self.pool_buffer.as_mut_slice().copy_within(bytes_to_copy..available_bytes, 0);
        self.current_pos = remaining_bytes;
      } else {
        // We extracted all available data, clear the buffer state
        self.current_pos = 0;
      }
    }

    bytes_to_copy
  }

  /// Returns the number of bytes currently buffered beyond the current line.
  fn remaining_bytes_count(&mut self) -> usize {
    if let Some(line_pos) = self.line_pos {
      if line_pos + 1 < self.current_pos {
        return self.current_pos - (line_pos + 1);
      }
    } else if self.current_pos > 0 {
      // Data in buffer but no line found yet
      return self.current_pos;
    }
    0
  }

  /// Compacts the buffer by moving any remaining data after the last processed line
  /// to the beginning of the buffer and updates the current position accordingly.
  ///
  /// This is called internally to prepare the buffer for reading more data while
  /// preserving any unprocessed bytes from previous reads.
  fn compact_buffer(&mut self) {
    if let Some(pos) = self.line_pos.take() {
      if pos < self.current_pos {
        let remaining_len = self.current_pos - (pos + 1);
        self.pool_buffer.as_mut_slice().copy_within(pos + 1..self.current_pos, 0);
        self.current_pos = remaining_len;
      } else {
        self.current_pos = 0;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::pin::Pin;
  use std::task::{Context, Poll};

  use crate::pool::Pool;

  /// Mock reader that returns data in chunks
  struct MockChunkedReader {
    chunks: Vec<Vec<u8>>,
    current_chunk: usize,
    position: usize,
  }

  impl MockChunkedReader {
    fn new(chunks: Vec<Vec<u8>>) -> Self {
      Self { chunks, current_chunk: 0, position: 0 }
    }
  }

  impl AsyncRead for MockChunkedReader {
    fn poll_read(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
      if self.current_chunk >= self.chunks.len() {
        return Poll::Ready(Ok(0)); // EOF
      }

      let current_data = &self.chunks[self.current_chunk];
      let remaining = current_data.len() - self.position;
      let to_copy = std::cmp::min(buf.len(), remaining);

      buf[..to_copy].copy_from_slice(&current_data[self.position..self.position + to_copy]);

      let current_data_len = current_data.len();
      self.position += to_copy;
      if self.position >= current_data_len {
        self.current_chunk += 1;
        self.position = 0;
      }

      Poll::Ready(Ok(to_copy))
    }
  }

  #[tokio::test]
  async fn test_empty_lines() {
    let mock_reader = MockChunkedReader::new(vec![b"\n\n".to_vec(), b"non-empty line\n".to_vec(), b"\n".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // First two lines should be empty
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"");

    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"");

    // Then a non-empty line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"non-empty line");

    // And another empty line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"");

    // Then EOF
    assert!(!reader.next().await.unwrap());
  }

  #[tokio::test]
  async fn test_max_line_length() {
    // Create a small buffer of 10 bytes
    let pool = Pool::new(1, 10);
    let pool_buffer = pool.acquire_buffer().await;

    // Create a reader with a line longer than the buffer
    let mock_reader = MockChunkedReader::new(vec![b"This line is definitely longer than 10 bytes\n".to_vec()]);

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Should result in MaxLineLengthExceeded error
    let result = reader.next().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("max line length exceeded"));
  }

  #[tokio::test]
  async fn test_multibyte_utf8() {
    // Japanese, emoji, and other multi-byte characters
    let mock_reader = MockChunkedReader::new(vec![
      "こんにちは\n".as_bytes().to_vec(),
      "Hello 🌍\n".as_bytes().to_vec(),
      "Привет мир\n".as_bytes().to_vec(),
    ]);

    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    assert!(reader.next().await.is_ok());
    assert_eq!(reader.get_line().unwrap(), "こんにちは".as_bytes());

    assert!(reader.next().await.is_ok());
    assert_eq!(reader.get_line().unwrap(), "Hello 🌍".as_bytes());

    assert!(reader.next().await.is_ok());
    assert_eq!(reader.get_line().unwrap(), "Привет мир".as_bytes());
  }

  #[tokio::test]
  async fn test_partial_reads() {
    let mock_reader = MockChunkedReader::new(vec![
      b"Partial ".to_vec(),
      b"line without ".to_vec(),
      b"ending yet".to_vec(),
      b"\nSecond line\n".to_vec(),
    ]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let reader = mock_reader;
    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(reader, pool_buffer);

    // First call should eventually return the complete line once we have a newline.
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), "Partial line without ending yet".as_bytes());

    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), "Second line".as_bytes());
  }

  #[tokio::test]
  async fn test_exact_buffer_size() {
    // Create a buffer and a line of exactly the same size
    let buffer_size = 10;
    let pool = Pool::new(1, buffer_size);
    let pool_buffer = pool.acquire_buffer().await;

    // Exactly 10 bytes
    let mock_reader = MockChunkedReader::new(vec![b"123456789".to_vec(), b"\n".to_vec()]);

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Should successfully read exactly buffer-sized line.
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), "123456789".as_bytes());
  }

  #[tokio::test]
  async fn test_extract_remaining() {
    // Create a reader that will read multiple lines in one go
    let mock_reader = MockChunkedReader::new(vec![b"first line\nsecond line\nthird line\npartial".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read the first line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"first line");

    // Check that there are remaining bytes
    assert!(reader.remaining_bytes_count() > 0);

    // Extract all remaining bytes (max_bytes = 0 means no limit)
    let mut remaining_buf = vec![0u8; 100];
    let bytes_copied = reader.extract_remaining(&mut remaining_buf, 0);
    assert_eq!(bytes_copied, b"second line\nthird line\npartial".len());
    assert_eq!(&remaining_buf[..bytes_copied], b"second line\nthird line\npartial");

    // After extraction, the next call to next() should return false (EOF)
    // since we've consumed all the buffered data
    assert!(!reader.next().await.unwrap());
  }

  #[tokio::test]
  async fn test_extract_remaining_no_extra_data() {
    // Create a reader with exactly one line
    let mock_reader = MockChunkedReader::new(vec![b"single line\n".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read the line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"single line");

    // Should have no remaining bytes
    assert_eq!(reader.remaining_bytes_count(), 0);

    // Extract remaining should return 0 bytes copied
    let mut remaining_buf = vec![0u8; 100];
    let bytes_copied = reader.extract_remaining(&mut remaining_buf, 0);
    assert_eq!(bytes_copied, 0);

    // Next call should return EOF
    assert!(!reader.next().await.unwrap());
  }

  #[tokio::test]
  async fn test_extract_remaining_before_line_complete() {
    // Create a reader with partial data (no newline yet)
    let mock_reader = MockChunkedReader::new(vec![b"partial data without newline".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Try to read a line - this should buffer the data but not find a complete line
    // We need to trigger some reading first by calling next, which will return false (EOF)
    assert!(!reader.next().await.unwrap());

    // We can still extract any buffered data
    let mut remaining_buf = vec![0u8; 100];
    let bytes_copied = reader.extract_remaining(&mut remaining_buf, 0);
    assert_eq!(bytes_copied, b"partial data without newline".len());
    assert_eq!(&remaining_buf[..bytes_copied], b"partial data without newline");
  }

  #[tokio::test]
  async fn test_extract_remaining_with_max_bytes() {
    // Test the max_bytes parameter
    let mock_reader = MockChunkedReader::new(vec![b"first line\nsecond line\nthird line\nfourth line\n".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read the first line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"first line");

    // Extract only the first 15 bytes
    let mut buf = vec![0u8; 100];
    let bytes_copied = reader.extract_remaining(&mut buf, 15);
    assert_eq!(bytes_copied, 15);
    assert_eq!(&buf[..bytes_copied], b"second line\nthi");

    // Should still have remaining data
    assert!(reader.remaining_bytes_count() > 0);

    // Extract the rest
    let bytes_copied2 = reader.extract_remaining(&mut buf, 0);
    assert_eq!(&buf[..bytes_copied2], b"rd line\nfourth line\n");

    // Now should have no more data
    assert_eq!(reader.remaining_bytes_count(), 0);
  }

  #[tokio::test]
  async fn test_extract_remaining_max_bytes_larger_than_available() {
    // Test when max_bytes is larger than available data
    let mock_reader = MockChunkedReader::new(vec![b"first line\nshort".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read the first line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"first line");

    // Try to extract more than available (max_bytes = 100, but only 5 bytes available)
    let mut buf = vec![0u8; 100];
    let bytes_copied = reader.extract_remaining(&mut buf, 100);
    assert_eq!(bytes_copied, 5);
    assert_eq!(&buf[..bytes_copied], b"short");

    // Should have no remaining data
    assert_eq!(reader.remaining_bytes_count(), 0);
  }
}
