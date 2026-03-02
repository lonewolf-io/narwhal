// SPDX-License-Identifier: BSD-3-Clause

use core::fmt::Debug;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::future::Future;
use std::io::Cursor;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use futures::FutureExt;
use monoio::BufResult;
use monoio::buf::{IoBuf, IoBufMut, IoVecBuf, IoVecBufMut};
use monoio::io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt};
use rand::random;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

use async_channel::{Receiver, Sender, TryRecvError, bounded};

use slab::Slab;

use narwhal_protocol::ErrorReason::{
  BadRequest, InternalServerError, OutboundQueueIsFull, PolicyViolation, ResponseTooLarge, ServerShuttingDown, Timeout,
};
use narwhal_protocol::{ErrorParameters, Message, PingParameters, SerializeError, deserialize, serialize};

use narwhal_util::codec_monoio::{StreamReader, StreamReaderError};

use narwhal_util::pool::{BucketedPool, MutablePoolBuffer, Pool, PoolBuffer};
use narwhal_util::string_atom::StringAtom;

use crate::runtime::Task;
use crate::service::Service;

const SERVER_OVERLOADED_ERROR: &[u8] = b"ERROR reason=SERVER_OVERLOADED detail=\\\"max connections reached\\\"\n";

/// A lock-free shared stream wrapper for a single-threaded io_uring runtime.
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

impl<S: AsyncReadRent> AsyncReadRent for LocalStream<S> {
  async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
    // SAFETY: single-threaded and cooperative – only one task
    // executes at any point, so no concurrent mutable access can occur.
    let stream = unsafe { &mut *self.0.get() };
    stream.read(buf).await
  }

  async fn readv<B: IoVecBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
    let stream = unsafe { &mut *self.0.get() };
    stream.readv(buf).await
  }
}

impl<S: AsyncWriteRent> AsyncWriteRent for LocalStream<S> {
  async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
    let stream = unsafe { &mut *self.0.get() };
    stream.write(buf).await
  }

  async fn writev<B: IoVecBuf>(&mut self, buf: B) -> BufResult<usize, B> {
    let stream = unsafe { &mut *self.0.get() };
    stream.writev(buf).await
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

const MAX_BUFFERS_PER_BATCH: usize = 192;

const READ_CHANNEL_CAPACITY: usize = 1024;

const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Result type sent from the read loop to the main connection loop.
enum ReadResult {
  /// Successfully read and parsed a message with an optional payload.
  Message { message: Message, payload: Option<PoolBuffer> },
  /// Stream closed by peer (EOF).
  Eof,
  /// Read loop encountered an error condition.
  ///
  /// The contained message is always a [`Message::Error`] that the main loop
  /// should write to the wire before closing the connection.
  Error(Message),
}

/// Represents the current state of a client connection in the protocol flow.
///
/// Client connections progress through these states in a strictly forward-only manner.
/// Once a connection reaches the `Authenticated` state, it remains in that state
/// until the connection is closed.
///
/// The implementation enforces that state transitions can only advance forward
/// and never regress to a previous state.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd)]
pub enum State {
  /// Initial state when a client first connects.
  /// In this state, only initial handshake messages should be accepted.
  Connecting,

  /// Client has established the connection but has not yet authenticated.
  /// In this state, only authentication-related messages are accepted.
  /// A timeout will disconnect the client if authentication is not completed.
  Connected,

  /// Terminal state: client has successfully authenticated and can perform all operations.
  /// Once a connection reaches this state, it cannot transition to any other state.
  /// The negotiated heartbeat interval is provided and used for connection health monitoring.
  /// All protocol messages should be accepted in this state.
  Authenticated { heartbeat_interval: Duration },
}

/// A trait for handling messages received by a connection.
///
/// Implementors of this trait are responsible for processing incoming protocol messages,
/// managing connection state transitions, and executing appropriate business logic.
///
/// The `Dispatcher` trait is designed to work within the connection management system
/// and follows a state-based approach to protocol handling, where different message
/// types are allowed in different connection states.
///
/// # State Transitions
///
/// Dispatchers enforce a forward-only state progression through the `State` enum:
/// - `State::Connecting` → `State::Connected` → `State::Authenticated`
///
/// Implementations must ensure state transitions only move forward, never backward.
#[async_trait(?Send)]
pub trait Dispatcher: 'static {
  /// Processes an incoming message based on the current connection state.
  ///
  /// This method is the core of the message handling logic. It receives a message,
  /// an optional payload buffer, and the current connection state, and is responsible
  /// for executing the appropriate business logic.
  ///
  /// # State Transitions
  ///
  /// This method may return a new state to transition the connection to. The
  /// connection manager will enforce that transitions only move forward in the
  /// state progression.
  ///
  /// # Parameters
  ///
  /// * `msg` - The protocol message to process
  /// * `payload` - Optional payload data associated with the message,
  ///               typically present for content-bearing messages like broadcasts
  /// * `state` - The current connection state
  ///
  /// # Returns
  ///
  /// * `Ok(Some(new_state))` - Processing succeeded and the connection should
  ///                           transition to the new state
  /// * `Ok(None)` - Processing succeeded with no state change
  /// * `Err(e)` - An error occurred during processing
  ///
  /// # Errors
  ///
  /// Errors are typically wrapped in `ConnError` to provide structured error
  /// information to the client, including whether the error is recoverable.
  async fn dispatch_message(
    &mut self,
    msg: Message,
    payload: Option<PoolBuffer>,
    state: State,
  ) -> anyhow::Result<Option<State>>;

  /// Initializes the dispatcher.
  ///
  /// This method is called once when a new connection is established, before
  /// any messages are processed. It allows the dispatcher to set up its initial
  /// state, register with other system components, or perform other setup tasks.
  ///
  /// # Returns
  ///
  /// * `Ok(())` - Bootstrapping succeeded
  /// * `Err(e)` - An error occurred during bootstrapping
  ///
  /// # Errors
  ///
  /// If bootstrapping fails, the connection will be closed immediately.
  async fn bootstrap(&mut self) -> anyhow::Result<()>;

  /// Cleans up the dispatcher's resources.
  ///
  /// This method is called when a connection is closed, either due to a client
  /// disconnect, an error, or server shutdown. It allows the dispatcher to
  /// clean up any resources, unregister from other components, or perform
  /// other teardown tasks.
  ///
  /// # Returns
  ///
  /// * `Ok(())` - Shutdown succeeded
  /// * `Err(e)` - An error occurred during shutdown
  ///
  /// # Errors
  ///
  /// Errors during shutdown are logged but generally do not affect the
  /// connection close process.
  async fn shutdown(&mut self) -> anyhow::Result<()>;
}

/// A factory for creating new `Dispatcher` instances.
///
/// This trait separates the creation of dispatchers from their usage,
/// allowing for dependency injection and better testability.
///
/// Implementors typically hold configuration data and references to
/// shared resources that new dispatchers will need access to.
#[async_trait(?Send)]
pub trait DispatcherFactory<D: Dispatcher>: Clone + Send + Sync + 'static {
  /// Creates a new instance of a dispatcher factory.
  ///
  /// This method is called whenever a new connection is established
  /// and needs a dispatcher to handle its messages.
  ///
  /// # Returns
  ///
  /// A dispatcher instance, ready to handle messages for the new connection.
  async fn create(&mut self, handler: usize, tx: ConnTx) -> D;

  /// Bootstraps the dispatcher factory with initial configuration and resources.
  ///
  /// This method is called once during initialization to set up any shared
  /// resources, establish connections, or perform other one-time setup tasks
  /// that the factory needs before it can start creating dispatchers.
  ///
  /// Implementations might use this to:
  /// * Initialize connection pools
  /// * Set up background tasks
  /// * Load configuration from external sources
  /// * Establish connections to external services
  ///
  /// # Returns
  ///
  /// * `Ok(())` - Bootstrap succeeded and the factory is ready to create dispatchers
  /// * `Err(e)` - An error occurred during bootstrap, preventing factory initialization
  async fn bootstrap(&mut self) -> anyhow::Result<()>;

  /// Shuts down the dispatcher factory and cleans up resources.
  ///
  /// This method is called when the connection manager is shutting down,
  /// allowing the factory to clean up any resources it holds.
  ///
  /// # Returns
  ///
  /// * `Ok(())` - Shutdown succeeded
  /// * `Err(e)` - An error occurred during shutdown
  async fn shutdown(&mut self) -> anyhow::Result<()>;
}

/// The connection configuration.
#[derive(Debug, Default)]
pub struct Config {
  /// The maximum number of connections that the manager can handle.
  pub max_connections: u32,

  /// The maximum message size allowed.
  pub max_message_size: u32,

  /// The maximum payload size allowed.
  pub max_payload_size: u32,

  /// The timeout for the connection phase.
  pub connect_timeout: Duration,

  /// The timeout for the authentication phase.
  pub authenticate_timeout: Duration,

  /// The timeout for reading a broadcast payload.
  pub payload_read_timeout: Duration,

  /// Total memory budget in bytes for the payload buffer pool.
  /// The pool will allocate buffers of varying sizes up to this total.
  pub payload_pool_memory_budget: u64,

  /// The maximum number of outbound messages that can be enqueued
  /// before disconnecting the client.
  pub outbound_message_queue_size: u32,

  /// The timeout for the request.
  pub request_timeout: Duration,

  /// The connection maximum number of inflight requests.
  pub max_inflight_requests: u32,

  /// The maximum number of bytes that can be read per second.
  pub rate_limit: u32,
}

/// The connection runtime inner state.
struct ConnRuntimeInner<ST: Service> {
  /// The configuration.
  config: Arc<Config>,

  /// The next handler ID to assign.
  next_handler: AtomicUsize,

  /// The number of active connections.
  active_connections: Arc<AtomicU32>,

  /// The message buffer pool.
  message_buffer_pool: Pool,

  /// The payload buffer pool.
  payload_buffer_pool: BucketedPool,

  /// A single shared newline buffer
  newline_buffer: PoolBuffer,

  /// The shutdown cancellation token.
  shutdown_token: CancellationToken,

  /// Phantom data for the service type.
  _phantom: PhantomData<ST>,
}

impl<ST: Service> std::fmt::Debug for ConnRuntimeInner<ST> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ConnRuntimeInner")
      .field("config", &self.config)
      .field("next_handler", &self.next_handler.load(Ordering::Relaxed))
      .field("active_connections", &self.active_connections)
      .field("message_buffer_pool", &self.message_buffer_pool)
      .field("payload_buffer_pool", &self.payload_buffer_pool)
      .field("newline_buffer", &self.newline_buffer)
      .field("shutdown_token", &self.shutdown_token)
      .finish()
  }
}

/// The connection runtime.
#[derive(Debug)]
pub struct ConnRuntime<ST: Service>(Arc<ConnRuntimeInner<ST>>);

// === impl ConnRuntime ===

impl<ST: Service> Clone for ConnRuntime<ST> {
  fn clone(&self) -> Self {
    Self(self.0.clone())
  }
}

impl<ST: Service> ConnRuntime<ST> {
  /// Creates a new connection runtime.
  pub async fn new(config: impl Into<Config>) -> Self {
    let conn_cfg = config.into();

    let max_connections = conn_cfg.max_connections as usize;

    // Account for the fact that each connection has two message buffers (read and write)
    let max_message_pool_buffers = max_connections * 2 + MAX_BUFFERS_PER_BATCH;

    let max_payload_buffers_per_bucket = max_connections + (max_connections * MAX_BUFFERS_PER_BATCH);

    // Create message buffer pool
    let message_buffer_pool = Pool::new(max_message_pool_buffers, conn_cfg.max_message_size as usize);

    // Create the bucketed pool with the configured memory budget.
    // The pool will distribute the budget across different size buckets.
    let payload_buffer_pool = BucketedPool::new_with_memory_budget(
      256,                                          // min buffer size
      conn_cfg.max_payload_size as usize,           // max buffer size
      conn_cfg.payload_pool_memory_budget as usize, // total memory budget
      max_payload_buffers_per_bucket,               // max buffers per bucket
      2,                                            // 2x growth between buckets
      0.5,                                          // 50% decay
    );

    // Create a single shared newline buffer
    let newline_buffer = {
      let pool = Pool::new(1, 1);
      let mut buf = pool.acquire_buffer().await;
      buf[0] = b'\n';
      buf.freeze(1)
    };

    let shutdown_token = CancellationToken::new();

    let inner = ConnRuntimeInner {
      config: Arc::new(conn_cfg),
      next_handler: AtomicUsize::new(1),
      active_connections: Arc::new(AtomicU32::new(0)),
      message_buffer_pool,
      payload_buffer_pool,
      newline_buffer,
      shutdown_token,
      _phantom: PhantomData,
    };

    Self(Arc::new(inner))
  }

  /// Bootstraps the connection runtime.
  pub async fn bootstrap(&self) -> anyhow::Result<()> {
    info!(max_conns = self.0.config.max_connections, service_type = ST::NAME, "connection runtime started");

    Ok(())
  }

  /// Shuts down the connection runtime.
  pub async fn shutdown(&self) -> anyhow::Result<()> {
    // Notify the shutdown to all active connections.
    self.0.shutdown_token.cancel();

    // Poll until all active connections have drained.
    let deadline = std::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
    loop {
      if self.0.active_connections.load(Ordering::SeqCst) == 0 {
        break;
      }
      if std::time::Instant::now() >= deadline {
        let remaining = self.0.active_connections.load(Ordering::SeqCst);
        warn!(remaining, service_type = ST::NAME, "connection runtime timed out waiting for connections to drain");
        break;
      }

      monoio::time::sleep(SHUTDOWN_DRAIN_POLL_INTERVAL).await;
    }

    info!(service_type = ST::NAME, "connection runtime stopped");

    Ok(())
  }

  /// Runs a single client connection to completion.
  pub async fn run_connection<S, D: Dispatcher, DF: DispatcherFactory<D>>(
    &self,
    mut stream: S,
    mut dispatcher_factory: DF,
  ) where
    S: AsyncReadRent + AsyncWriteRent + 'static,
  {
    // Assign a unique handler
    let handler = self.0.next_handler.fetch_add(1, Ordering::SeqCst);

    let config = self.0.config.clone();
    let message_buffer_pool = self.0.message_buffer_pool.clone();
    let payload_buffer_pool = self.0.payload_buffer_pool.clone();
    let newline_buffer = self.0.newline_buffer.clone();
    let active_connections = self.0.active_connections.clone();
    let shutdown_token = self.0.shutdown_token.clone();

    // Check if we've reached the maximum number of connections
    let current_connections = active_connections.fetch_add(1, Ordering::SeqCst);

    if current_connections >= config.max_connections {
      active_connections.fetch_sub(1, Ordering::SeqCst);

      let _ = stream.write_all(SERVER_OVERLOADED_ERROR.to_vec()).await;
      let _ = stream.flush().await;
      let _ = stream.shutdown().await;

      let max_conns = config.max_connections;
      warn!(max_conns, service_type = ST::NAME, "max connections limit reached");

      return;
    }

    // Create a new connection.
    let send_msg_channel_size = config.outbound_message_queue_size as usize;

    let (send_msg_tx, send_msg_rx) = bounded(send_msg_channel_size);
    let (close_tx, close_rx) = bounded(1);

    let tx = ConnTx { send_msg_tx, close_tx };

    let dispatcher = dispatcher_factory.create(handler, tx.clone()).await;

    let mut conn = Box::new(Conn {
      handler,
      config: config.clone(),
      state: State::Connecting,
      dispatcher: Rc::new(UnsafeCell::new(dispatcher)),
      request_tasks: Rc::new(RefCell::new(Slab::with_capacity(config.max_inflight_requests as usize))),
      cancellation_token: CancellationToken::new(),
      activity_counter: Rc::new(Cell::new(0)),
      pong_notifier: None,
      scheduled_task: None,
      inflight_requests: Rc::new(Cell::new(0)),
      tx,
    });

    conn.schedule_timeout(config.connect_timeout, Some(StringAtom::from("connection timeout")));

    trace!(handler = handler, service_type = ST::NAME, "connection registered");

    // Run loop until the connection is closed.
    let payload_read_timeout = config.payload_read_timeout;

    let max_payload_size = config.max_payload_size as usize;

    let rate_limit = config.rate_limit;

    match Conn::<D>::run::<S, ST>(
      stream,
      &mut conn,
      handler,
      send_msg_rx,
      close_rx,
      shutdown_token,
      message_buffer_pool,
      payload_buffer_pool,
      newline_buffer,
      payload_read_timeout,
      max_payload_size,
      rate_limit,
    )
    .await
    {
      Ok(_) => {},
      Err(e) => {
        warn!(handler = handler, service_type = ST::NAME, "connection error: {}", e.to_string());
      },
    }

    // Decrement the active connections counter
    active_connections.fetch_sub(1, Ordering::SeqCst);

    trace!(handler = handler, service_type = ST::NAME, "connection deregistered");
  }
}

/// A client connection.
pub struct Conn<D: Dispatcher> {
  /// The connection configuration.
  config: Arc<Config>,

  /// The connection handler.
  handler: usize,

  /// The connection state.
  state: State,

  /// The connection dispatcher.
  dispatcher: Rc<UnsafeCell<D>>,

  /// The transmitter channels.
  tx: ConnTx,

  /// Current number of inflight requests.
  inflight_requests: Rc<Cell<u32>>,

  // Increments on every received message
  activity_counter: Rc<Cell<u64>>,

  // Channel to send PONG notifications to ping loop
  pong_notifier: Option<Sender<u32>>,

  /// Cancellation sender for the current scheduled task (ping or timeout).
  /// Dropping this sender signals the spawned task to stop (monoio's
  /// JoinHandle drop does NOT cancel the task, so we use a channel).
  scheduled_task: Option<Sender<()>>,

  /// Track tasks associated with connection requests.
  request_tasks: Rc<RefCell<Slab<Task>>>,

  /// Token used to signal request cancellation.
  cancellation_token: CancellationToken,
}

// ===== impl Conn =====

impl<D: Dispatcher> Conn<D> {
  async fn dispatch_message<ST: Service>(&mut self, msg: Message, payload: Option<PoolBuffer>) -> anyhow::Result<()> {
    let dispatcher = self.dispatcher.clone();

    match self.state {
      State::Authenticated { .. } => {
        // Handle pong message
        if let Message::Pong(params) = &msg {
          if let Some(notifier) = &self.pong_notifier {
            notifier.send(params.id).await?;
          }
          return Ok(());
        }

        // Submit the request to the dispatcher asynchronously.
        let handler = self.handler;
        let state = self.state;
        let tx = self.tx.clone();

        self.submit_request::<_, ST>(async move {
          let dispatcher_ref = unsafe { &mut *dispatcher.get() };

          match dispatcher_ref.dispatch_message(msg, payload, state).await {
            Ok(_) => {},
            Err(e) => {
              Self::notify_error::<ST>(e, tx, handler)?;
            },
          }
          Ok(())
        })?;

        // Increment activity counter
        let current = self.activity_counter.get();
        self.activity_counter.set(current + 1);
      },
      _ => {
        let dispatcher_ref = unsafe { &mut *dispatcher.get() };

        match dispatcher_ref.dispatch_message(msg, payload, self.state).await {
          Ok(Some(new_state)) => {
            assert!(new_state >= self.state, "invalid state transition");

            let old_state = self.state;
            self.state = new_state;

            // Schedule timeout according to state transition.
            if old_state != self.state {
              // Cancel any previous timeout.
              self.cancel_scheduled_task();

              match self.state {
                State::Connected => {
                  self.schedule_timeout(
                    self.config.authenticate_timeout,
                    Some(StringAtom::from("authentication timeout")),
                  );
                },
                State::Authenticated { heartbeat_interval } => {
                  self.run_ping_loop::<ST>(heartbeat_interval);
                },
                _ => {},
              }
            }
          },
          Ok(None) => {},
          Err(e) => {
            Self::notify_error::<ST>(e, self.tx.clone(), self.handler)?;
          },
        }
      },
    }

    Ok(())
  }

  fn submit_request<F, ST>(&mut self, future: F) -> anyhow::Result<()>
  where
    F: Future<Output = anyhow::Result<()>> + 'static,
    ST: Service,
  {
    // First check if the maximum number of inflight requests has been reached,
    // and if not, increment the counter.
    let max_inflight_requests = self.config.max_inflight_requests;
    let inflight_requests = self.inflight_requests.clone();

    let current = inflight_requests.get();
    if current > max_inflight_requests {
      return Err(
        narwhal_protocol::Error {
          id: None,
          reason: PolicyViolation,
          detail: Some(StringAtom::from("max inflight requests reached")),
        }
        .into(),
      );
    }
    inflight_requests.set(current + 1);

    // Spawn the request task.
    let handler = self.handler;
    let request_timeout = self.config.request_timeout;

    let tx = self.tx.clone();

    let cancellation_token = self.cancellation_token.clone();

    let task_slot = self.request_tasks.borrow().vacant_key();
    let request_tasks = self.request_tasks.clone();

    let task = monoio::spawn(async move {
      let timeout_future = monoio::time::timeout(request_timeout, future);

      futures::select! {
        res = timeout_future.fuse() => {
          match res {
            Ok(res) => {
              if let Err(e) = res && let Err(e) = Self::notify_error::<ST>(e, tx, handler) {
                warn!(handler = handler, service_type = ST::NAME, "failed to notify request error: {}", e.to_string());
              }
            },
            Err(_) => {
              error!(handler = handler, service_type = ST::NAME, "request timeout");
            }
          }
        },
        _ = cancellation_token.cancelled().fuse() => {
          trace!(handler = handler, service_type = ST::NAME, "request cancelled");
        },
      }

      // Decrement the inflight requests counter.
      let current = inflight_requests.get();
      inflight_requests.set(current.saturating_sub(1));

      // Clean up task
      if request_tasks.borrow().contains(task_slot) {
        drop(request_tasks.borrow_mut().remove(task_slot));
      }
    });

    // Store the task handle for tracking
    let key = self.request_tasks.borrow_mut().insert(task);
    debug_assert!(key == task_slot);

    Ok(())
  }

  /// Schedules a timeout task.
  fn schedule_timeout(&mut self, timeout: Duration, detail: Option<StringAtom>) {
    let tx = self.tx.clone();
    let (cancel_tx, cancel_rx) = bounded::<()>(1);

    monoio::spawn(async move {
      futures::select! {
        _ = monoio::time::sleep(timeout).fuse() => {
          tx.close(Message::Error(ErrorParameters { id: None, reason: Timeout.into(), detail }));
        },
        _ = cancel_rx.recv().fuse() => {},
      }
    });

    self.scheduled_task = Some(cancel_tx);
  }

  /// Runs the ping/pong monitoring loop
  fn run_ping_loop<ST: Service>(&mut self, heartbeat_interval: Duration) {
    let tx = self.tx.clone();
    let handler = self.handler;

    // A 3x heartbeat interval is used for the timeout.
    let heartbeat_timeout = heartbeat_interval * 3;

    let activity_counter = self.activity_counter.clone();

    // Create PONG notification channel
    let (pong_tx, pong_rx) = bounded::<u32>(1);
    self.pong_notifier = Some(pong_tx);

    // Create cancellation channel (monoio JoinHandle drop doesn't cancel)
    let (cancel_tx, cancel_rx) = bounded::<()>(1);

    monoio::spawn(async move {
      let mut cancel = std::pin::pin!(cancel_rx.recv().fuse());
      let mut last_check_counter = activity_counter.get();

      loop {
        // Sleep with cancellation
        futures::select! {
          _ = monoio::time::sleep(heartbeat_interval).fuse() => {},
          _ = cancel.as_mut() => break,
        }

        let current_counter = activity_counter.get();

        if current_counter != last_check_counter {
          last_check_counter = current_counter;
          continue; // Activity detected, skip ping
        }

        let ping_id: u32 = random();
        tx.send_message(Message::Ping(PingParameters { id: ping_id }));
        trace!(id = ping_id, handler, service_type = ST::NAME, "sent ping");

        // Wait for PONG or timeout, with cancellation
        futures::select! {
          result = monoio::time::timeout(heartbeat_timeout, pong_rx.recv()).fuse() => {
            match result {
              Ok(Ok(pong_id)) if pong_id == ping_id => {
                trace!(id = ping_id, "pong received");
                last_check_counter = activity_counter.get();
              },
              Ok(Ok(_)) => {
                tx.close(Message::Error(ErrorParameters {
                  id: None,
                  reason: BadRequest.into(),
                  detail: Some(StringAtom::from("wrong pong id")),
                }));
                break;
              },
              Ok(Err(_)) => break,
              Err(_) => {
                tx.close(Message::Error(ErrorParameters {
                  id: None,
                  reason: Timeout.into(),
                  detail: Some(StringAtom::from("ping timeout")),
                }));
                break;
              },
            }
          },
          _ = cancel.as_mut() => break,
        }
      }
    });

    self.scheduled_task = Some(cancel_tx);
  }

  /// Cancels currently scheduled task.
  fn cancel_scheduled_task(&mut self) {
    if let Some(task) = self.scheduled_task.take() {
      drop(task)
    }
  }

  /// Bootstraps the connection.
  async fn bootstrap(&mut self) -> anyhow::Result<()> {
    unsafe { &mut *self.dispatcher.get() }.bootstrap().await?;
    Ok(())
  }

  /// Shuts down the connection.
  async fn shutdown(&mut self) -> anyhow::Result<()> {
    // Cancel any pending ping or timeout task.
    self.cancel_scheduled_task();

    // Signal cancellation and wait for all request tasks to finish.
    self.cancellation_token.cancel();

    // Wait for all request tasks to complete
    let tasks = std::mem::take(&mut *self.request_tasks.borrow_mut());
    for (_key, task) in tasks {
      let _ = task.await;
    }

    // Shutdown the dispatcher.
    unsafe { &mut *self.dispatcher.get() }.shutdown().await?;

    Ok(())
  }

  fn notify_error<ST: Service>(e: anyhow::Error, tx: ConnTx, handler: usize) -> anyhow::Result<()> {
    // Notify the client about the error, or disconnect the connection in case it's an internal
    // or non-recoverable error.
    if let Some(conn_err) = e.downcast_ref::<narwhal_protocol::Error>() {
      if conn_err.is_recoverable() {
        tx.send_message(conn_err.into());
      } else {
        tx.close(conn_err.into());
      }
    } else {
      error!(handler = handler, service_type = ST::NAME, "internal server error: {}", e.to_string());
      tx.close(Message::Error(ErrorParameters { id: None, reason: InternalServerError.into(), detail: None }));
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  async fn run<S, ST>(
    stream: S,
    conn: &mut Conn<D>,
    handler: usize,
    send_msg_rx: Receiver<(Message, Option<PoolBuffer>)>,
    close_rx: Receiver<Message>,
    shutdown_token: CancellationToken,
    message_buffer_pool: Pool,
    payload_buffer_pool: BucketedPool,
    newline_buffer: PoolBuffer,
    payload_read_timeout: Duration,
    max_payload_size: usize,
    rate_limit: u32,
  ) -> anyhow::Result<()>
  where
    S: AsyncReadRent + AsyncWriteRent + 'static,
    ST: Service,
  {
    let local_stream = LocalStream::new(stream);

    let reader = local_stream.clone();
    let mut writer = local_stream;

    // Bootstrap the connection.
    conn.bootstrap().await?;

    let loop_result = async {
      Self::run_connection_loop::<_, _, ST>(
        reader,
        &mut writer,
        conn,
        handler,
        send_msg_rx,
        close_rx,
        &shutdown_token,
        message_buffer_pool.clone(),
        payload_buffer_pool.clone(),
        newline_buffer.clone(),
        payload_read_timeout,
        max_payload_size,
        rate_limit,
      )
      .await?;

      Ok::<(), anyhow::Error>(())
    }
    .await;

    // Shutdown the connection.
    let shutdown_result = conn.shutdown().await;

    match writer.shutdown().await {
      Ok(_) => {},
      Err(e) => {
        // Ignore expected socket disconnection errors that occur when client disconnects abruptly
        use std::io::ErrorKind::*;
        match e.kind() {
          NotConnected | BrokenPipe | ConnectionAborted | ConnectionReset | UnexpectedEof => {},
          _ => return Err(e.into()),
        }
      },
    }

    // Return any error from the main loop first, then any error from shutdown.
    loop_result?;
    shutdown_result?;

    Ok(())
  }

  /// Dedicated read loop spawned as a separate task.
  ///
  /// Sequentially reads from the stream, deserializes messages, reads payloads, and enforces rate limiting.
  /// Results are sent to the main connection loop to be processed.
  #[allow(clippy::too_many_arguments)]
  async fn run_read_loop<T>(
    mut stream_reader: StreamReader<LocalStream<T>>,
    read_tx: Sender<ReadResult>,
    payload_buffer_pool: BucketedPool,
    payload_read_timeout: Duration,
    max_payload_size: usize,
    rate_limit: u32,
    handler: usize,
    service_name: &'static str,
  ) where
    T: AsyncReadRent + AsyncWriteRent + 'static,
  {
    let mut rate_limit_counter: u32 = 0;
    let mut rate_limit_last_check = std::time::Instant::now();

    let mut delimiter_buf = Box::new([0u8; 1]);

    loop {
      match stream_reader.next().await {
        Ok(true) => {
          let line_bytes = stream_reader.get_line().unwrap();
          let message_length = line_bytes.len() as u32;

          match deserialize(Cursor::new(line_bytes)) {
            Ok(msg) => {
              let mut payload_opt: Option<PoolBuffer> = None;
              let mut payload_length: u32 = 0;

              // Read associated payload if present.
              if let Some(payload_info) = msg.payload_info() {
                if payload_info.length > max_payload_size {
                  let err_message = Message::Error(ErrorParameters {
                    id: payload_info.id,
                    reason: PolicyViolation.into(),
                    detail: Some("payload too large".into()),
                  });
                  let _ = read_tx.send(ReadResult::Error(err_message)).await;
                  break;
                }
                payload_length = payload_info.length as u32;

                let pool_buff = payload_buffer_pool.acquire_buffer(payload_info.length).await.unwrap();

                match monoio::time::timeout(
                  payload_read_timeout,
                  Self::read_payload(
                    pool_buff,
                    delimiter_buf,
                    &mut stream_reader,
                    payload_info.length,
                    payload_info.id,
                  ),
                )
                .await
                {
                  Ok(Ok(mut result)) => {
                    payload_opt = Some(result.data.freeze(payload_info.length));
                    delimiter_buf = result.delimiter;
                  },
                  Ok(Err(e)) => {
                    let err_message: Message = if let Some(e) = e.downcast_ref::<narwhal_protocol::Error>() {
                      e.into()
                    } else {
                      warn!(handler = handler, service_type = service_name, "failed to read payload: {}", e);
                      Message::Error(ErrorParameters { id: None, reason: InternalServerError.into(), detail: None })
                    };
                    let _ = read_tx.send(ReadResult::Error(err_message)).await;
                    break;
                  },
                  Err(_) => {
                    let err_message = Message::Error(ErrorParameters {
                      id: None,
                      reason: Timeout.into(),
                      detail: Some("payload read timeout".into()),
                    });
                    let _ = read_tx.send(ReadResult::Error(err_message)).await;
                    break;
                  },
                }
              }

              // Check if the rate limit is exceeded.
              if rate_limit > 0 {
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(rate_limit_last_check);
                if elapsed.as_secs() > 1 {
                  rate_limit_counter = 0;
                  rate_limit_last_check = now;
                }

                rate_limit_counter += message_length + payload_length;

                if rate_limit_counter > rate_limit {
                  let err_message = Message::Error(ErrorParameters {
                    id: msg.correlation_id(),
                    reason: PolicyViolation.into(),
                    detail: Some("rate limit exceeded".into()),
                  });
                  let _ = read_tx.send(ReadResult::Error(err_message)).await;
                  break;
                }
              }

              // Send parsed message to the main connection loop.
              if read_tx.send(ReadResult::Message { message: msg, payload: payload_opt }).await.is_err() {
                break;
              }
            },
            Err(e) => {
              let err_detail = format!("{}", e);
              let err_message = Message::Error(ErrorParameters {
                id: None,
                reason: BadRequest.into(),
                detail: Some(StringAtom::from(err_detail)),
              });
              let _ = read_tx.send(ReadResult::Error(err_message)).await;
              break;
            },
          }
        },
        Ok(false) => {
          // Stream closed by the client.
          let _ = read_tx.send(ReadResult::Eof).await;
          break;
        },
        Err(e) => {
          match e {
            StreamReaderError::MaxLineLengthExceeded => {
              let err_message = Message::Error(ErrorParameters {
                id: None,
                reason: PolicyViolation.into(),
                detail: Some("max message size exceeded".into()),
              });
              let _ = read_tx.send(ReadResult::Error(err_message)).await;
            },
            StreamReaderError::IoError(e) => {
              if e.kind() != std::io::ErrorKind::UnexpectedEof {
                error!(
                  handler = handler,
                  service_type = service_name,
                  "failed to read from connection: {}",
                  e.to_string()
                );
              }
            },
          }
          break;
        },
      }
    }
  }

  #[allow(clippy::too_many_arguments)]
  async fn run_connection_loop<T, W, ST>(
    reader: LocalStream<T>,
    writer: &mut W,
    conn: &mut Conn<D>,
    handler: usize,
    send_msg_rx: Receiver<(Message, Option<PoolBuffer>)>,
    close_rx: Receiver<Message>,
    shutdown_token: &CancellationToken,
    message_buffer_pool: Pool,
    payload_buffer_pool: BucketedPool,
    newline_buffer: PoolBuffer,
    payload_read_timeout: Duration,
    max_payload_size: usize,
    rate_limit: u32,
  ) -> anyhow::Result<()>
  where
    T: AsyncReadRent + AsyncWriteRent + 'static,
    W: AsyncWriteRent,
    ST: Service,
  {
    // Initialize stream reader and hand it off to the read loop.
    let read_pool_buffer = message_buffer_pool.acquire_buffer().await;
    let stream_reader = StreamReader::with_pool_buffer(reader, read_pool_buffer);

    let (read_tx, read_rx) = bounded::<ReadResult>(READ_CHANNEL_CAPACITY);

    // Spawn the read loop as a separate task.
    let reader_task = monoio::spawn(Self::run_read_loop(
      stream_reader,
      read_tx,
      payload_buffer_pool,
      payload_read_timeout,
      max_payload_size,
      rate_limit,
      handler,
      ST::NAME,
    ));

    let mut pool_buffer_batch = Vec::<PoolBuffer>::with_capacity(MAX_BUFFERS_PER_BATCH);
    let mut iovec_batch = Vec::<libc::iovec>::with_capacity(MAX_BUFFERS_PER_BATCH);

    let mut cancelled = std::pin::pin!(shutdown_token.cancelled().fuse());

    'connection_loop: loop {
      futures::select! {
        // Receive parsed messages from the read loop.
        res = read_rx.recv().fuse() => {
          match res {
            Ok(ReadResult::Message { message, payload }) => {
              match conn.dispatch_message::<ST>(message, payload).await {
                Ok(_) => {},
                Err(e) => {
                  warn!(handler = handler, service_type = ST::NAME, "failed to dispatch message: {}", e.to_string());
                  break 'connection_loop;
                },
              }
            }
            Ok(ReadResult::Eof) => {
              trace!(handler = handler, service_type = ST::NAME, "connection closed by peer");
              break 'connection_loop;
            }
            Ok(ReadResult::Error(err_message)) => {
              let _  = Self::write_message(&err_message, None, writer, message_buffer_pool.acquire_buffer().await, &newline_buffer, pool_buffer_batch, iovec_batch).await?;
              break 'connection_loop;
            }
            Err(_) => {
              // Read task exited unexpectedly.
              break 'connection_loop;
            }
          }
        },

        // Write outbound messages to the stream.
        res = send_msg_rx.recv().fuse() => {
          const MESSAGE_CHANNEL_CLOSED_LOG: &str = "message channel closed";

          match res {
            Ok((message, payload_opt)) => {
              Self::add_message_to_batch(&message, payload_opt, &mut pool_buffer_batch, &message_buffer_pool, &newline_buffer).await?;
            }
            Err(_) => {
              error!(handler = handler, service_type = ST::NAME, MESSAGE_CHANNEL_CLOSED_LOG);
              break 'connection_loop;
            }
          };

          loop {
            if pool_buffer_batch.len() >= MAX_BUFFERS_PER_BATCH {
                break;
            }

            match send_msg_rx.try_recv() {
              Ok((message, payload_opt)) => {
                Self::add_message_to_batch(&message, payload_opt, &mut pool_buffer_batch, &message_buffer_pool, &newline_buffer).await?;
              },
              Err(TryRecvError::Empty) => break,
              Err(TryRecvError::Closed) => {
                error!(handler = handler, service_type = ST::NAME, MESSAGE_CHANNEL_CLOSED_LOG);
                break 'connection_loop;
              }
            }
          }

          (pool_buffer_batch, iovec_batch) = Self::write_batch(pool_buffer_batch, iovec_batch, writer).await?;
        },

        // Close the connection.
        res = close_rx.recv().fuse() => {
          let err_message = res.unwrap();
          let _  = Self::write_message(&err_message, None, writer, message_buffer_pool.acquire_buffer().await, &newline_buffer, pool_buffer_batch, iovec_batch).await?;
          trace!(handler = handler, service_type = ST::NAME, "closed connection");
          break 'connection_loop;
        },

        // Close the connection on shutdown.
        _ = cancelled.as_mut() => {
          let err_message = Message::Error(ErrorParameters{id: None, reason: ServerShuttingDown.into(), detail: None});
          let _ = Self::write_message(&err_message, None, writer, message_buffer_pool.acquire_buffer().await, &newline_buffer, pool_buffer_batch, iovec_batch).await?;
          trace!(handler = handler, service_type = ST::NAME, "closed connection on shutdown");
          break 'connection_loop;
        },
      }
    }

    // Stop polling the reader task.
    drop(reader_task);

    Ok(())
  }

  async fn write_message<W>(
    message: &Message,
    payload_opt: Option<PoolBuffer>,
    writer: &mut W,
    message_buffer: MutablePoolBuffer,
    newline_buffer: &PoolBuffer,
    mut batch: Vec<PoolBuffer>,
    iovec_batch: Vec<libc::iovec>,
  ) -> anyhow::Result<(Vec<PoolBuffer>, Vec<libc::iovec>)>
  where
    W: AsyncWriteRent,
  {
    let message_buff = Self::serialize_message(message, message_buffer)?;
    batch.push(message_buff);

    if let Some(payload_buf) = payload_opt {
      batch.push(payload_buf);
      batch.push(newline_buffer.clone());
    };

    Self::write_batch(batch, iovec_batch, writer).await
  }

  async fn write_batch<W>(
    mut batch: Vec<PoolBuffer>,
    mut iovec_batch: Vec<libc::iovec>,
    writer: &mut W,
  ) -> anyhow::Result<(Vec<PoolBuffer>, Vec<libc::iovec>)>
  where
    W: AsyncWriteRent,
  {
    // Populate the pre-allocated iovec batch from the pool buffers.
    iovec_batch.extend(batch.iter().map(|b| libc::iovec { iov_base: b.read_ptr() as *mut _, iov_len: b.bytes_init() }));

    let mut remaining: usize = iovec_batch.iter().map(|v| v.iov_len).sum();

    // Loop writev until every byte is out.
    while remaining > 0 {
      let (result, returned_iovecs) = writer.writev(iovec_batch).await;
      let written = result?;

      iovec_batch = returned_iovecs;

      if written == 0 {
        return Err(anyhow!("writev unable to make progress"));
      }
      remaining -= written;

      if remaining == 0 {
        break;
      }

      // Advance iovecs past the bytes just written.
      let mut consumed = 0usize;
      let mut to_skip = written;

      for iovec in iovec_batch.iter_mut() {
        if to_skip >= iovec.iov_len {
          to_skip -= iovec.iov_len;
          consumed += 1;
        } else {
          // SAFETY: we advance the pointer within the bounds of the
          // original PoolBuffer, which is kept alive in `batch`.
          iovec.iov_base = unsafe { (iovec.iov_base as *mut u8).add(to_skip) as *mut _ };
          iovec.iov_len -= to_skip;
          break;
        }
      }

      // Remove fully-consumed entries so writev never sees leading
      // zero-length iovecs (which some implementations return 0 for).
      if consumed > 0 {
        iovec_batch.drain(..consumed);
      }
    }

    batch.clear();
    iovec_batch.clear();

    writer.flush().await?;

    Ok((batch, iovec_batch))
  }

  async fn add_message_to_batch(
    message: &Message,
    payload_opt: Option<PoolBuffer>,
    batch: &mut Vec<PoolBuffer>,
    message_buffer_pool: &Pool,
    newline_buffer: &PoolBuffer,
  ) -> anyhow::Result<()> {
    let message_buff = Self::serialize_message(message, message_buffer_pool.acquire_buffer().await)?;
    batch.push(message_buff);

    if let Some(payload_buf) = payload_opt {
      batch.push(payload_buf);
      batch.push(newline_buffer.clone());
    }
    Ok(())
  }

  fn serialize_message(message: &Message, message_buffer: MutablePoolBuffer) -> anyhow::Result<PoolBuffer> {
    Self::serialize_message_inner(message, message_buffer, true)
      .map_err(|e| anyhow!("failed to serialize message: {}", e))
  }

  fn serialize_message_inner(
    message: &Message,
    mut message_buffer: MutablePoolBuffer,
    handle_too_large: bool,
  ) -> anyhow::Result<PoolBuffer> {
    let write_buffer = message_buffer.as_mut_slice();

    match serialize(message, write_buffer) {
      Ok(n) => Ok(message_buffer.freeze(n)),
      Err(SerializeError::MessageTooLarge) if handle_too_large => match message.correlation_id() {
        Some(correlation_id) => {
          let error_msg = narwhal_protocol::Error::new(ResponseTooLarge)
            .with_id(correlation_id)
            .with_detail(StringAtom::from("response exceeded maximum message size"));

          Self::serialize_message_inner(&error_msg.into(), message_buffer, false)
        },
        None => Err(anyhow!(SerializeError::MessageTooLarge)),
      },
      Err(e) => Err(e.into()),
    }
  }

  async fn read_payload<B, T>(
    mut buf: B,
    mut delimiter_buf: Box<[u8; 1]>,
    stream_reader: &mut StreamReader<LocalStream<T>>,
    len: usize,
    correlation_id: Option<u32>,
  ) -> anyhow::Result<PayloadReadResult<B>>
  where
    B: IoBuf + IoBufMut,
    T: AsyncReadRent + 'static,
  {
    buf = stream_reader.read_raw(buf, len).await?;

    // Read last byte, and ensure it's a newline
    delimiter_buf = stream_reader.read_raw(delimiter_buf, 1).await?;

    if delimiter_buf[0] != b'\n' {
      let mut error = narwhal_protocol::Error::new(BadRequest).with_detail(StringAtom::from("invalid payload format"));
      if let Some(id) = correlation_id {
        error = error.with_id(id);
      }
      return Err(error.into());
    }

    Ok(PayloadReadResult { data: buf, delimiter: delimiter_buf })
  }
}

/// The result of reading a payload and its trailing delimiter.
struct PayloadReadResult<B> {
  /// The payload data buffer.
  data: B,

  /// The delimiter buffer.
  delimiter: Box<[u8; 1]>,
}

/// The connection transmitter.
#[derive(Clone, Debug)]
pub struct ConnTx {
  /// The send message channel.
  send_msg_tx: Sender<(Message, Option<PoolBuffer>)>,

  /// The connection close channel.
  close_tx: Sender<Message>,
}

// ===== impl ConnTx =====

impl ConnTx {
  /// Creates a new connection transmitter.
  ///
  /// # Arguments
  ///
  /// * `send_msg_tx` - The send message channel.
  /// * `close_tx` - The connection close channel.
  pub fn new(send_msg_tx: Sender<(Message, Option<PoolBuffer>)>, close_tx: Sender<Message>) -> Self {
    Self { send_msg_tx, close_tx }
  }

  /// Sends a message without a payload to the connection.
  ///
  /// # Arguments
  ///
  /// * `message` - The message to send.
  pub fn send_message(&self, message: Message) {
    self.send_message_with_payload(message, None);
  }

  /// Sends a message with an optional payload to the connection.
  ///
  /// # Arguments
  ///
  /// * `message` - The message to send.
  /// * `payload_opt` - An optional payload to send with the message.
  ///
  /// # Panics
  ///
  /// Panics if the message is not a `Message::Message` or `Message::S2mForwardPayloadAck`.
  pub fn send_message_with_payload(&self, message: Message, payload_opt: Option<PoolBuffer>) {
    assert!(
      payload_opt.is_none()
        || matches!(
          message,
          Message::Message { .. } | Message::ModDirect { .. } | Message::S2mForwardBroadcastPayloadAck { .. }
        ),
      "a Message::Message, Message::ModDirect or Message::S2mForwardBroadcastPayloadAck variant is expected when payload is present"
    );

    match self.send_msg_tx.try_send((message, payload_opt)) {
      Ok(_) => {},
      Err(_) => {
        // The send channel is full, so most likely the client is either not reading
        // or is too slow to process incoming messages. In this case, we close the connection.
        self.close(Message::Error(ErrorParameters { id: None, reason: OutboundQueueIsFull.into(), detail: None }));
      },
    }
  }

  /// Closes the connection.
  ///
  /// # Arguments
  ///
  /// * `message` - The message to send to the connection before closing.
  ///
  /// # Panics
  ///
  /// Panics if the message is not a `Message::Error`.
  pub fn close(&self, message: Message) {
    assert!(matches!(message, Message::Error { .. }), "a Message::Error message is expected");
    let _ = self.close_tx.try_send(message);
  }
}
