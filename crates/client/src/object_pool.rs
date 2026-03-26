// SPDX-License-Identifier: BSD-3-Clause

use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use async_lock::{Semaphore, SemaphoreGuardArc};
use futures::FutureExt;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// Errors returned when recycling a pooled object.
pub enum RecycleError<E> {
  /// The underlying backend reported an error.
  Backend(E),
}

/// Convenience alias for recycle results.
pub type RecycleResult<E> = Result<(), RecycleError<E>>;

/// Errors returned by [`ObjectPool::get`].
#[derive(Debug)]
pub enum ObjectPoolError<E> {
  /// The pool has been closed.
  Closed,

  /// The manager's `create` method returned an error.
  Backend(E),
}

impl<E: fmt::Display> fmt::Display for ObjectPoolError<E> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ObjectPoolError::Closed => write!(f, "object pool is closed"),
      ObjectPoolError::Backend(e) => write!(f, "backend error: {}", e),
    }
  }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for ObjectPoolError<E> {}

/// Trait for managing pooled objects.
///
/// Implementations are responsible for creating, health-checking (recycling),
/// and cleaning up objects.
///
/// Both the manager and its managed type must be `Send + Sync` because the
/// pool can be shared across OS threads.
pub trait Manager: Send + Sync + 'static {
  /// The type of object being managed (e.g. a connection).
  type Type: Send;

  /// The error type returned by [`Manager::create`] and [`Manager::recycle`].
  type Error: fmt::Debug + fmt::Display + Send + 'static;

  /// Creates a new managed object.
  ///
  /// Called when the pool needs a fresh object.  The future executes on
  /// the caller's async runtime.
  fn create(&self) -> impl Future<Output = Result<Self::Type, Self::Error>>;

  /// Checks whether an existing object is still healthy.
  ///
  /// Called lazily when an idle object is retrieved from the pool via
  /// [`ObjectPool::get`].  Return `Ok(())` to keep the object, or
  /// `Err(RecycleError)` to discard it.
  fn recycle(&self, obj: &mut Self::Type) -> impl Future<Output = RecycleResult<Self::Error>>;

  /// Synchronously cleans up an object that is being permanently removed
  /// from the pool (e.g. because it failed recycling or the pool is closing).
  ///
  /// Called from synchronous contexts (including [`Drop`]).
  /// Implementations that require async cleanup should signal cancellation
  /// (e.g. via a `CancellationToken`) and let background tasks finish on
  /// their own.
  fn detach(&self, obj: &mut Self::Type);
}

/// Mutable state protected by a [`Mutex`].
struct ObjectPoolState<M: Manager> {
  /// Idle (available) objects ready to be handed out.
  idle: Vec<M::Type>,

  /// Whether the pool has been closed.
  closed: bool,
}

/// Shared, immutable configuration plus the mutex-protected state.
struct ObjectPoolInner<M: Manager> {
  /// The manager responsible for creating / recycling / detaching objects.
  manager: M,

  /// Mutable pool state.
  state: Mutex<ObjectPoolState<M>>,

  /// Semaphore with `max_size` permits — gates how many objects may be
  /// checked out simultaneously.  Callers of [`ObjectPool::get`] acquire a
  /// permit; the permit is released when the [`ObjectPoolObject`] is dropped.
  semaphore: Arc<Semaphore>,

  /// Signalled by [`ObjectPool::close`] to immediately wake any callers
  /// blocked in [`ObjectPool::get`].
  close_token: CancellationToken,
}

impl<M: Manager> ObjectPoolInner<M> {
  /// Returns an idle object to the pool, or detaches it when the pool is
  /// closed.
  ///
  /// This is called from [`ObjectPoolObject::drop`] *before* the semaphore
  /// permit is released, so the next waiter will find the object in the
  /// idle list.
  fn return_object(&self, mut obj: M::Type) {
    let should_detach = {
      let mut state = self.state.lock();
      if state.closed {
        true
      } else {
        state.idle.push(obj);
        return;
      }
    };

    if should_detach {
      self.manager.detach(&mut obj);
    }
  }
}

/// Builder for constructing an [`ObjectPool`].
///
/// # Example
///
/// ```ignore
/// let pool = ObjectPool::builder(my_manager)
///     .max_size(8)
///     .build()?;
/// ```
pub struct ObjectPoolBuilder<M: Manager> {
  manager: M,
  max_size: usize,
}

// === impl ObjectPoolBuilder ===

impl<M: Manager> ObjectPoolBuilder<M> {
  /// Sets the maximum number of managed objects in the pool.
  ///
  /// Defaults to `16` if not explicitly set.
  pub fn max_size(mut self, size: usize) -> Self {
    self.max_size = size;
    self
  }

  /// Consumes the builder and returns an [`ObjectPool`].
  ///
  /// # Errors
  ///
  /// Returns [`ObjectPoolBuildError::InvalidMaxSize`] when `max_size` is zero.
  pub fn build(self) -> Result<ObjectPool<M>, ObjectPoolBuildError> {
    if self.max_size == 0 {
      return Err(ObjectPoolBuildError::InvalidMaxSize);
    }

    let inner = Arc::new(ObjectPoolInner {
      manager: self.manager,
      state: Mutex::new(ObjectPoolState { idle: Vec::with_capacity(self.max_size), closed: false }),
      semaphore: Arc::new(Semaphore::new(self.max_size)),
      close_token: CancellationToken::new(),
    });

    Ok(ObjectPool { inner })
  }
}

/// Error returned when building a pool with invalid configuration.
#[derive(Debug)]
pub enum ObjectPoolBuildError {
  /// `max_size` was set to zero.
  InvalidMaxSize,
}

impl fmt::Display for ObjectPoolBuildError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ObjectPoolBuildError::InvalidMaxSize => write!(f, "max_size must be greater than 0"),
    }
  }
}

impl std::error::Error for ObjectPoolBuildError {}

/// A runtime-agnostic managed object pool.
///
/// The pool is `Send + Sync` and cheaply cloneable (via [`Arc`]).
/// Objects are lazily created and health-checked on retrieval.
///
/// When all objects are in use, [`ObjectPool::get`] **waits** until a
/// slot becomes available.
pub struct ObjectPool<M: Manager> {
  inner: Arc<ObjectPoolInner<M>>,
}

// === impl ObjectPool ===

impl<M: Manager> Clone for ObjectPool<M> {
  fn clone(&self) -> Self {
    ObjectPool { inner: Arc::clone(&self.inner) }
  }
}

impl<M: Manager> ObjectPool<M> {
  /// Creates a new [`ObjectPoolBuilder`] with the given manager.
  pub fn builder(manager: M) -> ObjectPoolBuilder<M> {
    ObjectPoolBuilder { manager, max_size: 16 }
  }

  /// Retrieves an object from the pool, **waiting** if all objects are
  /// currently checked out.
  ///
  /// 1. Acquires a semaphore permit (blocks if `max_size` objects are
  ///    already checked out).
  /// 2. Pops idle objects and health-checks them (discarding unhealthy
  ///    ones).
  /// 3. If no healthy idle object is found, creates a new one via
  ///    [`Manager::create`].
  /// 4. Returns the object wrapped in an [`ObjectPoolObject`]; the permit
  ///    is released when the wrapper is dropped.
  ///
  /// Callers that need a timeout should wrap this call with their
  /// runtime's timeout primitive (e.g. `tokio::time::timeout`).
  pub async fn get(&self) -> Result<ObjectPoolObject<M>, ObjectPoolError<M::Error>> {
    // Early check.
    if self.inner.state.lock().closed {
      return Err(ObjectPoolError::Closed);
    }

    // Wait for a slot.  Race the semaphore against the close signal so
    // that `close()` can wake blocked callers immediately.
    let permit = {
      let mut sem_fut = std::pin::pin!(self.inner.semaphore.acquire_arc().fuse());
      let mut close_fut = std::pin::pin!(self.inner.close_token.cancelled().fuse());

      futures::select! {
        p = sem_fut => p,
        _ = close_fut => return Err(ObjectPoolError::Closed),
      }
    };

    // Double-check after waking — the pool may have been closed while we
    // were waiting for the permit.
    if self.inner.state.lock().closed {
      // Permit is dropped here, releasing the slot.
      return Err(ObjectPoolError::Closed);
    }

    // Try to reuse a healthy idle object.
    loop {
      let idle_obj = self.inner.state.lock().idle.pop();
      match idle_obj {
        Some(mut obj) => match self.inner.manager.recycle(&mut obj).await {
          Ok(()) => {
            return Ok(ObjectPoolObject { inner: Some(obj), pool: Arc::clone(&self.inner), _permit: permit });
          },
          Err(_) => {
            // Unhealthy — detach and try the next idle object.
            self.inner.manager.detach(&mut obj);
            continue;
          },
        },
        None => break,
      }
    }

    // Create a new object.
    match self.inner.manager.create().await {
      Ok(obj) => Ok(ObjectPoolObject { inner: Some(obj), pool: Arc::clone(&self.inner), _permit: permit }),
      Err(e) => Err(ObjectPoolError::Backend(e)), // permit dropped
    }
  }

  /// Retains only the idle objects for which the predicate returns `true`.
  ///
  /// Objects that are currently checked out (held as
  /// [`ObjectPoolObject`]s) are unaffected.  Removed objects are
  /// detached via [`Manager::detach`].
  pub fn retain(&self, mut predicate: impl FnMut(&M::Type) -> bool) {
    let removed: Vec<M::Type> = {
      let mut state = self.inner.state.lock();
      let old_idle = std::mem::take(&mut state.idle);
      let mut kept = Vec::with_capacity(old_idle.len());
      let mut removed = Vec::new();

      for obj in old_idle {
        if predicate(&obj) {
          kept.push(obj);
        } else {
          removed.push(obj);
        }
      }

      state.idle = kept;
      removed
    };

    // Detach outside the lock.
    for mut obj in removed {
      self.inner.manager.detach(&mut obj);
    }
  }

  /// Closes the pool, preventing new objects from being handed out.
  ///
  /// All currently idle objects are drained and detached.  Callers
  /// blocked in [`ObjectPool::get`] are woken immediately and receive
  /// [`ObjectPoolError::Closed`].  Objects that are still checked out
  /// will be detached when their [`ObjectPoolObject`] wrappers are dropped.
  pub fn close(&self) {
    let drained: Vec<M::Type> = {
      let mut state = self.inner.state.lock();
      state.closed = true;
      state.idle.drain(..).collect()
    };

    // Detach outside the lock.
    for mut obj in drained {
      self.inner.manager.detach(&mut obj);
    }

    // Wake all callers currently blocked in `get()`.
    self.inner.close_token.cancel();
  }
}

/// A smart-pointer wrapper around a pooled object.
///
/// Dereferences to the inner managed type.  When dropped the object is
/// returned to the pool's idle list (or detached if the pool is closed) and
/// the semaphore permit is released, potentially waking a caller blocked in
/// [`ObjectPool::get`].
///
/// # Drop order
///
/// The fields are declared so that Rust's automatic field-drop ordering
/// guarantees the object is returned to the idle list **before** the
/// semaphore permit is released.  This ensures the next waiter always finds
/// the object in the idle list.
pub struct ObjectPoolObject<M: Manager> {
  // Dropped first — taken in the manual `Drop` impl.
  inner: Option<M::Type>,

  // Dropped second — `return_object` is called via this Arc.
  pool: Arc<ObjectPoolInner<M>>,

  // Dropped last — releasing the semaphore permit.
  _permit: SemaphoreGuardArc,
}

// === impl ObjectPoolObject ===

impl<M: Manager> Deref for ObjectPoolObject<M> {
  type Target = M::Type;

  fn deref(&self) -> &Self::Target {
    self.inner.as_ref().expect("ObjectPoolObject used after take")
  }
}

impl<M: Manager> DerefMut for ObjectPoolObject<M> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    self.inner.as_mut().expect("ObjectPoolObject used after take")
  }
}

impl<M: Manager> Drop for ObjectPoolObject<M> {
  fn drop(&mut self) {
    if let Some(obj) = self.inner.take() {
      // Return the object *before* `_permit` is dropped so the next
      // waiter will find it in the idle list.
      self.pool.return_object(obj);
    }
    // After this body returns, Rust drops the remaining fields in
    // declaration order: `pool` (Arc ref-count decrement), then
    // `_permit` (semaphore release → a blocked `get()` may wake).
  }
}

impl<M: Manager> fmt::Debug for ObjectPoolObject<M>
where
  M::Type: fmt::Debug,
{
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ObjectPoolObject").field("inner", &self.inner).finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
  use std::time::Duration;

  struct TestManager {
    create_count: AtomicUsize,
    fail_create: AtomicBool,
    fail_recycle: AtomicBool,
  }

  #[derive(Debug)]
  struct TestError(&'static str);

  impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      write!(f, "{}", self.0)
    }
  }

  impl std::error::Error for TestError {}

  struct TestObj {
    id: usize,
    healthy: AtomicBool,
  }

  impl Manager for TestManager {
    type Type = TestObj;
    type Error = TestError;

    async fn create(&self) -> Result<TestObj, TestError> {
      if self.fail_create.load(Ordering::Relaxed) {
        return Err(TestError("create failed"));
      }
      let id = self.create_count.fetch_add(1, Ordering::Relaxed);
      Ok(TestObj { id, healthy: AtomicBool::new(true) })
    }

    async fn recycle(&self, obj: &mut TestObj) -> RecycleResult<TestError> {
      if self.fail_recycle.load(Ordering::Relaxed) || !obj.healthy.load(Ordering::Relaxed) {
        return Err(RecycleError::Backend(TestError("recycle failed")));
      }
      Ok(())
    }

    fn detach(&self, _obj: &mut TestObj) {
      // no-op in tests
    }
  }

  fn test_manager() -> TestManager {
    TestManager {
      create_count: AtomicUsize::new(0),
      fail_create: AtomicBool::new(false),
      fail_recycle: AtomicBool::new(false),
    }
  }

  #[compio::test]
  async fn test_basic_get_and_return() {
    let pool = ObjectPool::builder(test_manager()).max_size(2).build().unwrap();

    let obj = pool.get().await.unwrap();
    assert_eq!(obj.id, 0);
    drop(obj);

    // Should reuse the same object.
    let obj = pool.get().await.unwrap();
    assert_eq!(obj.id, 0);
  }

  #[compio::test]
  async fn test_creates_up_to_max_size() {
    let pool = ObjectPool::builder(test_manager()).max_size(2).build().unwrap();

    let obj1 = pool.get().await.unwrap();
    let obj2 = pool.get().await.unwrap();
    assert_eq!(obj1.id, 0);
    assert_eq!(obj2.id, 1);

    drop(obj1);
    drop(obj2);
  }

  #[compio::test]
  async fn test_waits_when_full() {
    let pool = ObjectPool::builder(test_manager()).max_size(1).build().unwrap();

    let obj = pool.get().await.unwrap();
    assert_eq!(obj.id, 0);

    // Pool is at capacity (1/1).  Spawn a task that will block on get().
    let pool2 = pool.clone();
    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();

    let _handle = compio::runtime::spawn(async move {
      let obj = pool2.get().await.unwrap();
      // Should receive the recycled object from the first get().
      assert_eq!(obj.id, 0);
      done2.store(true, Ordering::SeqCst);
    });

    // Yield so the spawned task has a chance to reach the semaphore wait.
    compio::runtime::time::sleep(Duration::from_millis(50)).await;
    assert!(!done.load(Ordering::SeqCst), "spawned task should still be blocked");

    // Return the object — releases the permit and wakes the waiter.
    drop(obj);

    // Yield so the spawned task can complete.
    compio::runtime::time::sleep(Duration::from_millis(50)).await;
    assert!(done.load(Ordering::SeqCst), "spawned task should have completed");
  }

  #[compio::test]
  async fn test_unhealthy_object_discarded() {
    let pool = ObjectPool::builder(test_manager()).max_size(2).build().unwrap();

    let obj = pool.get().await.unwrap();
    obj.healthy.store(false, Ordering::Relaxed);
    drop(obj);

    // The unhealthy object should be discarded on get(), a new one created.
    let obj = pool.get().await.unwrap();
    assert_eq!(obj.id, 1);
  }

  #[compio::test]
  async fn test_retain_filters_idle() {
    let pool = ObjectPool::builder(test_manager()).max_size(4).build().unwrap();

    let o1 = pool.get().await.unwrap();
    let o2 = pool.get().await.unwrap();

    o2.healthy.store(false, Ordering::Relaxed);

    drop(o1);
    drop(o2);

    pool.retain(|obj| obj.healthy.load(Ordering::Relaxed));

    let obj = pool.get().await.unwrap();
    // obj with id=0 was healthy and retained; id=1 was discarded.
    assert_eq!(obj.id, 0);
  }

  #[compio::test]
  async fn test_close_prevents_get() {
    let pool = ObjectPool::builder(test_manager()).max_size(2).build().unwrap();

    pool.close();

    let err = pool.get().await;
    assert!(matches!(err, Err(ObjectPoolError::Closed)));
  }

  #[compio::test]
  async fn test_close_wakes_blocked_waiters() {
    let pool = ObjectPool::builder(test_manager()).max_size(1).build().unwrap();

    // Check out the only slot.
    let obj = pool.get().await.unwrap();

    // Spawn a task that will block waiting for an object.
    let pool2 = pool.clone();
    let got_closed = Arc::new(AtomicBool::new(false));
    let got_closed2 = got_closed.clone();

    let _handle = compio::runtime::spawn(async move {
      let result = pool2.get().await;
      if matches!(result, Err(ObjectPoolError::Closed)) {
        got_closed2.store(true, Ordering::SeqCst);
      }
    });

    // Yield so the spawned task reaches the semaphore wait.
    compio::runtime::time::sleep(Duration::from_millis(50)).await;
    assert!(!got_closed.load(Ordering::SeqCst));

    // Close the pool — should immediately wake the blocked waiter.
    pool.close();

    // Yield so the spawned task can observe the close signal.
    compio::runtime::time::sleep(Duration::from_millis(50)).await;
    assert!(got_closed.load(Ordering::SeqCst), "blocked waiter should have received Closed");

    drop(obj);
  }

  #[compio::test]
  async fn test_create_failure_releases_slot() {
    let mgr = test_manager();
    mgr.fail_create.store(true, Ordering::Relaxed);
    let pool = ObjectPool::builder(mgr).max_size(1).build().unwrap();

    let err = pool.get().await;
    assert!(matches!(err, Err(ObjectPoolError::Backend(_))));

    // Slot should have been released — we can try again without blocking.
    let err = pool.get().await;
    assert!(matches!(err, Err(ObjectPoolError::Backend(_))));
  }

  #[test]
  fn test_zero_max_size_rejected() {
    let err = ObjectPool::builder(test_manager()).max_size(0).build();
    assert!(err.is_err());
  }
}
