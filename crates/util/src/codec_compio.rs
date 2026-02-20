// SPDX-License-Identifier: BSD-3-Clause

use compio::buf::{IntoInner, IoBuf, IoBufMut};
use compio::io::AsyncRead;

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
      StreamReaderError::IoError(e) => write!(f, "i/o error: {}", e),
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
  pool_buffer: Option<MutablePoolBuffer>,
  current_pos: usize,
  line_pos: Option<usize>,
}

impl<R: AsyncRead + Unpin> StreamReader<R> {
  /// Creates a new LineReader with the specified async reader and pre-allocated buffer.
  ///
  /// # Arguments
  /// * `reader` - Async reader implementing AsyncRead
  /// * `pool_buffer` - Pre-allocated buffer from a pool
  pub fn with_pool_buffer(reader: R, pool_buffer: MutablePoolBuffer) -> Self {
    Self { reader, pool_buffer: Some(pool_buffer), current_pos: 0, line_pos: None }
  }

  /// Returns last read line from the buffer.
  ///
  /// # Returns
  /// * `Option<&[u8]>` - A slice of the last read line, or None if no line was read.
  pub fn get_line(&mut self) -> Option<&[u8]> {
    if let Some(pos) = self.line_pos
      && let Some(pool_buffer) = self.pool_buffer.as_ref()
    {
      let line = &pool_buffer.as_slice()[..pos];
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
  ///
  /// # Cancellation Safety
  ///
  /// ⚠️ **This method is NOT cancellation-safe.**
  ///
  /// If this future is cancelled (e.g., dropped while awaiting in a `select!` statement),
  /// the `StreamReader` will be left in an invalid state:
  ///
  /// 1. **Buffer Loss**: The internal `pool_buffer` is moved out during the read operation.
  ///    If cancelled, it becomes `None` permanently, violating struct invariants.
  ///
  /// 2. **Data Loss**: Any data successfully read by the kernel but not yet returned will be lost.
  ///    This can result in protocol corruption and stream desynchronization.
  ///
  /// 3. **State Corruption**: Internal position tracking (`current_pos`, `line_pos`) will not
  ///    reflect the actual state of the underlying stream.
  ///
  /// **Recommendation**: Do not use this method in `select!` blocks where other branches can
  /// cancel it during normal operation. If cancellation is unavoidable (e.g., shutdown),
  /// the connection should be closed entirely rather than attempting to continue reading.
  pub async fn next(&mut self) -> anyhow::Result<bool, StreamReaderError> {
    debug_assert!(self.pool_buffer.is_some(), "reading from unset pool buffer");

    // Get the buffer capacity once at the start
    let max_line_length = {
      let pool_buffer = self.pool_buffer.as_mut().unwrap();
      pool_buffer.buf_capacity()
    };

    // First, compact the buffer to remove any previously processed line.
    self.compact_buffer();

    loop {
      // Check if we have a complete line in the buffer
      {
        let pool_buffer = self.pool_buffer.as_ref().unwrap();
        let buffer = &pool_buffer.as_slice()[..self.current_pos];

        if let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
          self.line_pos = Some(pos);
          return Ok(true);
        }
      }

      if self.current_pos == max_line_length {
        return Err(StreamReaderError::MaxLineLengthExceeded);
      }

      // Take ownership of the pool buffer temporarily for the read operation
      let mut owned_pool_buffer = self.pool_buffer.take().unwrap();

      // Set buffer length to current_pos so the Slice below knows where
      // initialized data ends.
      unsafe {
        owned_pool_buffer.set_len(self.current_pos);
      }

      // Create a Slice starting at current_pos so the read writes AFTER
      // existing data.
      let read_slice = owned_pool_buffer.slice(self.current_pos..);

      // Read data into the slice
      let buf_result = self.reader.read(read_slice).await;

      // Restore the pool buffer from the slice
      self.pool_buffer = Some(buf_result.1.into_inner());

      match buf_result.0 {
        Ok(bytes_read) => {
          // Check if EOF is reached
          if bytes_read == 0 {
            return Ok(false);
          }
          // Update current_pos by adding the bytes just read
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
  /// * `buf` - Owned mutable buffer to fill with data.
  /// * `len` - The exact number of bytes to read. Must be <= `buf.buf_capacity()`.
  ///
  /// # Returns
  /// * `Ok(buf)` - Successfully read exactly `len` bytes, returning the buffer back
  /// * `Err(_)` - Contains an error if:
  ///   - `len` exceeds the buffer capacity
  ///   - The underlying reader encounters an I/O error
  ///   - EOF is reached before reading the requested number of bytes
  ///
  /// # Cancellation Safety
  ///
  /// ⚠️ **This method is NOT cancellation-safe.**
  ///
  /// Similar to `next()`, this method moves the buffer ownership to the compio I/O operation.
  /// If cancelled during the read, the `StreamReader` will be left in an invalid state with
  /// potential data loss and state corruption.
  ///
  /// See the `next()` method documentation for detailed information about cancellation safety
  /// issues and recommended usage patterns.
  pub async fn read_raw<B: IoBufMut>(&mut self, mut buf: B, len: usize) -> anyhow::Result<B> {
    assert!(len <= buf.buf_capacity(), "len exceeds buffer capacity");

    // Reset buffer length to 0 and fill up to requested len
    unsafe {
      buf.set_len(0);
    }
    let mut bytes_filled = 0;

    // First, try to fill the buffer from any remaining bytes in the stream reader
    if self.remaining_bytes_count() > 0 {
      let to_extract = std::cmp::min(self.remaining_bytes_count(), len);

      // Get the data to copy
      let start_pos = if let Some(line_pos) = self.line_pos { line_pos + 1 } else { 0 };

      // Copy data directly into the buffer's uninitialized memory
      // We need to scope the borrow to avoid conflicts later
      {
        let pool_buffer = self.pool_buffer.as_ref().unwrap();
        let source_data = &pool_buffer.as_slice()[start_pos..start_pos + to_extract];

        unsafe {
          let uninit_slice = buf.as_uninit();
          for (i, &byte) in source_data.iter().enumerate() {
            uninit_slice[i].write(byte);
          }
          buf.set_len(to_extract);
        }
      }

      bytes_filled = to_extract;

      // Update position tracking
      if let Some(line_pos) = self.line_pos {
        if line_pos + 1 + to_extract >= self.current_pos {
          self.line_pos = None;
          self.current_pos = 0;
        } else {
          self.line_pos = Some(line_pos + to_extract);
        }
      } else {
        self.current_pos -= to_extract;
        if self.current_pos > 0 {
          let pool_buffer = self.pool_buffer.as_mut().unwrap();
          pool_buffer.as_mut_slice().copy_within(to_extract..to_extract + self.current_pos, 0);
        }
      }
    }

    // If we still need more data, read directly from the underlying reader
    while bytes_filled < len {
      let slice = buf.slice(bytes_filled..len);
      let buf_result = self.reader.read(slice).await;
      buf = buf_result.1.into_inner();

      match buf_result.0 {
        Ok(0) => {
          return Err(anyhow::anyhow!("unexpected EOF while reading raw bytes"));
        },
        Ok(n) => {
          bytes_filled += n;
          if bytes_filled >= len {
            break;
          }
        },
        Err(e) => {
          return Err(e.into());
        },
      }
    }

    Ok(buf)
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
      let pool_buffer = match self.pool_buffer.as_mut() {
        Some(pb) => pb,
        None => return,
      };

      if pos < self.current_pos {
        let remaining_len = self.current_pos - (pos + 1);
        pool_buffer.as_mut_slice().copy_within(pos + 1..self.current_pos, 0);
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

  use compio::buf::{BufResult, IoBufMut};

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
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
      if self.current_chunk >= self.chunks.len() {
        return BufResult(Ok(0), buf); // EOF
      }

      let current_data = &self.chunks[self.current_chunk];
      let remaining = current_data.len() - self.position;
      let buf_capacity = buf.buf_capacity();
      let current_len = buf.buf_len();
      let available_space = buf_capacity - current_len;
      let to_copy = std::cmp::min(available_space, remaining);

      if to_copy > 0 {
        // Write to the uninitialized portion of the buffer
        let uninit_slice = &mut buf.as_uninit()[current_len..current_len + to_copy];
        for (i, &byte) in current_data[self.position..self.position + to_copy].iter().enumerate() {
          uninit_slice[i].write(byte);
        }

        unsafe {
          buf.set_len(current_len + to_copy);
        }
      }

      let current_data_len = current_data.len();
      self.position += to_copy;
      if self.position >= current_data_len {
        self.current_chunk += 1;
        self.position = 0;
      }

      BufResult(Ok(to_copy), buf)
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
  async fn test_read_raw_with_buffered_data() {
    // Test reading raw bytes when there's buffered data after a line read
    let mock_reader = MockChunkedReader::new(vec![b"first line\nRAW_DATA_HERE".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read the first line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"first line");

    // Now read raw bytes - should use the buffered data
    let buf = vec![0u8; 13];
    let result = reader.read_raw(buf, 13).await.unwrap();
    assert_eq!(&result[..], b"RAW_DATA_HERE");
  }

  #[tokio::test]
  async fn test_read_raw_direct_read() {
    // Test reading raw bytes when buffer is empty (direct read from reader)
    let mock_reader = MockChunkedReader::new(vec![b"DIRECT_RAW_DATA".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read raw bytes directly without reading a line first
    let buf = vec![0u8; 15];
    let result = reader.read_raw(buf, 15).await.unwrap();
    assert_eq!(&result[..], b"DIRECT_RAW_DATA");
  }

  #[tokio::test]
  async fn test_read_raw_spanning_chunks() {
    // Test reading raw bytes that span across buffered data and multiple reads
    let mock_reader = MockChunkedReader::new(vec![b"line\nBUFFERED".to_vec(), b"_THEN_".to_vec(), b"READ".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read the line first
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"line");

    // Read raw bytes that will use buffered data and then read more
    let buf = vec![0u8; 18];
    let result = reader.read_raw(buf, 18).await.unwrap();
    assert_eq!(&result[..], b"BUFFERED_THEN_READ");
  }

  #[tokio::test]
  async fn test_read_raw_eof() {
    // Test reading raw bytes when there's not enough data (EOF)
    let mock_reader = MockChunkedReader::new(vec![b"short".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Try to read more bytes than available
    let buf = vec![0u8; 100];
    let result = reader.read_raw(buf, 100).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("EOF"));
  }

  #[tokio::test]
  async fn test_read_raw_partial_buffered() {
    // Test reading raw bytes where some data is buffered and some needs to be read
    let mock_reader = MockChunkedReader::new(vec![b"first\nsecond\nthird".to_vec(), b"_part".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Read first line
    assert!(reader.next().await.unwrap());
    assert_eq!(reader.get_line().unwrap(), b"first");

    // Read 15 bytes raw - "second\nthird" (13 bytes) is buffered, need 2 more from next chunk
    let buf = vec![0u8; 15];
    let result = reader.read_raw(buf, 15).await.unwrap();
    assert_eq!(&result[..], b"second\nthird_pa");
  }

  #[tokio::test]
  async fn test_read_raw_respects_len_not_capacity() {
    // Provide a single chunk with more data than we'll request
    let mock_reader = MockChunkedReader::new(vec![b"ABCDextra".to_vec()]);
    let pool = Pool::new(1, 1024);
    let pool_buffer = pool.acquire_buffer().await;

    let mut reader = StreamReader::<MockChunkedReader>::with_pool_buffer(mock_reader, pool_buffer);

    // Create a buffer with capacity 9, but only request 4 bytes.
    let buf = Vec::with_capacity(9);
    let result = reader.read_raw(buf, 4).await.unwrap();

    assert_eq!(result.len(), 4, "read_raw should return exactly `len` bytes, not more");
    assert_eq!(&result[..], b"ABCD");
  }
}
