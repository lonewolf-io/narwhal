// SPDX-License-Identifier: BSD-3-Clause

use compio::buf::{IntoInner, IoBuf, IoBufMut, SetLen};
use compio::io::AsyncRead;

use crate::pool::MutablePoolBuffer;

/// Helper to get a slice view of an IoBuf's initialized data.
fn iobuf_as_slice<B: IoBuf>(buf: &B) -> &[u8] {
  buf.as_init()
}

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

impl<R: AsyncRead> StreamReader<R> {
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
      let line = &iobuf_as_slice(pool_buffer)[..pos];
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
  /// # Cancellation Safety
  ///
  /// **This method is NOT cancellation-safe.**
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
        let buffer = &iobuf_as_slice(pool_buffer)[..self.current_pos];

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
      let compio::buf::BufResult(result, returned_slice): compio::buf::BufResult<usize, _> =
        self.reader.read(read_slice).await;

      // Restore the pool buffer from the slice
      self.pool_buffer = Some(returned_slice.into_inner());

      match result {
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
  /// # Cancellation Safety
  ///
  /// **This method is NOT cancellation-safe.**
  pub async fn read_raw<B: IoBuf + IoBufMut>(&mut self, mut buf: B, len: usize) -> anyhow::Result<B> {
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
      {
        let pool_buffer = self.pool_buffer.as_ref().unwrap();
        let source_data = &iobuf_as_slice(pool_buffer)[start_pos..start_pos + to_extract];

        unsafe {
          let dst = buf.as_uninit().as_mut_ptr() as *mut u8;
          std::ptr::copy_nonoverlapping(source_data.as_ptr(), dst, to_extract);
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
      let compio::buf::BufResult(result, returned_slice): compio::buf::BufResult<usize, _> =
        self.reader.read(slice).await;
      buf = returned_slice.into_inner();

      match result {
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
  fn remaining_bytes_count(&self) -> usize {
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
  fn compact_buffer(&mut self) {
    if let Some(pos) = self.line_pos.take() {
      let pool_buffer = match self.pool_buffer.as_mut() {
        Some(pb) => pb,
        None => return,
      };

      if pos < self.current_pos {
        let remaining_len = self.current_pos - (pos + 1);
        pool_buffer.as_mut_slice().copy_within(pos + 1..self.current_pos, 0);
        unsafe {
          pool_buffer.set_len(remaining_len);
        }
        self.current_pos = remaining_len;
      } else {
        unsafe {
          pool_buffer.set_len(0);
        }
        self.current_pos = 0;
      }
    }
  }
}
