// SPDX-License-Identifier: BSD-3-Clause

use anyhow::anyhow;
use compio::buf::{IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use narwhal_protocol::ErrorReason::BadRequest;
use narwhal_util::codec_compio::{StreamReader, StreamReaderError};
use narwhal_util::pool::{BucketedPool, Pool, PoolBuffer};
use narwhal_util::string_atom::StringAtom;

use super::{ConnStreamFactory, ConnStreamReader, ConnWriter, LocalStream, ReadLineError};

impl<S: AsyncRead> AsyncRead for LocalStream<S> {
  async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
    // SAFETY: single-threaded and cooperative – only one task
    // executes at any point, so no concurrent mutable access can occur.
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

pub struct CompioStreamReader<T: AsyncRead + 'static> {
  stream_reader: StreamReader<LocalStream<T>>,
  delimiter_buf: Box<[u8; 1]>,
}

impl<T: AsyncRead + 'static> CompioStreamReader<T> {
  async fn new(reader: LocalStream<T>, message_buffer_pool: &Pool) -> Self {
    let read_pool_buffer = message_buffer_pool.acquire_buffer().await;
    let stream_reader = StreamReader::with_pool_buffer(reader, read_pool_buffer);
    Self { stream_reader, delimiter_buf: Box::new([0u8; 1]) }
  }
}

impl<T: AsyncRead + 'static> ConnStreamReader for CompioStreamReader<T> {
  async fn next(&mut self) -> Result<bool, ReadLineError> {
    self.stream_reader.next().await.map_err(|e| match e {
      StreamReaderError::MaxLineLengthExceeded => ReadLineError::MaxLineLengthExceeded,
      StreamReaderError::IoError(e) => ReadLineError::Io(e),
    })
  }

  fn line(&mut self) -> Option<&[u8]> {
    self.stream_reader.get_line()
  }

  async fn read_payload(
    &mut self,
    pool: &BucketedPool,
    len: usize,
    correlation_id: Option<u32>,
  ) -> anyhow::Result<PoolBuffer> {
    let pool_buff = pool.acquire_buffer(len).await.unwrap();
    let mut buf = self.stream_reader.read_raw(pool_buff, len).await?;

    // Take delimiter_buf for ownership-based I/O, then put it back.
    let delimiter = std::mem::replace(&mut self.delimiter_buf, Box::new([0u8; 1]));
    let delimiter = self.stream_reader.read_raw(delimiter, 1).await?;

    if delimiter[0] != b'\n' {
      self.delimiter_buf = delimiter;
      let mut error = narwhal_protocol::Error::new(BadRequest).with_detail(StringAtom::from("invalid payload format"));
      if let Some(id) = correlation_id {
        error = error.with_id(id);
      }
      return Err(error.into());
    }

    self.delimiter_buf = delimiter;
    Ok(buf.freeze(len))
  }
}

pub struct CompioConnWriter<W> {
  writer: W,
}

impl<W> CompioConnWriter<W> {
  fn new(writer: W) -> Self {
    Self { writer }
  }
}

impl<W: AsyncWrite> ConnWriter for CompioConnWriter<W> {
  async fn write_flush_batch(&mut self, batch: &mut Vec<PoolBuffer>) -> anyhow::Result<()> {
    if batch.is_empty() {
      return Ok(());
    }

    if batch.len() == 1 {
      let buf = batch.drain(..).next().unwrap();
      let compio::buf::BufResult(result, _) = self.writer.write_all(buf).await;
      result.map_err(|e| anyhow!(e))?;
    } else {
      let total_len: usize = batch.iter().map(|b| b.len()).sum();
      let mut coalesced = Vec::with_capacity(total_len);
      for buf in batch.drain(..) {
        coalesced.extend_from_slice(&buf);
      }
      let compio::buf::BufResult(result, _) = self.writer.write_all(coalesced).await;
      result.map_err(|e| anyhow!(e))?;
    }

    self.writer.flush().await?;

    Ok(())
  }

  async fn shutdown(&mut self) -> std::io::Result<()> {
    self.writer.shutdown().await
  }
}

impl<S: AsyncRead + AsyncWrite + 'static> ConnStreamFactory for S {
  type Reader = CompioStreamReader<S>;
  type Writer = CompioConnWriter<LocalStream<S>>;

  async fn create(self, pool: &Pool) -> (Self::Reader, Self::Writer) {
    let local_stream = LocalStream::new(self);
    let reader = CompioStreamReader::new(local_stream.clone(), pool).await;
    let writer = CompioConnWriter::new(local_stream);
    (reader, writer)
  }

  async fn reject(mut self, error: &[u8]) {
    let _ = self.write_all(error.to_vec()).await;
    let _ = AsyncWrite::flush(&mut self).await;
    let _ = AsyncWrite::shutdown(&mut self).await;
  }
}
