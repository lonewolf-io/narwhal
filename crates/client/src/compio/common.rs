// SPDX-License-Identifier: BSD-3-Clause

use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::io::Cursor;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicU32};
use std::time::Duration;

use anyhow::anyhow;
use async_channel::{Receiver, Sender, bounded};
use async_lock::{Mutex, Semaphore};
use compio::buf::{IntoInner, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::FutureExt;
use futures_channel::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

use narwhal_common::service::Service;
use narwhal_protocol::{Message, PongParameters, deserialize, serialize};
use narwhal_util::codec_compio::StreamReader;
use narwhal_util::pool::{MutablePoolBuffer, Pool, PoolBuffer};

use super::dialer::Dialer;
use crate::config::{Config, SessionInfo};
use crate::conn_state::{ErrorState, INBOUND_QUEUE_SIZE, OUTBOUND_QUEUE_SIZE, PendingRequest, PendingRequests};
use crate::object_pool;

/// A lock-free shared stream wrapper for a single-threaded io_uring runtime.
///
/// Multiple clones of a `LocalStream` share the same underlying stream via
/// `Rc<UnsafeCell<S>>`.
///
/// # Safety
///
/// Only safe within a **single-threaded**, cooperative async runtime like
/// compio.  Because only one task ever executes at a time, the two halves
/// (reader / writer) never actually access the inner stream concurrently.
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

/// A trait for performing the protocol handshake over a compio stream.
///
/// The trait mirrors [`crate::tokio::common::Handshaker`] but operates
/// on compio `AsyncRead + AsyncWrite` streams.
#[async_trait::async_trait(?Send)]
pub trait Handshaker<S>: Clone + Send + Sync + 'static
where
  S: AsyncRead + AsyncWrite + 'static,
{
  /// Extra metadata returned alongside [`SessionInfo`].
  type SessionExtraInfo: Clone + Send + Sync;

  /// Performs a handshake over the given stream, returning negotiated
  /// session parameters and any extra implementation-specific data.
  async fn handshake(&self, stream: &mut S) -> anyhow::Result<(SessionInfo, Self::SessionExtraInfo)>;
}

/// Sends a request `message` over a compio stream and reads the response.
///
/// This is the compio-based counterpart of [`narwhal_protocol::request`]
/// (which targets `futures::io`).
pub(crate) async fn request<S>(
  message: Message,
  stream: &mut S,
  mut pool_buffer: MutablePoolBuffer,
) -> anyhow::Result<Message>
where
  S: AsyncRead + AsyncWrite + 'static,
{
  // Serialize the request into the pool buffer.
  let n = serialize(&message, pool_buffer.as_mut_slice()).map_err(|e| anyhow!("failed to serialize message: {}", e))?;

  // Write to the stream.
  let data = pool_buffer.as_slice()[..n].to_vec();
  let compio::buf::BufResult(result, _) = stream.write_all(data).await;
  result?;
  stream.flush().await?;

  // Read the response.
  let mut reader = StreamReader::with_pool_buffer(&mut *stream, pool_buffer);
  let has_line = reader.next().await.map_err(|e| anyhow!("failed to read response: {}", e))?;
  if !has_line {
    return Err(anyhow!("connection closed while reading response"));
  }

  match reader.get_line() {
    Some(line_bytes) => Ok(deserialize(Cursor::new(line_bytes))?),
    None => Err(anyhow!("failed to read response from server")),
  }
}

/// Result type for a server response: the decoded message and an optional payload buffer.
type ResponseResult = anyhow::Result<(Message, Option<PoolBuffer>)>;

/// A handle returned by [`Client::send_message`] that resolves to the
/// server's response.
///
/// The handle is `Send` and may be awaited on any thread.  If dropped
/// without being awaited the pending-request entry is cleaned up
/// automatically.
pub struct ResponseHandle {
  rx: Option<oneshot::Receiver<ResponseResult>>,
  pending_requests: PendingRequests,
  correlation_id: u32,
  timeout: Duration,
}

impl ResponseHandle {
  /// Awaits the server response, subject to the configured request timeout.
  pub async fn response(mut self) -> anyhow::Result<(Message, Option<PoolBuffer>)> {
    let rx = self.rx.take().expect("response() called twice");

    match compio::runtime::time::timeout(self.timeout, rx).await {
      Ok(Ok(result)) => result,
      Ok(Err(_)) => Err(anyhow!("response channel closed before receiving a response")),
      Err(_) => Err(anyhow!("request timed out")),
    }
  }
}

impl Drop for ResponseHandle {
  fn drop(&mut self) {
    // Ensure the pending-request entry (and its semaphore permit) is always
    // freed, whether or not the handle was awaited.
    self.pending_requests.remove(&self.correlation_id);
  }
}

/// A Narwhal client that uses compio-based I/O.
///
/// Modes are determined automatically by the [`Config::max_idle_connections`]
/// value:
/// - `1` → single persistent connection (lazily created).
/// - `> 1` → connection pool backed by [`crate::object_pool::ObjectPool`].
///
/// The client is `Send + Sync` and cheaply cloneable.
#[derive(Debug)]
pub struct Client<S, HS, ST>(Arc<Mutex<ClientInner<S, HS, ST>>>)
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service;

impl<S, HS, ST> Clone for Client<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn clone(&self) -> Self {
    Self(Arc::clone(&self.0))
  }
}

impl<S, HS, ST> Client<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  /// Creates a new `Client`.
  ///
  /// # Arguments
  ///
  /// * `client_id` — human-readable identifier (for logs).
  /// * `config` — connection and retry settings.
  /// * `dialer` — factory for new transport streams.
  /// * `handshaker` — performs the protocol handshake on each new stream.
  pub fn new(
    client_id: impl Into<String>,
    config: Config,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
  ) -> anyhow::Result<Self> {
    let inner = if config.max_idle_connections == 1 {
      ClientInner::new(client_id, config, dialer, handshaker)?
    } else {
      ClientInner::new_pooled(client_id, config, dialer, handshaker)?
    };
    Ok(Self(Arc::new(Mutex::new(inner))))
  }

  /// Sends a message and returns a [`ResponseHandle`] that the caller awaits
  /// to obtain the server's reply.
  ///
  /// The internal mutex is released *before* the potentially-slow semaphore
  /// acquisition and channel send, so other threads are not blocked.
  pub async fn send_message(
    &self,
    message: Message,
    payload_opt: Option<PoolBuffer>,
  ) -> anyhow::Result<ResponseHandle> {
    let mut inner = self.0.lock().await;
    if inner.config.max_idle_connections == 1 {
      let conn = inner.get_or_create_connection().await?;
      drop(inner);
      conn.send_message(message, payload_opt).await
    } else {
      let conn = inner.get_connection().await?;
      drop(inner);
      conn.send_message(message, payload_opt).await
    }
  }

  /// Returns the session info negotiated during the handshake.
  pub async fn session_info(&self) -> anyhow::Result<(SessionInfo, HS::SessionExtraInfo)> {
    let mut inner = self.0.lock().await;
    if inner.config.max_idle_connections == 1 {
      let conn = inner.get_or_create_connection().await?;
      Ok(conn.session_info.as_ref().unwrap().clone())
    } else {
      let conn = inner.get_connection().await?;
      Ok(conn.session_info.as_ref().unwrap().clone())
    }
  }

  /// Gracefully shuts down the client.
  pub async fn shutdown(&self) -> anyhow::Result<()> {
    let mut inner = self.0.lock().await;
    inner.shutdown().await
  }

  /// Returns the next monotonically-increasing correlation ID.
  pub async fn next_id(&self) -> u32 {
    let inner = self.0.lock().await;
    inner.next_id()
  }

  /// Returns a stream of unsolicited inbound messages from the server.
  ///
  /// **May only be called once** per `Client` instance.
  ///
  /// # Panics
  ///
  /// Panics if called more than once.
  pub async fn inbound_stream(&self) -> Receiver<(Message, Option<PoolBuffer>)> {
    let mut inner = self.0.lock().await;
    inner.inbound_rx.take().expect("inbound_stream can only be called once")
  }
}

struct ClientInner<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  client_id: Arc<String>,
  config: Arc<Config>,
  next_id: AtomicU32,
  next_conn_id: AtomicU32,
  dialer: Arc<dyn Dialer<Stream = S>>,
  handshaker: HS,
  inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
  inbound_rx: Option<Receiver<(Message, Option<PoolBuffer>)>>,
  conn: Option<Arc<ClientConn<S, HS, ST>>>,
  conn_pool: Option<object_pool::ObjectPool<ClientConnManager<S, HS, ST>>>,
  _service_type: PhantomData<ST>,
}

impl<S, HS, ST> fmt::Debug for ClientInner<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ClientInner").field("client_id", &self.client_id).field("config", &self.config).finish()
  }
}

impl<S, HS, ST> ClientInner<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  /// Single-connection mode constructor.
  fn new(
    client_id: impl Into<String>,
    config: Config,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
  ) -> anyhow::Result<Self> {
    let (inbound_tx, inbound_rx) = bounded(INBOUND_QUEUE_SIZE);
    Ok(Self {
      client_id: Arc::new(client_id.into()),
      config: Arc::new(config),
      next_id: AtomicU32::new(1),
      next_conn_id: AtomicU32::new(1),
      dialer,
      handshaker,
      inbound_tx,
      inbound_rx: Some(inbound_rx),
      conn: None,
      conn_pool: None,
      _service_type: PhantomData,
    })
  }

  /// Pooled-connection mode constructor.
  fn new_pooled(
    client_id: impl Into<String>,
    config: Config,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
  ) -> anyhow::Result<Self> {
    let max_idle = config.max_idle_connections;
    let arc_client_id = Arc::new(client_id.into());
    let arc_config = Arc::new(config);
    let (inbound_tx, inbound_rx) = bounded(INBOUND_QUEUE_SIZE);

    let conn_pool = object_pool::ObjectPool::builder(ClientConnManager::new(
      arc_client_id.clone(),
      arc_config.clone(),
      dialer.clone(),
      handshaker.clone(),
      inbound_tx.clone(),
    ))
    .max_size(max_idle)
    .build()
    .map_err(|e| anyhow!("failed to build connection pool: {}", e))?;

    Ok(Self {
      client_id: arc_client_id,
      config: arc_config,
      next_id: AtomicU32::new(1),
      next_conn_id: AtomicU32::new(1),
      dialer,
      handshaker,
      inbound_tx,
      inbound_rx: Some(inbound_rx),
      conn: None,
      conn_pool: Some(conn_pool),
      _service_type: PhantomData,
    })
  }

  async fn get_or_create_connection(&mut self) -> anyhow::Result<Arc<ClientConn<S, HS, ST>>> {
    debug_assert!(
      self.config.max_idle_connections == 1,
      "get_or_create_connection should only be called in single connection mode"
    );

    // Fast path: reuse existing healthy connection.
    if let Some(conn) = &self.conn
      && !conn.is_unhealthy()
    {
      return Ok(conn.clone());
    }

    // Slow path: create with exponential backoff.
    let conn = retry_with_backoff(
      self.config.backoff_initial_delay,
      self.config.backoff_max_delay,
      self.config.backoff_max_retries,
      || self.create_connection(),
    )
    .await?;

    let conn = Arc::new(conn);
    self.conn = Some(conn.clone());
    Ok(conn)
  }

  async fn get_connection(&self) -> anyhow::Result<object_pool::ObjectPoolObject<ClientConnManager<S, HS, ST>>> {
    debug_assert!(self.config.max_idle_connections > 1, "get_connection should only be called in pooled mode");

    let conn_pool = self.conn_pool.as_ref().expect("conn_pool must be set for pooled mode");

    retry_with_backoff(
      self.config.backoff_initial_delay,
      self.config.backoff_max_delay,
      self.config.backoff_max_retries,
      || self.get_connection_from_pool(conn_pool),
    )
    .await
  }

  async fn get_connection_from_pool(
    &self,
    conn_pool: &object_pool::ObjectPool<ClientConnManager<S, HS, ST>>,
  ) -> anyhow::Result<object_pool::ObjectPoolObject<ClientConnManager<S, HS, ST>>> {
    let get_pool_conn = || async { conn_pool.get().await.map_err(|e| anyhow!("{}", e)) };

    match get_pool_conn().await {
      Ok(conn) => {
        if !conn.is_unhealthy() {
          return Ok(conn);
        }
        conn_pool.retain(|c| !c.is_unhealthy());
        get_pool_conn().await
      },
      Err(e) => Err(e),
    }
  }

  async fn create_connection(&self) -> anyhow::Result<ClientConn<S, HS, ST>> {
    let conn_id = self.next_conn_id.fetch_add(1, atomic::Ordering::SeqCst);

    let mut conn = ClientConn::<S, HS, ST>::new(
      self.client_id.clone(),
      conn_id,
      self.config.clone(),
      self.dialer.clone(),
      self.handshaker.clone(),
      self.inbound_tx.clone(),
    );

    conn.connect().await?;
    Ok(conn)
  }

  async fn shutdown(&mut self) -> anyhow::Result<()> {
    if self.config.max_idle_connections == 1 {
      if let Some(conn) = &self.conn {
        conn.shutdown().await?;
      }
    } else if let Some(pool) = &self.conn_pool {
      pool.retain(|_| false);
      pool.close();
    }
    Ok(())
  }

  fn next_id(&self) -> u32 {
    self
      .next_id
      .fetch_update(atomic::Ordering::SeqCst, atomic::Ordering::SeqCst, |cur| {
        let mut next = cur.wrapping_add(1);
        if next == 0 {
          next = 1;
        }
        Some(next)
      })
      .unwrap()
  }
}

struct ClientConnManager<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  client_id: Arc<String>,
  config: Arc<Config>,
  next_conn_id: AtomicU32,
  dialer: Arc<dyn Dialer<Stream = S>>,
  handshaker: HS,
  inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
  _service_type: PhantomData<ST>,
}

impl<S, HS, ST> ClientConnManager<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn new(
    client_id: Arc<String>,
    config: Arc<Config>,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
    inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
  ) -> Self {
    Self {
      client_id,
      config,
      next_conn_id: AtomicU32::new(1),
      dialer,
      handshaker,
      inbound_tx,
      _service_type: PhantomData,
    }
  }
}

impl<S, HS, ST> object_pool::Manager for ClientConnManager<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  type Type = ClientConn<S, HS, ST>;
  type Error = anyhow::Error;

  async fn create(&self) -> Result<Self::Type, Self::Error> {
    let conn_id = self.next_conn_id.fetch_add(1, atomic::Ordering::SeqCst);

    let mut conn = ClientConn::<S, HS, ST>::new(
      self.client_id.clone(),
      conn_id,
      self.config.clone(),
      self.dialer.clone(),
      self.handshaker.clone(),
      self.inbound_tx.clone(),
    );

    conn.connect().await?;
    Ok(conn)
  }

  async fn recycle(&self, conn: &mut Self::Type) -> object_pool::RecycleResult<Self::Error> {
    if conn.is_unhealthy() {
      conn.shutdown().await.ok();
      return Err(object_pool::RecycleError::Backend(
        conn.error_state.take_error().unwrap_or_else(|| anyhow!("connection is unhealthy")),
      ));
    }
    Ok(())
  }

  fn detach(&self, conn: &mut Self::Type) {
    // Signal cancellation.
    conn.shutdown_token.cancel();

    debug!(
      client_id = conn.client_id.as_str(),
      connection_id = conn.conn_id,
      service_type = ST::NAME,
      "detached client connection"
    );
  }
}

/// A single connection to a Narwhal server, driven by compio.
///
/// The struct itself is `Send` (it holds only `Arc`, channels, tokens, ...).
/// The actual I/O tasks (reader + writer) live on the compio event loop of
/// the thread that called [`connect`].
struct ClientConn<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  client_id: Arc<String>,
  conn_id: u32,
  config: Arc<Config>,
  dialer: Arc<dyn Dialer<Stream = S>>,
  handshaker: HS,
  inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
  session_info: Option<(SessionInfo, HS::SessionExtraInfo)>,
  inflight_requests_sem: Option<Arc<Semaphore>>,
  pending_requests: PendingRequests,
  writer_tx: Option<Sender<(Message, Option<PoolBuffer>)>>,
  shutdown_token: CancellationToken,
  error_state: ErrorState,
  _service_type: PhantomData<ST>,
}

// SAFETY: every field is Send.  The !Send I/O state (LocalStream, compio
// tasks) lives exclusively inside spawned compio tasks and is never stored
// in this struct.
unsafe impl<S, HS, ST> Send for ClientConn<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
}

impl<S, HS, ST> ClientConn<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn new(
    client_id: Arc<String>,
    conn_id: u32,
    config: Arc<Config>,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
    inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
  ) -> Self {
    Self {
      client_id,
      conn_id,
      config,
      dialer,
      handshaker,
      inbound_tx,
      session_info: None,
      inflight_requests_sem: None,
      pending_requests: PendingRequests::new(),
      writer_tx: None,
      shutdown_token: CancellationToken::new(),
      error_state: ErrorState::new(),
      _service_type: PhantomData,
    }
  }

  /// Establishes the connection: dials, handshakes, then spawns reader and
  /// writer tasks on the **current** compio event loop.
  ///
  /// Must be called from within a compio runtime.
  async fn connect(&mut self) -> anyhow::Result<()> {
    let mut stream = self.dialer.dial().await?;

    let (session_info, session_extra_info) = self.handshaker.handshake(&mut stream).await?;

    debug!(
      client_id = self.client_id.as_str(),
      conn_id = self.conn_id,
      heartbeat_interval = session_info.heartbeat_interval,
      max_inflight_requests = session_info.max_inflight_requests,
      max_message_size = session_info.max_message_size,
      max_payload_size = session_info.max_payload_size,
      service_type = ST::NAME,
      "client handshake completed",
    );

    // Configure semaphore
    self.inflight_requests_sem = Some(Arc::new(Semaphore::new(session_info.max_inflight_requests as usize)));

    // Buffer pools
    let message_pool = Pool::new(2 * self.config.max_idle_connections, session_info.max_message_size as usize);
    let payload_buffer_count = OUTBOUND_QUEUE_SIZE + INBOUND_QUEUE_SIZE + (2 * self.config.max_idle_connections);
    let payload_pool = Pool::new(payload_buffer_count, session_info.max_payload_size as usize);

    // Share the stream between reader and writer via LocalStream (Rc).
    let shared_stream = LocalStream::new(stream);
    let reader_stream = shared_stream.clone();
    let writer_stream = shared_stream;

    // Spawn writer task
    let (writer_tx, writer_rx) = bounded::<(Message, Option<PoolBuffer>)>(OUTBOUND_QUEUE_SIZE);

    let writer_client_id = self.client_id.clone();
    let writer_conn_id = self.conn_id;
    let writer_error_state = self.error_state.clone();
    let writer_shutdown = self.shutdown_token.clone();
    let writer_msg_buf = message_pool.acquire_buffer().await;

    compio::runtime::spawn(Self::writer_task(
      writer_client_id,
      writer_conn_id,
      writer_stream,
      writer_rx,
      writer_msg_buf,
      writer_error_state,
      writer_shutdown,
    ))
    .detach();

    self.writer_tx = Some(writer_tx.clone());

    // Spawn reader task
    let reader_msg_buf = message_pool.acquire_buffer().await;

    compio::runtime::spawn(Self::reader_task(
      self.client_id.clone(),
      self.conn_id,
      reader_stream,
      reader_msg_buf,
      payload_pool,
      self.pending_requests.clone(),
      self.error_state.clone(),
      writer_tx,
      self.inbound_tx.clone(),
      self.shutdown_token.clone(),
      self.config.payload_read_timeout,
    ))
    .detach();

    // Store session info
    self.session_info = Some((session_info, session_extra_info));

    Ok(())
  }

  /// Signals cancellation — the reader / writer tasks will observe it and
  /// exit on their own.
  async fn shutdown(&self) -> anyhow::Result<()> {
    self.shutdown_token.cancel();
    Ok(())
  }

  /// Initiates a request and returns a [`ResponseHandle`].
  async fn send_message(&self, message: Message, payload_opt: Option<PoolBuffer>) -> anyhow::Result<ResponseHandle> {
    let correlation_id =
      message.correlation_id().ok_or_else(|| anyhow!("message must have a correlation identifier"))?;

    let sem = self.inflight_requests_sem.as_ref().expect("inflight_requests_sem must be set after connect");
    let permit = sem.acquire_arc().await;

    let (resp_tx, resp_rx) = oneshot::channel();

    self.pending_requests.insert(correlation_id, PendingRequest { sender: Some(resp_tx), _permit: permit });

    if let Err(e) = self.writer_tx.as_ref().unwrap().send((message, payload_opt)).await {
      self.pending_requests.remove(&correlation_id);
      return Err(anyhow!("failed to send message: {}", e));
    }

    Ok(ResponseHandle {
      rx: Some(resp_rx),
      pending_requests: self.pending_requests.clone(),
      correlation_id,
      timeout: self.config.timeout,
    })
  }

  #[inline]
  fn is_unhealthy(&self) -> bool {
    self.error_state.has_error()
  }

  async fn writer_task(
    client_id: Arc<String>,
    conn_id: u32,
    mut writer: LocalStream<S>,
    rx: Receiver<(Message, Option<PoolBuffer>)>,
    mut message_buff: MutablePoolBuffer,
    error_state: ErrorState,
    shutdown_token: CancellationToken,
  ) {
    loop {
      if shutdown_token.is_cancelled() {
        break;
      }

      let mut msg_res = std::pin::pin!(rx.recv().fuse());
      let mut cancelled = std::pin::pin!(shutdown_token.cancelled().fuse());

      futures::select! {
        msg_res = msg_res => {
          match msg_res {
            Ok((msg, payload_opt)) => {
              if let Err(e) = Self::write_message(&msg, payload_opt, &mut writer, message_buff.as_mut_slice()).await {
                error!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, "{}", e);
                error_state.set_error(e);
                break;
              }
            },
            Err(_) => {
              // All senders dropped — exit.
              break;
            },
          }
        },
        _ = cancelled => {
          break;
        },
      }
    }

    if let Err(e) = writer.shutdown().await {
      warn!(
        client_id = client_id.as_str(),
        connection_id = conn_id,
        service_type = ST::NAME,
        "failed to shutdown client stream: {}",
        e
      );
    }
  }

  #[allow(clippy::too_many_arguments)]
  async fn reader_task(
    client_id: Arc<String>,
    conn_id: u32,
    reader: LocalStream<S>,
    read_buffer: MutablePoolBuffer,
    payload_buffer_pool: Pool,
    pending_requests: PendingRequests,
    error_state: ErrorState,
    writer_tx: Sender<(Message, Option<PoolBuffer>)>,
    inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
    shutdown_token: CancellationToken,
    payload_read_timeout: Duration,
  ) {
    let mut stream_reader = StreamReader::with_pool_buffer(reader, read_buffer);

    loop {
      if shutdown_token.is_cancelled() {
        break;
      }

      let mut cancelled = std::pin::pin!(shutdown_token.cancelled().fuse());

      futures::select! {
        res = stream_reader.next().fuse() => {
          match res {
            Ok(true) => {
              let line_bytes = stream_reader.get_line().unwrap();

              match deserialize(Cursor::new(line_bytes)) {
                Ok(msg) => {
                  // Read optional payload.
                  let res = match Self::read_message_payload(
                    &msg,
                    &mut stream_reader,
                    &payload_buffer_pool,
                    payload_read_timeout,
                  )
                  .await
                  {
                    Ok(payload_opt) => Ok((msg.clone(), payload_opt)),
                    Err(e) => Err(anyhow!(e)),
                  };

                  if let Some(correlation_id) = msg.correlation_id() {
                    match msg {
                      Message::Ping(_) => {
                        trace!(
                          client_id = client_id.as_str(),
                          connection_id = conn_id,
                          correlation_id,
                          service_type = ST::NAME,
                          "received ping message"
                        );
                        if writer_tx
                          .send((Message::Pong(PongParameters { id: correlation_id }), None))
                          .await
                          .is_ok()
                        {
                          trace!(
                            client_id = client_id.as_str(),
                            connection_id = conn_id,
                            correlation_id,
                            service_type = ST::NAME,
                            "sent pong message"
                          );
                        }
                        continue;
                      },
                      _ => {
                        if let Some(sender) = pending_requests.take_response_sender(correlation_id) {
                          if sender.send(res).is_err() {
                            warn!(
                              client_id = client_id.as_str(),
                              connection_id = conn_id,
                              correlation_id,
                              service_type = ST::NAME,
                              "failed to send client response: receiver dropped"
                            );
                          }
                        } else {
                          warn!(
                            client_id = client_id.as_str(),
                            connection_id = conn_id,
                            correlation_id,
                            service_type = ST::NAME,
                            "unexpected response"
                          );
                        }
                      },
                    }
                  } else {
                    // Unsolicited inbound message — no correlation id.
                    match res {
                      Ok((msg, payload_opt)) => {
                        if let Err(e) = inbound_tx.try_send((msg, payload_opt)) {
                          warn!(
                            client_id = client_id.as_str(),
                            connection_id = conn_id,
                            service_type = ST::NAME,
                            error = ?e,
                            "dropped inbound message: channel full or closed"
                          );
                        }
                      },
                      Err(e) => {
                        warn!(
                          client_id = client_id.as_str(),
                          connection_id = conn_id,
                          service_type = ST::NAME,
                          error = ?e,
                          "failed to read inbound message payload"
                        );
                      },
                    }
                  }
                },
                Err(e) => {
                  warn!(
                    client_id = client_id.as_str(),
                    connection_id = conn_id,
                    service_type = ST::NAME,
                    "{}",
                    e
                  );
                  error_state.set_error(e);
                  break;
                },
              }
            },

            Ok(false) => {
              debug!(
                client_id = client_id.as_str(),
                connection_id = conn_id,
                service_type = ST::NAME,
                "connection closed by peer"
              );
              error_state.set_error(anyhow!("connection closed by peer"));
              break;
            },

            Err(e) => {
              warn!(
                client_id = client_id.as_str(),
                connection_id = conn_id,
                service_type = ST::NAME,
                "{}",
                e
              );
              error_state.set_error(e.into());
              break;
            },
          }
        },

        _ = cancelled => {
          break;
        },
      }
    }
  }

  async fn write_message(
    message: &Message,
    payload_opt: Option<PoolBuffer>,
    writer: &mut LocalStream<S>,
    write_buffer: &mut [u8],
  ) -> anyhow::Result<()> {
    let n = serialize(message, write_buffer).map_err(|e| anyhow!("failed to serialize message: {}", e))?;

    let data = write_buffer[..n].to_vec();
    let compio::buf::BufResult(result, _) = writer.write_all(data).await;
    result?;

    if let Some(payload) = payload_opt {
      let compio::buf::BufResult(result, _) = writer.write_all(payload).await;
      result?;

      let compio::buf::BufResult(result, _) = writer.write_all(vec![b'\n']).await;
      result?;
    }

    writer.flush().await?;
    Ok(())
  }

  async fn read_message_payload(
    msg: &Message,
    stream_reader: &mut StreamReader<LocalStream<S>>,
    payload_pool: &Pool,
    payload_read_timeout: Duration,
  ) -> anyhow::Result<Option<PoolBuffer>> {
    match msg.payload_info() {
      Some(payload_info) => {
        let pool_buff = payload_pool.acquire_buffer().await;

        match compio::runtime::time::timeout(
          payload_read_timeout,
          Self::read_payload(stream_reader, pool_buff, payload_info.length),
        )
        .await
        {
          Ok(result) => result.map(Some),
          Err(_) => Err(anyhow!("payload read timed out")),
        }
      },
      None => Ok(None),
    }
  }

  async fn read_payload(
    stream_reader: &mut StreamReader<LocalStream<S>>,
    pool_buff: MutablePoolBuffer,
    payload_length: usize,
  ) -> anyhow::Result<PoolBuffer> {
    // Read exactly `payload_length` bytes into a sub-slice of the pool buffer.
    let payload_slice = pool_buff.slice(0..payload_length);
    let payload_slice = stream_reader
      .read_raw(payload_slice, payload_length)
      .await
      .map_err(|e| anyhow!("failed to read payload: {}", e))?;
    let mut pool_buff = payload_slice.into_inner();

    // Read and verify the trailing newline delimiter.
    let cr = vec![0u8; 1];
    let cr = stream_reader.read_raw(cr, 1).await.map_err(|e| anyhow!("failed to read payload delimiter: {}", e))?;
    if cr[0] != b'\n' {
      return Err(anyhow!("invalid payload format: expected newline delimiter"));
    }

    Ok(pool_buff.freeze(payload_length))
  }
}

/// Simple exponential-backoff helper using `compio::runtime::time::sleep`.
///
/// This replaces [`narwhal_util::backoff::ExponentialBackoff`] which is
/// hard-wired to `tokio::time::sleep`.
async fn retry_with_backoff<F, Fut, T>(
  initial_delay: Duration,
  max_delay: Duration,
  max_retries: usize,
  mut operation: F,
) -> anyhow::Result<T>
where
  F: FnMut() -> Fut,
  Fut: Future<Output = anyhow::Result<T>>,
{
  let mut delay = initial_delay;
  let mut last_error: Option<anyhow::Error> = None;

  for _ in 0..max_retries {
    match operation().await {
      Ok(result) => return Ok(result),
      Err(e) => {
        last_error = Some(e);
        compio::runtime::time::sleep(delay).await;
        let next = Duration::from_secs_f64(delay.as_secs_f64() * 1.5);
        delay = std::cmp::min(next, max_delay);
      },
    }
  }

  Err(last_error.unwrap_or_else(|| anyhow!("retry_with_backoff: no attempts were made")))
}
