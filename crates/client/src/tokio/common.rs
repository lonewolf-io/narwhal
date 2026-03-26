// SPDX-License-Identifier: BSD-3-Clause

use std::fmt;
use std::io::Cursor;
use std::marker::PhantomData;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, atomic};
use std::time::Duration;

use ::tokio::task::JoinHandle;
use anyhow::anyhow;
use async_channel::{Receiver, Sender, bounded};
use async_lock::{Mutex, Semaphore};
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures_channel::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, trace, warn};

use super::dialer::Dialer;
use crate::config::{Config, SessionInfo};
use crate::conn_state::{ErrorState, INBOUND_QUEUE_SIZE, OUTBOUND_QUEUE_SIZE, PendingRequest, PendingRequests};
use crate::object_pool;
use narwhal_protocol::{Message, PongParameters, deserialize, serialize};
use narwhal_util::backoff::ExponentialBackoff;
use narwhal_util::codec_tokio::StreamReader;
use narwhal_util::pool::{MutablePoolBuffer, Pool, PoolBuffer};

use narwhal_common::service::Service;

/// A trait representing the logic required to perform a handshake with the Narwhal server.
///
/// The `Handshaker` is responsible for negotiating session parameters over an established stream
/// (TCP or Unix domain socket). This trait allows customization of the handshake logic while
/// remaining agnostic to the transport layer.
///
/// # Associated Types
///
/// * `SessionExtraInfo`: Extra metadata returned by the handshake, which can be used by the client
///   for additional context (e.g., authentication tokens, feature flags, etc.).
#[async_trait::async_trait]
pub trait Handshaker<S>: Clone + Send + Sync + 'static
where
  S: AsyncRead + AsyncWrite,
{
  /// Extra data returned as part of the handshake response, in addition to `SessionInfo`.
  type SessionExtraInfo: Clone + Send + Sync;

  /// Performs a handshake over the provided stream.
  ///
  /// This is typically the first step after establishing a physical connection to the server.
  ///
  /// # Arguments
  ///
  /// * `stream` - A mutable reference to the underlying transport stream used for communication.
  ///
  /// # Returns
  ///
  /// On success, returns a tuple containing:
  /// - `SessionInfo`: Core session parameters negotiated with the server.
  /// - `Self::SessionExtraInfo`: Additional data specific to the implementation.
  ///
  /// # Errors
  ///
  /// Returns an error if the handshake fails due to protocol mismatch, authentication failure,
  /// server error, etc.
  async fn handshake(&self, stream: &mut S) -> anyhow::Result<(SessionInfo, Self::SessionExtraInfo)>;
}

/// An Narwhal client that supports both single connection and connection pooling modes.
///
/// The mode is automatically determined by the `max_idle_connections` configuration:
/// - When `max_idle_connections == 1`: Uses a single persistent connection
/// - When `max_idle_connections > 1`: Uses connection pooling
///
/// Connections are created lazily on first use and automatically reconnect
/// on failure with exponential backoff.
#[derive(Debug)]
pub struct Client<S, HS, ST>(Arc<Mutex<ClientInner<S, HS, ST>>>)
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service;

impl<S, HS, ST> Clone for Client<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn clone(&self) -> Self {
    Self(Arc::clone(&self.0))
  }
}

// === impl Client ===

impl<S, HS, ST> Client<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  /// Creates a new `Client` instance.
  ///
  /// The client automatically uses a single persistent connection when `max_idle_connections` is 1,
  /// or connection pooling when `max_idle_connections` is greater than 1.
  ///
  /// # Arguments
  ///
  /// * `client_id` - A string-like identifier for the client.
  /// * `config` - Configuration settings for the client.
  /// * `dialer` - A dialer for establishing connections.
  /// * `handshaker` - An implementation of the `Handshaker` trait to perform the handshake.
  ///
  /// # Errors
  ///
  /// Returns an error if the client cannot be initialized.
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

  /// Sends a message to the server and returns a `JoinHandle` to await the response.
  ///
  /// # Arguments
  ///
  /// * `message` - A `Message` instance that must contain a correlation ID.
  /// * `payload_opt` - An optional `PoolBuffer` containing payload data to be sent
  ///   along with the message. Use `None` for messages without payload data.
  ///
  /// # Returns
  ///
  /// A handle to a task that resolves to the response `Message` and an optional payload.
  pub async fn send_message(
    &self,
    message: Message,
    payload_opt: Option<PoolBuffer>,
  ) -> anyhow::Result<JoinHandle<anyhow::Result<(Message, Option<PoolBuffer>)>>> {
    let mut inner = self.0.lock().await;
    if inner.config.max_idle_connections == 1 {
      let conn = inner.get_or_create_connection().await?;
      Ok(conn.send_message(message, payload_opt).await)
    } else {
      let conn = inner.get_connection().await?;
      Ok(conn.send_message(message, payload_opt).await)
    }
  }

  /// Retrieves the current session information, including extra info from the handshake.
  ///
  /// # Returns
  ///
  /// A tuple of `(SessionInfo, H::SessionExtraInfo)` if the handshake was successful.
  ///
  /// # Errors
  ///
  /// Returns an error if no valid connection could be established.
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

  /// Gracefully shuts down the client by terminating connections.
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` once all connections are closed.
  pub async fn shutdown(&self) -> anyhow::Result<()> {
    let mut inner = self.0.lock().await;
    inner.shutdown().await
  }

  /// Generates the next unique correlation ID for message tracking.
  ///
  /// This method provides a monotonically increasing 16-bit identifier that can be used
  /// to correlate requests with their corresponding responses. The ID counter wraps around
  /// to 0 after reaching the maximum value of `u16::MAX`.
  ///
  /// # Returns
  ///
  /// A unique `u32` correlation ID that can be used for message tracking.
  pub async fn next_id(&self) -> u32 {
    let inner = self.0.lock().await;
    inner.next_id()
  }

  /// Returns a stream of inbound messages from the server.
  ///
  /// This method provides access to unsolicited messages sent by the server that are not
  /// responses to client requests.
  ///
  /// # Important
  ///
  /// This method can only be called **once** per `Client` instance. Subsequent calls will panic.
  /// This is because the method takes ownership of the internal receiver, ensuring there is only
  /// one consumer of inbound messages.
  ///
  /// # Panics
  ///
  /// Panics if called more than once on the same `Client` instance.
  pub async fn inbound_stream(&self) -> Receiver<(Message, Option<PoolBuffer>)> {
    let mut inner = self.0.lock().await;
    inner.inbound_rx.take().expect("inbound_stream can only be called once")
  }
}

// === ClientInner ===

pub struct ClientInner<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  /// The client ID used to identify the client.
  client_id: Arc<String>,

  /// The client configuration.
  config: Arc<Config>,

  /// The next correlation id.
  next_id: AtomicU32,

  /// The next connection id.
  next_conn_id: AtomicU32,

  /// The dialer used to establish connections.
  dialer: Arc<dyn Dialer<Stream = S>>,

  /// The handshaker used to perform the handshake with the server.
  handshaker: HS,

  /// Sender for inbound messages.
  inbound_tx: Sender<(Message, Option<PoolBuffer>)>,

  /// Receiver for inbound messages.
  inbound_rx: Option<Receiver<(Message, Option<PoolBuffer>)>>,

  /// Single persistent connection (lazily initialized).
  conn: Option<Arc<ClientConn<S, HS, ST>>>,

  /// Connection pool for managing connections (only used when max_idle_connections > 1).
  conn_pool: Option<object_pool::ObjectPool<ClientConnManager<S, HS, ST>>>,

  /// Phantom data for the service type.
  _service_type: PhantomData<ST>,
}

impl<S, HS, ST> fmt::Debug for ClientInner<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ClientInner").field("client_id", &self.client_id).field("config", &self.config).finish()
  }
}

// === impl ClientInner ===

impl<S, HS, ST> ClientInner<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  fn new(
    client_id: impl Into<String>,
    config: Config,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
  ) -> anyhow::Result<Self> {
    let arc_client_id = Arc::new(client_id.into());
    let arc_config = Arc::new(config);

    let (inbound_tx, inbound_rx) = bounded(INBOUND_QUEUE_SIZE);

    Ok(Self {
      client_id: arc_client_id,
      config: arc_config,
      next_id: AtomicU32::new(1),
      next_conn_id: AtomicU32::new(1),
      dialer,
      handshaker,
      conn: None,
      inbound_tx,
      inbound_rx: Some(inbound_rx),
      conn_pool: None,
      _service_type: PhantomData,
    })
  }

  fn new_pooled(
    client_id: impl Into<String>,
    config: Config,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
  ) -> anyhow::Result<Self> {
    let max_idle_connections = config.max_idle_connections;

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
    .max_size(max_idle_connections)
    .build()
    .map_err(|e| anyhow!("failed to build connection pool: {}", e))?;

    Ok(Self {
      client_id: arc_client_id,
      config: arc_config,
      next_id: AtomicU32::new(1),
      next_conn_id: AtomicU32::new(1),
      dialer,
      handshaker,
      conn: None,
      inbound_tx,
      inbound_rx: Some(inbound_rx),
      conn_pool: Some(conn_pool),
      _service_type: PhantomData,
    })
  }

  async fn get_or_create_connection(&mut self) -> anyhow::Result<Arc<ClientConn<S, HS, ST>>> {
    debug_assert!(
      self.config.max_idle_connections == 1,
      "get_or_create_connection should only be called in single connection mode (max_idle_connections == 1)"
    );

    // Fast path: return existing connection if healthy
    if let Some(conn) = &self.conn
      && !conn.is_unhealthy()
    {
      return Ok(conn.clone());
    }

    // Slow path: create new connection with retry logic
    let conn = ExponentialBackoff::new()
      .with_initial_delay(self.config.backoff_initial_delay)
      .with_max_delay(self.config.backoff_max_delay)
      .with_jitter(true)
      .with_max_attempts(self.config.backoff_max_retries)
      .retry_with_backoff(|| async { self.create_connection().await }, ::tokio::time::sleep)
      .await?;

    let conn = Arc::new(conn);
    self.conn = Some(conn.clone());

    Ok(conn)
  }

  async fn get_connection(&self) -> anyhow::Result<object_pool::ObjectPoolObject<ClientConnManager<S, HS, ST>>> {
    debug_assert!(
      self.config.max_idle_connections > 1,
      "get_connection should only be called in pooled mode (max_idle_connections > 1)"
    );

    let conn_pool = self.conn_pool.as_ref().expect("conn_pool must be set for pooled mode");

    ExponentialBackoff::new()
      .with_initial_delay(self.config.backoff_initial_delay)
      .with_max_delay(self.config.backoff_max_delay)
      .with_jitter(true)
      .with_max_attempts(self.config.backoff_max_retries)
      .retry_with_backoff(|| async { self.get_connection_from_pool(conn_pool).await }, ::tokio::time::sleep)
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
        // Filter out unhealthy connections and retry.
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
      Ok(())
    } else {
      if let Some(conn_pool) = &self.conn_pool {
        conn_pool.retain(|_| false);
        conn_pool.close();
      }
      Ok(())
    }
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
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  /// The client ID used to identify the client.
  client_id: Arc<String>,

  /// The client configuration.
  config: Arc<Config>,

  /// Unique connection ID generator.
  next_conn_id: atomic::AtomicU32,

  /// The dialer used to establish connections.
  dialer: Arc<dyn Dialer<Stream = S>>,

  /// The handshaker used to perform the handshake with the server.
  handshaker: HS,

  /// The inbound message sender.
  inbound_tx: Sender<(Message, Option<PoolBuffer>)>,

  /// Phantom data for the service type.
  _service_type: std::marker::PhantomData<ST>,
}

impl<S, HS, ST> ClientConnManager<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  pub fn new(
    client_id: Arc<String>,
    config: Arc<Config>,
    dialer: Arc<dyn Dialer<Stream = S>>,
    handshaker: HS,
    inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
  ) -> Self {
    Self {
      client_id,
      config,
      next_conn_id: atomic::AtomicU32::new(1),
      dialer,
      handshaker,
      inbound_tx,
      _service_type: std::marker::PhantomData,
    }
  }
}

impl<S, HS, ST> object_pool::Manager for ClientConnManager<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
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

  async fn recycle(&self, conn: &mut ClientConn<S, HS, ST>) -> object_pool::RecycleResult<Self::Error> {
    // Check if the connection is still healthy, and if not, shutdown it.
    if conn.is_unhealthy() {
      conn.shutdown().await.ok();
      return Err(object_pool::RecycleError::Backend(conn.error_state.take_error().unwrap()));
    }
    Ok(())
  }

  fn detach(&self, conn: &mut ClientConn<S, HS, ST>) {
    tokio::task::block_in_place(|| {
      tokio::runtime::Handle::current().block_on(async {
        match conn.shutdown().await {
          Ok(_) => {
            debug!(
              client_id = conn.client_id.as_str(),
              connection_id = conn.conn_id,
              service_type = ST::NAME,
              "detached client connection"
            )
          },
          Err(e) => error!(
            client_id = conn.client_id.as_str(),
            connection_id = conn.conn_id,
            service_type = ST::NAME,
            "failed to detach client connection: {}",
            e
          ),
        }
      })
    });
  }
}

struct ClientConn<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
  HS: Handshaker<S>,
  ST: Service,
{
  /// The client ID used to identify the client.
  client_id: Arc<String>,

  /// Unique connection ID.
  conn_id: u32,

  /// The configuration for the client.
  config: Arc<Config>,

  /// The dialer used to establish connections.
  dialer: Arc<dyn Dialer<Stream = S>>,

  /// The handshaker used to perform the handshake with the server.
  handshaker: HS,

  /// The inbound message sender.
  inbound_tx: Sender<(Message, Option<PoolBuffer>)>,

  /// The session information after the handshake is completed.
  session_info: Option<(SessionInfo, HS::SessionExtraInfo)>,

  /// Semaphore to limit the number of inflight requests.
  inflight_requests_sem: Option<Arc<Semaphore>>,

  /// Pending requests map, used to track requests that are waiting for a response.
  pending_requests: PendingRequests,

  /// The sender for the writer task.
  writer_tx: Option<Sender<(Message, Option<PoolBuffer>)>>,

  /// Connection task tracker.
  task_tracker: TaskTracker,

  /// The shutdown cancellation token.
  shutdown_token: CancellationToken,

  /// Indicates whether the connection is active.
  error_state: ErrorState,

  /// Phantom data for the service type.
  _service_type: PhantomData<ST>,
}

// === impl ClientConn ===

impl<S, HS, ST> ClientConn<S, HS, ST>
where
  S: AsyncRead + AsyncWrite + Send + Sync + 'static,
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
    let task_tracker = TaskTracker::new();
    let shutdown_token = CancellationToken::new();

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
      task_tracker,
      shutdown_token,
      error_state: ErrorState::new(),
      _service_type: PhantomData,
    }
  }

  async fn connect(&mut self) -> anyhow::Result<()> {
    // Establish connection using the dialer
    let mut stream = self.dialer.dial().await?;

    // Handshake with the server
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

    // Configure max inflight requests semaphore
    self.inflight_requests_sem = Some(Arc::new(Semaphore::new(session_info.max_inflight_requests as usize)));

    // Create buffer pools for message serialization (reading and writing) and payload handling
    let message_pool = Pool::new(2 * self.config.max_idle_connections, session_info.max_message_size as usize);

    // Calculate the maximum number of in-flight payload buffers.
    // This includes space for outbound and inbound messages, as well as space for each idle connection (reader and writer tasks).
    let payload_buffer_count = OUTBOUND_QUEUE_SIZE + INBOUND_QUEUE_SIZE + (2 * self.config.max_idle_connections);

    let payload_pool = Pool::new(payload_buffer_count, session_info.max_payload_size as usize);

    // Spawn writer and reader tasks
    let task_tracker = self.task_tracker.clone();
    let shutdown_token = self.shutdown_token.clone();

    let (rh, wh) = AsyncReadExt::split(stream);

    let (writer_tx, writer_rx) = bounded::<(Message, Option<PoolBuffer>)>(OUTBOUND_QUEUE_SIZE);
    task_tracker.spawn(Self::writer_task(
      self.client_id.clone(),
      self.conn_id,
      wh,
      writer_rx,
      payload_pool.acquire_buffer().await,
      self.error_state.clone(),
      shutdown_token.clone(),
    ));
    self.writer_tx = Some(writer_tx.clone());

    task_tracker.spawn(Self::reader_task(
      self.client_id.clone(),
      self.conn_id,
      rh,
      message_pool.acquire_buffer().await,
      payload_pool,
      self.pending_requests.clone(),
      self.error_state.clone(),
      writer_tx.clone(),
      self.inbound_tx.clone(),
      shutdown_token.clone(),
      self.config.payload_read_timeout,
    ));

    self.session_info = Some((session_info, session_extra_info));

    Ok(())
  }

  async fn shutdown(&self) -> anyhow::Result<()> {
    // Signal shutdown to all tasks
    self.shutdown_token.cancel();

    // Wait for all tasks to complete
    self.task_tracker.close();
    self.task_tracker.wait().await;

    Ok(())
  }

  async fn send_message(
    &self,
    message: Message,
    payload_opt: Option<PoolBuffer>,
  ) -> JoinHandle<anyhow::Result<(Message, Option<PoolBuffer>)>> {
    assert!(self.inflight_requests_sem.is_some());

    let request_timeout = self.config.timeout;
    let pending_requests = self.pending_requests.clone();
    let writer_tx = self.writer_tx.as_ref().unwrap().clone();
    let inflight_requests_sem = self.inflight_requests_sem.as_ref().unwrap().clone();

    self.task_tracker.spawn(async move {
      match tokio::time::timeout(
        request_timeout,
        Self::perform_request(message, payload_opt, pending_requests, writer_tx, inflight_requests_sem),
      )
      .await
      {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("request timed out")),
      }
    })
  }

  #[inline]
  fn is_unhealthy(&self) -> bool {
    self.error_state.has_error()
  }

  async fn perform_request(
    message: Message,
    payload_opt: Option<PoolBuffer>,
    pending_requests: PendingRequests,
    writer_tx: Sender<(Message, Option<PoolBuffer>)>,
    inflight_requests_sem: Arc<Semaphore>,
  ) -> anyhow::Result<(Message, Option<PoolBuffer>)> {
    // First, check if the message has a correlation ID.
    let correlation_id = match message.correlation_id() {
      Some(id) => id,
      None => {
        return Err(anyhow::anyhow!("message must have a correlation identifier"));
      },
    };

    // Acquire a permit from the semaphore.
    let permit = inflight_requests_sem.acquire_arc().await;

    // Create response channels.
    let (resp_tx, resp_rx) = oneshot::channel();

    // Register the pending request.
    pending_requests.insert(correlation_id, PendingRequest { sender: Some(resp_tx), _permit: permit });

    // Send the message.
    if let Err(e) = writer_tx.send((message, payload_opt)).await {
      pending_requests.remove(&correlation_id);
      return Err(anyhow!("failed to send message: {}", e));
    }

    // Wait for a response with timeout.
    let resp = resp_rx.await;

    pending_requests.remove(&correlation_id);

    // Return the response.
    match resp {
      Ok(Ok((msg, payload_opt))) => Ok((msg, payload_opt)),
      Ok(Err(e)) => Err(e),
      Err(_) => panic!("response channel closed before receiving a response"),
    }
  }

  async fn writer_task(
    client_id: Arc<String>,
    conn_id: u32,
    mut wh: futures::io::WriteHalf<S>,
    rx: Receiver<(Message, Option<PoolBuffer>)>,
    mut message_buff: MutablePoolBuffer,
    error_state: ErrorState,
    shutdown_token: CancellationToken,
  ) {
    let cancelled = shutdown_token.cancelled();
    tokio::pin!(cancelled);

    loop {
      tokio::select! {
        // Write the message to the stream.
        msg_res = rx.recv() => {
          match msg_res {
            Ok((msg, payload_opt)) => {
              match Self::write_message(&msg, payload_opt, &mut wh, message_buff.as_mut_slice()).await {
                Ok(_) => {},
                Err(e) => {
                  error!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, "{}", e.to_string());
                  error_state.set_error(e);
                  break;
                },
              }
            }
            Err(_) => {
              // All senders have been dropped, exit the loop.
              break;
            }
          }
        },

        // Shutdown token received, exit the loop.
        _ = &mut cancelled => {
          break;
        }
      }
    }

    match wh.close().await {
      Ok(_) => {},
      Err(e) => warn!(
        client_id = client_id.as_str(),
        connection_id = conn_id,
        service_type = ST::NAME,
        "failed to shutdown client stream: {}",
        e
      ),
    }
  }

  #[allow(clippy::too_many_arguments)]
  async fn reader_task(
    client_id: Arc<String>,
    conn_id: u32,
    rh: futures::io::ReadHalf<S>,
    read_buffer: MutablePoolBuffer,
    payload_buffer_pool: Pool,
    pending_requests: PendingRequests,
    error_state: ErrorState,
    writer_tx: Sender<(Message, Option<PoolBuffer>)>,
    inbound_tx: Sender<(Message, Option<PoolBuffer>)>,
    shutdown_token: CancellationToken,
    payload_read_timeout: Duration,
  ) where
    ST: Service,
  {
    let mut stream_reader = StreamReader::with_pool_buffer(rh, read_buffer);

    let cancelled = shutdown_token.cancelled();
    tokio::pin!(cancelled);

    loop {
      tokio::select! {
        // Read the next line from the stream.
        res = {
          stream_reader.next()
        } => {
          match res {
            Ok(true) => {
              let line_bytes = stream_reader.get_line().unwrap();

              // Deserialize the server message and handle it.
              match deserialize(Cursor::new(line_bytes)) {
                Ok(msg) => {
                    // Read optional payload.
                    let res = match Self::read_message_payload(&msg, &mut stream_reader, payload_buffer_pool.acquire_buffer().await, payload_read_timeout).await {
                        Ok(payload_opt) => Ok((msg.clone(), payload_opt)),
                        Err(e) => Err(anyhow!(e)),
                    };

                    if let Some(correlation_id) = msg.correlation_id() {
                        match msg {
                            Message::Ping(_) => {
                                trace!(client_id = client_id.as_str(), connection_id = conn_id, correlation_id, service_type = ST::NAME, "received ping message");

                                if writer_tx.send((Message::Pong(PongParameters { id: correlation_id }), None)).await.is_ok() {
                                    trace!(client_id = client_id.as_str(), connection_id = conn_id, correlation_id, service_type = ST::NAME, "sent pong message");
                                }
                                continue;
                            },
                            _ => {
                                // If the response correlates with a pending request, send the result to the corresponding sender.
                                if let Some(sender) = pending_requests.take_response_sender(correlation_id) {
                                    if sender.send(res).is_err() {
                                        warn!(client_id = client_id.as_str(), connection_id = conn_id, correlation_id, service_type = ST::NAME, "failed to send client response: receiver dropped");
                                    }
                                } else {
                                    warn!(client_id = client_id.as_str(), connection_id = conn_id, correlation_id, service_type = ST::NAME, "unexpected response");
                                }
                            }
                        }
                    } else {
                        match res {
                            Ok((msg, payload_opt)) => {
                                if let Err(e) = inbound_tx.try_send((msg, payload_opt)) {
                                    warn!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, error = ?e, "dropped inbound message: channel full or closed");
                                }
                            },
                            Err(e) => {
                                warn!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, error = ?e, "failed to read inbound message payload");
                            }
                        }
                    }
                }
                Err(e) => {
                  warn!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, "{}", e.to_string());
                  error_state.set_error(e);
                  break;
                }
              }
            }

            Ok(false) => {
              // Stream closed by the server.
              debug!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, "connection closed by peer");
              error_state.set_error(anyhow!("connection closed by peer"));
              break;
            }

            Err(e) => {
              warn!(client_id = client_id.as_str(), connection_id = conn_id, service_type = ST::NAME, "{}", e.to_string());
              error_state.set_error(e.into());
              break;
            }
          }
        }

        // Shutdown token received, exit the loop.
        _ = &mut cancelled => {
          break;
        }
      }
    }
  }

  async fn read_message_payload(
    msg: &Message,
    stream_reader: &mut StreamReader<futures::io::ReadHalf<S>>,
    mut pool_buff: MutablePoolBuffer,
    payload_read_timeout: Duration,
  ) -> anyhow::Result<Option<PoolBuffer>> {
    match msg.payload_info() {
      Some(payload_info) => {
        let payload = &mut pool_buff.as_mut_slice()[..payload_info.length];

        match tokio::time::timeout(payload_read_timeout, Self::read_payload(payload, stream_reader)).await {
          Ok(res) => match res {
            Ok(_) => Ok(Some(pool_buff.freeze(payload_info.length))),
            Err(e) => Err(anyhow!("failed to read payload: {}", e)),
          },
          Err(_) => Err(anyhow!("payload read timed out")),
        }
      },
      None => Ok(None),
    }
  }

  async fn write_message(
    message: &Message,
    payload_opt: Option<PoolBuffer>,
    writer: &mut futures::io::WriteHalf<S>,
    write_buffer: &mut [u8],
  ) -> anyhow::Result<()> {
    let n = serialize(message, write_buffer).map_err(|e| anyhow!("failed to serialize message: {}", e))?;
    writer.write_all(&write_buffer[..n]).await?;

    if let Some(payload) = payload_opt {
      writer.write_all(payload.as_slice()).await?;
      let _ = writer.write(b"\n").await?;
    }

    writer.flush().await?;

    Ok(())
  }

  async fn read_payload(
    buffer: &mut [u8],
    stream_reader: &mut StreamReader<futures::io::ReadHalf<S>>,
  ) -> anyhow::Result<()> {
    stream_reader.read_raw(buffer).await?;

    // Read last byte, and ensure it's a newline.
    let mut cr: [u8; 1] = [0; 1];
    stream_reader.read_raw(&mut cr).await?;

    // Verify it's actually a newline
    if cr[0] != b'\n' {
      return Err(anyhow!("invalid payload format"));
    }

    Ok(())
  }
}
