// SPDX-License-Identifier: BSD-3-Clause

use std::fmt;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use bytes::BytesMut;
use crossbeam_queue::ArrayQueue;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A generic buffer pool that allows sharing buffers across threads.
#[derive(Clone)]
pub struct Pool(Arc<PoolInner>);

/// The inner state of a pool.
struct PoolInner {
  /// Queue of available buffers.
  available: ArrayQueue<BytesMut>,

  /// Semaphore for limiting the number of in-use buffers.
  sem: Arc<Semaphore>,

  /// Size of each buffer.
  buffer_size: usize,
}

// === impl Pool ===

impl Pool {
  /// Create a new buffer pool with the specified number of buffers and buffer size
  pub fn new(count: usize, buffer_size: usize) -> Self {
    Self::from_buffer(count, buffer_size, BytesMut::zeroed(buffer_size * count))
  }

  /// Creates a new buffer pool from a pre-allocated BytesMut buffer.
  fn from_buffer(count: usize, buffer_size: usize, mut buffer: BytesMut) -> Self {
    assert!(buffer_size * count <= buffer.len(), "buffer size is too small");

    let available = ArrayQueue::new(count);
    for _ in 0..count {
      available.force_push(buffer.split_to(buffer_size));
    }
    let sem = Semaphore::new(count);

    let inner = PoolInner { available, sem: Arc::new(sem), buffer_size };

    Self(Arc::new(inner))
  }

  /// Get a mutable buffer from the pool.
  pub async fn acquire_buffer(&self) -> MutablePoolBuffer {
    let permit = self.0.sem.clone().acquire_owned().await.unwrap();
    let buffer = self.0.available.pop().unwrap();

    MutablePoolBuffer(PoolBufferInner { data: Some(buffer), pool: Some(self.0.clone()), permit: Some(permit) })
  }

  /// Release multiple buffers back to the pool.
  pub fn release_buffers(&self, buffers: &mut Vec<PoolBuffer>) {
    let mut n = 0;
    for buffer in buffers.drain(..) {
      if let Some(bytes_mut) = buffer.try_into_bytes_mut() {
        self.0.available.force_push(bytes_mut);
        n += 1;
      }
    }
    self.0.sem.add_permits(n);
  }

  /// Get the number of buffers currently in use.
  pub fn in_use_count(&self) -> usize {
    self.0.available.capacity() - self.0.available.len()
  }

  /// Get the number of available buffers.
  pub fn available_count(&self) -> usize {
    self.0.available.len()
  }

  /// Check if there are any available buffers.
  pub fn has_available(&self) -> bool {
    self.available_count() > 0
  }

  /// Get the size of the buffers in the pool.
  pub fn buffer_size(&self) -> usize {
    self.0.buffer_size
  }

  /// Get the total number of bytes capacity in the pool.
  pub fn bytes_capacity(&self) -> usize {
    self.0.available.capacity() * self.0.buffer_size
  }
}

impl std::fmt::Debug for Pool {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Pool")
      .field("in_use_count", &self.in_use_count())
      .field("available_count", &self.available_count())
      .field("buffer_size", &self.buffer_size())
      .finish()
  }
}

/// A pool that manages multiple buckets of different sized buffers
#[derive(Clone)]
pub struct BucketedPool {
  buckets: Arc<[Pool]>,
}

// === impl BucketedPool ===

impl BucketedPool {
  /// Creates a new bucketed pool with a memory budget constraint.
  ///
  /// This constructor allocates multiple buffer pools (buckets) of varying sizes within
  /// a total memory budget. It uses a top-down allocation strategy that prioritizes
  /// larger buffers first, as they can serve as "universal donors" for smaller buffer
  /// requests.
  ///
  /// # Parameters
  ///
  /// * `min_buffer_size` - Minimum buffer size in bytes for the smallest bucket.
  /// * `max_buffer_size` - Maximum buffer size in bytes for the largest bucket.
  /// * `memory_budget` - Total memory budget in bytes to distribute across all buckets.
  /// * `max_buffers_per_bucket` - Hard cap on the number of buffers per bucket.
  /// * `growth_factor` - Multiplier for generating bucket sizes (e.g., 2 for powers of 2).
  /// * `decay_factor` - Budget decay factor (0.0..1.0) applied when allocating from
  ///   largest to smallest buckets. Higher values allocate more to larger buckets.
  ///
  /// # Allocation Strategy
  ///
  /// The method uses a top-down allocation approach:
  /// 1. Generates bucket sizes from `min_buffer_size` to `max_buffer_size` using `growth_factor`
  /// 2. Iterates from largest to smallest, allocating `remaining_budget * decay_factor` to each
  /// 3. Applies `max_buffers_per_bucket` cap to prevent over-allocation
  /// 4. Any budget saved by hitting the cap flows down to smaller buckets
  /// 5. The smallest bucket receives all remaining budget
  pub fn new_with_memory_budget(
    min_buffer_size: usize,
    max_buffer_size: usize,
    memory_budget: usize,
    max_buffers_per_bucket: usize,
    growth_factor: usize,
    decay_factor: f64,
  ) -> Self {
    assert!(min_buffer_size > 0, "minimum buffer size must be greater than 0");
    assert!(max_buffer_size > 0, "maximum buffer size must be greater than 0");
    assert!(min_buffer_size <= max_buffer_size, "min size > max size");
    assert!(growth_factor > 1, "growth factor must be > 1");
    assert!(memory_budget > 0, "memory budget must be > 0");
    assert!(decay_factor > 0.0 && decay_factor < 1.0, "decay factor must be 0.0..1.0");

    // Generate all bucket sizes first (Small -> Large)
    // We do this first so we can iterate them in reverse later.
    let mut sizes = Vec::new();
    let mut current_size = min_buffer_size;
    while current_size <= max_buffer_size {
      sizes.push(current_size);
      current_size *= growth_factor;
    }

    // Top-Down Allocation (Iterate Largest -> Smallest)
    // We use reverse iterator to prioritize allocating the "Universal Donors" (large buckets).
    let mut bucket_configs = Vec::new();
    let mut remaining_budget = memory_budget;

    for (i, &size) in sizes.iter().rev().enumerate() {
      let is_last_bucket = i == sizes.len() - 1;

      // Calculate the budget target for this specific bucket tier
      let bucket_budget_target = if is_last_bucket {
        // The smallest bucket gets whatever budget is left (Sweep the floor)
        remaining_budget
      } else {
        ((remaining_budget as f64) * decay_factor).floor() as usize
      };

      let bucket_budget_target = bucket_budget_target.min(remaining_budget);

      // Calculate buffer count
      let raw_count = bucket_budget_target / size;

      // Apply the hard cap (max_buffers_per_bucket)
      let actual_count = raw_count.min(max_buffers_per_bucket);

      if actual_count > 0 {
        bucket_configs.push((actual_count, size));

        // We only deduct what we *actually* spent.
        // Any budget saved by hitting 'max_buffers_per_bucket' stays in 'remaining_budget'
        // and is naturally available for the next (smaller) bucket in the loop.
        remaining_budget -= actual_count * size;
      }
    }

    // Sort configurations back to Ascending Order (Small -> Large)
    bucket_configs.sort_by_key(|&(_, size)| size);

    // We sum up the exact requirements to ensure we allocate exactly what is needed.
    let actual_total_memory = bucket_configs.iter().map(|(count, size)| count * size).sum::<usize>();

    let mut master_buffer = BytesMut::zeroed(actual_total_memory);

    let mut buckets = Vec::with_capacity(bucket_configs.len());
    for (count, size) in bucket_configs {
      let bucket_total_size = count * size;

      let bucket_buffer = master_buffer.split_to(bucket_total_size);

      buckets.push(Pool::from_buffer(count, size, bucket_buffer));
    }

    Self { buckets: Arc::from(buckets) }
  }

  /// Acquire a mutable buffer of at least the requested size
  ///
  /// Returns a buffer that can accommodate the requested size,
  /// or None if requested size is larger than the largest bucket.
  ///
  /// # Arguments
  ///
  /// * `size` - The minimum size of the buffer to acquire
  pub async fn acquire_buffer(&self, size: usize) -> Option<MutablePoolBuffer> {
    let mut largest_suitable_bucket: Option<&Pool> = None;

    for bucket in self.buckets.iter() {
      if bucket.buffer_size() >= size {
        if bucket.has_available() {
          return Some(bucket.acquire_buffer().await);
        } else {
          // Keep a reference to the largest bucket that can accommodate the requested size.
          largest_suitable_bucket = Some(bucket);
        }
      }
    }

    // If no buckets have available buffers, we'll block on the largest suitable bucket,
    // waiting for a buffer to be returned to the pool.
    if let Some(bucket) = largest_suitable_bucket {
      return Some(bucket.acquire_buffer().await);
    }

    None
  }

  /// Get the total number of buffers currently in use across all buckets
  pub fn total_in_use_count(&self) -> usize {
    self.buckets.iter().map(|pool| pool.in_use_count()).sum()
  }

  /// Get the total number of available buffers across all buckets
  pub fn total_available_count(&self) -> usize {
    self.buckets.iter().map(|pool| pool.available_count()).sum()
  }

  /// Get the total number of bytes capacity across all buckets
  pub fn total_bytes_capacity(&self) -> usize {
    self.buckets.iter().map(|pool| pool.bytes_capacity()).sum()
  }
}

impl std::fmt::Debug for BucketedPool {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("BucketedPool")
      .field("num_buckets", &self.buckets.len())
      .field("total_in_use", &self.total_in_use_count())
      .field("total_available", &self.total_available_count())
      .finish()
  }
}

/// The inner state of a pool buffer.
struct PoolBufferInner {
  /// The actual buffer data.
  data: Option<BytesMut>,

  /// Reference to the pool for returning the buffer.
  pool: Option<Arc<PoolInner>>,

  /// Semaphore permit for the buffer.
  permit: Option<OwnedSemaphorePermit>,
}

impl Debug for PoolBufferInner {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.debug_struct("PoolBufferInner").field("data", &self.data.as_ref().map(|d| d.len())).finish()
  }
}

// === impl PoolBufferInner ===

impl PoolBufferInner {
  /// Get a read-only reference to the buffer data.
  fn as_slice(&self) -> &[u8] {
    assert!(self.data.is_some());
    let data = self.data.as_ref().unwrap();
    data.deref()
  }

  /// Get a mutable reference to the buffer data.
  fn as_mut_slice(&mut self) -> &mut [u8] {
    assert!(self.data.is_some());
    let data = self.data.as_mut().unwrap();
    data.deref_mut()
  }

  /// Return the length of the buffer.
  fn len(&self) -> usize {
    assert!(self.data.is_some());
    let data = self.data.as_ref().unwrap();
    data.len()
  }
}

impl Drop for PoolBufferInner {
  fn drop(&mut self) {
    if let (Some(mut buffer), Some(pool)) = (self.data.take(), self.pool.take()) {
      unsafe { buffer.set_len(pool.buffer_size) };
      pool.available.force_push(buffer);
    }
  }
}

/// A mutable buffer from the pool.
/// This buffer can be converted into a shared read-only buffer.
#[derive(Debug)]
pub struct MutablePoolBuffer(PoolBufferInner);

// === impl MutablePoolBuffer ===

impl MutablePoolBuffer {
  /// Get a read-only reference to the buffer data.
  #[inline]
  pub fn as_slice(&self) -> &[u8] {
    self.0.as_slice()
  }

  /// Get a mutable reference to the buffer data.
  #[inline]
  pub fn as_mut_slice(&mut self) -> &mut [u8] {
    self.0.as_mut_slice()
  }

  /// Return the length of the buffer.
  #[inline]
  #[allow(clippy::len_without_is_empty)]
  pub fn len(&self) -> usize {
    self.0.len()
  }

  /// Convert a mutable buffer into a shareable read-only buffer.
  pub fn freeze(&mut self, size: usize) -> PoolBuffer {
    let data = self.0.data.take();
    let pool = self.0.pool.take();
    let permit = self.0.permit.take();

    let inner = PoolBufferInner { data, pool, permit };

    PoolBuffer { inner: Arc::new(inner), len: size }
  }
}

impl Deref for MutablePoolBuffer {
  type Target = BytesMut;

  fn deref(&self) -> &Self::Target {
    debug_assert!(self.0.data.is_some(), "MutablePoolBuffer has been consumed");
    self.0.data.as_ref().unwrap()
  }
}

impl DerefMut for MutablePoolBuffer {
  fn deref_mut(&mut self) -> &mut Self::Target {
    debug_assert!(self.0.data.is_some(), "MutablePoolBuffer has been consumed");
    self.0.data.as_mut().unwrap()
  }
}

impl compio::buf::IoBuf for MutablePoolBuffer {
  fn as_init(&self) -> &[u8] {
    let data = self.0.data.as_ref().unwrap();
    &data[..data.len()]
  }
}

impl compio::buf::SetLen for MutablePoolBuffer {
  unsafe fn set_len(&mut self, len: usize) {
    unsafe {
      self.0.data.as_mut().unwrap().set_len(len);
    }
  }
}

impl compio::buf::IoBufMut for MutablePoolBuffer {
  fn as_uninit(&mut self) -> &mut [std::mem::MaybeUninit<u8>] {
    let data = self.0.data.as_mut().unwrap();
    let cap = data.capacity();
    // SAFETY: BytesMut capacity memory is allocated and valid for MaybeUninit<u8>
    unsafe { std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut std::mem::MaybeUninit<u8>, cap) }
  }
}

/// A read-only buffer from the pool.
#[derive(Clone, Debug)]
pub struct PoolBuffer {
  /// The inner state of the buffer.
  inner: Arc<PoolBufferInner>,

  /// The length of the buffer.
  len: usize,
}

// === impl PoolBuffer ===

impl PoolBuffer {
  /// Get a read-only reference to the buffer data.
  #[inline]
  pub fn as_slice(&self) -> &[u8] {
    &self.inner.as_slice()[..self.len]
  }

  /// Get the length of the buffer.
  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }

  /// Tells whether the buffer is empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// Attempts to unwrap the underlying `BytesMut` from the pool buffer.
  fn try_into_bytes_mut(self) -> Option<BytesMut> {
    match Arc::try_unwrap(self.inner) {
      Ok(mut inner) => {
        if let Some(permit) = inner.permit.take() {
          permit.forget();
        }
        inner.data.take()
      },
      Err(_) => None,
    }
  }
}

impl Deref for PoolBuffer {
  type Target = [u8];

  fn deref(&self) -> &Self::Target {
    self.as_slice()
  }
}

impl compio::buf::IoBuf for PoolBuffer {
  fn as_init(&self) -> &[u8] {
    self.as_slice()
  }
}

#[cfg(test)]
mod pool_tests {
  use super::*;

  use compio::buf::{IoBuf, IoBufMut, SetLen};

  #[compio::test]
  async fn test_buffer_pool() {
    let pool = Pool::new(5, 1024);

    // Check initial state
    assert_eq!(pool.in_use_count(), 0);
    assert_eq!(pool.available_count(), 5);

    // Get some buffers
    let buf1 = pool.acquire_buffer().await;
    let buf2 = pool.acquire_buffer().await;

    assert_eq!(pool.in_use_count(), 2);
    assert_eq!(pool.available_count(), 3);

    // Return a buffer
    drop(buf1);

    assert_eq!(pool.in_use_count(), 1);
    assert_eq!(pool.available_count(), 4);

    drop(buf2);
  }

  #[compio::test]
  async fn test_io_buf_mut_conformance() {
    let pool = Pool::new(1, 1024);
    let mut buf = pool.acquire_buffer().await;

    assert_eq!(buf.buf_capacity(), 1024);

    buf.clear();
    assert_eq!(buf.as_init().len(), 0);
    assert_eq!(buf.buf_capacity(), 1024);

    unsafe {
      buf.set_len(10);
    }
    assert_eq!(buf.as_init().len(), 10);

    unsafe {
      let ptr = buf.as_uninit().as_mut_ptr();
      (*ptr) = std::mem::MaybeUninit::new(42);
    }

    // Verify deref works - we should be able to use BytesMut methods
    buf.clear();
    assert_eq!(buf.len(), 0);
  }

  #[compio::test]
  async fn test_shared_buffer_conversion() {
    let pool = Pool::new(1, 1024);
    let mut buf = pool.acquire_buffer().await;

    // Write some data
    buf.as_mut_slice()[0] = 42;

    // Convert to shared buffer
    let shared_buf = buf.freeze(100);

    // Verify data is preserved
    assert_eq!(shared_buf.as_slice()[0], 42);
    assert_eq!(shared_buf.len(), 100);

    // Original buffer should be consumed (freeze consumes it)
    // Note: buf is moved by freeze(), so we can't access it here
  }

  #[compio::test]
  async fn test_multithreaded() {
    use std::sync::Arc;
    use std::thread;

    let pool = Arc::new(Pool::new(10, 1024));
    let mut handles = vec![];

    for i in 0..10 {
      let pool_clone = pool.clone();
      let handle = thread::spawn(move || {
        crate::runtime_block_on(async move {
          let mut buf = pool_clone.acquire_buffer().await;
          buf.as_mut_slice()[0] = i as u8;
          assert_eq!(buf.as_slice()[0], i as u8);
        });
      });
      handles.push(handle);
    }

    for handle in handles {
      handle.join().unwrap();
    }

    // All buffers should be returned
    assert_eq!(pool.in_use_count(), 0);
    assert_eq!(pool.available_count(), 10);
  }
}

#[cfg(test)]
mod bucketed_pool_tests {
  use super::*;
  use std::thread;

  #[test]
  fn test_bucketed_pool_with_memory_budget_creation() {
    // Test memory budget constructor with 100MB, 10K max buffers, 50% decay
    let pool = BucketedPool::new_with_memory_budget(
      1024,              // 1KB min
      131072,            // 128KB max
      100 * 1024 * 1024, // 100MB budget
      10000,             // max 10K buffers per bucket
      2,                 // 2x growth
      0.5,               // 50% decay
    );

    // Should create multiple buckets with exponential decay distribution
    assert!(pool.buckets.len() > 1);

    // Total memory used should not exceed budget
    let mut total_memory = 0;
    for bucket in pool.buckets.iter() {
      total_memory += bucket.available_count() * bucket.buffer_size();
    }
    assert!(total_memory <= 100 * 1024 * 1024);
  }

  #[test]
  fn test_memory_budget_distribution() {
    // Test with 10MB budget
    let pool = BucketedPool::new_with_memory_budget(
      4096,             // 4KB min
      65536,            // 64KB max
      10 * 1024 * 1024, // 10MB budget
      1000,             // max 1000 buffers per bucket
      2,                // 2x growth
      1.0 / 3.0,        // 33% decay
    );

    // Calculate actual memory usage
    let mut total_memory = 0;
    for bucket in pool.buckets.iter() {
      total_memory += bucket.available_count() * bucket.buffer_size();
    }

    // Should use most of the budget but not exceed it
    assert!(total_memory <= 10 * 1024 * 1024);
    assert!(total_memory >= 8 * 1024 * 1024); // Should use at least 80% of budget
  }

  #[test]
  fn test_max_buffers_cap() {
    // Test that max_buffers parameter properly caps each bucket
    let pool = BucketedPool::new_with_memory_budget(
      1024,              // 1KB min
      8192,              // 8KB max
      100 * 1024 * 1024, // 100MB budget (very large)
      100,               // max 100 buffers per bucket
      2,                 // 2x growth
      0.5,               // 50% decay
    );

    // No bucket should have more than 100 buffers
    for bucket in pool.buckets.iter() {
      assert!(bucket.available_count() <= 100);
    }
  }

  #[test]
  fn test_small_memory_budget() {
    // Test with small memory budget
    let pool = BucketedPool::new_with_memory_budget(
      4096,       // 4KB min
      16384,      // 16KB max
      100 * 1024, // 100KB budget
      100,        // max 100 buffers per bucket
      2,          // 2x growth
      0.5,        // 50% decay
    );

    // Should still create buckets within budget
    let mut total_memory = 0;
    for bucket in pool.buckets.iter() {
      total_memory += bucket.available_count() * bucket.buffer_size();
    }
    assert!(total_memory <= 100 * 1024);
    assert!(!pool.buckets.is_empty());
  }

  #[test]
  fn test_memory_budget_edge_cases() {
    // Test with aggressive decay
    let pool = BucketedPool::new_with_memory_budget(
      1024,        // 1KB
      8192,        // 8KB
      1024 * 1024, // 1MB budget
      1000,        // max buffers
      2,           // 2x growth
      0.9,         // 90% decay
    );

    // Largest bucket should get most of the budget (top-down allocation)
    let last_idx = pool.buckets.len() - 1;
    let largest_bucket_memory = pool.buckets[last_idx].available_count() * pool.buckets[last_idx].buffer_size();
    assert!(largest_bucket_memory >= 800 * 1024); // At least 80% of budget

    // Test with minimal decay
    let pool = BucketedPool::new_with_memory_budget(
      1024,        // 1KB
      8192,        // 8KB
      1024 * 1024, // 1MB budget
      1000,        // max buffers
      2,           // 2x growth
      0.1,         // 10% decay
    );

    // Budget should be more evenly distributed
    let total_buckets = pool.buckets.len();
    assert!(total_buckets >= 3); // Should have several buckets
  }

  #[test]
  fn test_budget_redistribution_when_capped() {
    // Test that unused budget from capped buckets gets redistributed
    // With 50% decay and 256MB budget, the largest bucket (64KB) would get 128MB = ~2048 buffers
    // By capping at 1000, we force redistribution of ~64MB to smaller buckets
    let pool = BucketedPool::new_with_memory_budget(
      4096,              // 4KB min
      65536,             // 64KB max
      256 * 1024 * 1024, // 256MB budget
      1000,              // max 1000 buffers per bucket (will cap largest bucket)
      2,                 // 2x growth
      0.5,               // 50%
    );

    // Calculate actual memory usage
    let mut total_memory = 0;
    let mut buffer_counts = Vec::new();

    for bucket in pool.buckets.iter() {
      let count = bucket.available_count();
      let size = bucket.buffer_size();
      total_memory += count * size;
      buffer_counts.push((size, count));
    }

    // Largest bucket (64KB, last in array) should be capped at 1000
    let last_idx = buffer_counts.len() - 1;
    assert_eq!(
      buffer_counts[last_idx].1, 1000,
      "Largest bucket should be capped at max_buffers. Buffer counts: {:?}",
      buffer_counts
    );

    // With 1000 buffer cap per bucket, max possible is ~124MB
    // (1000*64KB + 1000*32KB + 1000*16KB + 1000*8KB + 1000*4KB)
    // So we expect around 45-50% of the 256MB budget to be used
    let budget = 256 * 1024 * 1024;
    assert!(
      total_memory >= (budget * 45 / 100),
      "Should use at least 45% of budget. Used: {}MB of {}MB",
      total_memory / 1024 / 1024,
      budget / 1024 / 1024
    );

    // Check that smaller buckets also hit the cap due to redistribution
    // With top-down allocation and the largest bucket capped, remaining budget flows to smaller buckets
    // All buckets should be capped at 1000 since there's plenty of budget
    for (size, count) in &buffer_counts {
      assert_eq!(
        *count,
        1000,
        "All buckets should be capped at max_buffers with this large budget. Bucket {}KB has {} buffers. All counts: {:?}",
        size / 1024,
        count,
        buffer_counts
      );
    }
  }

  #[test]
  #[should_panic(expected = "decay factor must be 0.0..1.0")]
  fn test_invalid_memory_budget_decay_zero() {
    BucketedPool::new_with_memory_budget(1024, 8192, 1024 * 1024, 100, 2, 0.0);
  }

  #[test]
  #[should_panic(expected = "decay factor must be 0.0..1.0")]
  fn test_invalid_memory_budget_decay_one() {
    BucketedPool::new_with_memory_budget(1024, 8192, 1024 * 1024, 100, 2, 1.0);
  }

  #[test]
  #[should_panic(expected = "memory budget must be > 0")]
  fn test_invalid_zero_budget() {
    BucketedPool::new_with_memory_budget(1024, 8192, 0, 100, 2, 0.5);
  }

  #[compio::test]
  async fn test_acquire_appropriate_bucket() {
    // Test that acquire() returns the right bucket size for different requests
    let pool = BucketedPool::new_with_memory_budget(100, 800, 100 * 1024, 100, 2, 0.5);

    // Request 50 bytes - should get from bucket 0 (100 bytes)
    let buf1 = pool.acquire_buffer(50).await.unwrap();
    assert_eq!(buf1.len(), 100);

    // Request 150 bytes - should get from bucket 1 (200 bytes)
    let buf2 = pool.acquire_buffer(150).await.unwrap();
    assert_eq!(buf2.len(), 200);

    // Request 350 bytes - should get from bucket 2 (400 bytes)
    let buf3 = pool.acquire_buffer(350).await.unwrap();
    assert_eq!(buf3.len(), 400);

    // Request 700 bytes - should get from bucket 3 (800 bytes)
    let buf4 = pool.acquire_buffer(700).await.unwrap();
    assert_eq!(buf4.len(), 800);
  }

  #[compio::test]
  async fn test_no_available_for_oversized_requests() {
    // Test that oversized requests return None
    let pool = BucketedPool::new_with_memory_budget(100, 400, 10 * 1024, 100, 2, 0.5);

    // Request 500 bytes - larger than max bucket (400), should return None
    let buf = pool.acquire_buffer(500).await;
    assert!(buf.is_none());

    // Verify pool stats remain unchanged
    let initial_available = pool.total_available_count();
    assert_eq!(pool.total_available_count(), initial_available);
  }

  #[compio::test]
  async fn test_bucket_reuse() {
    // Test that buffers are properly returned to their buckets
    let pool = BucketedPool::new_with_memory_budget(100, 400, 10 * 1024, 100, 2, 0.5);

    let initial_available = pool.total_available_count();
    let initial_in_use = pool.total_in_use_count();

    // Acquire and release buffers
    {
      let _buf1 = pool.acquire_buffer(50).await.unwrap();
      let _buf2 = pool.acquire_buffer(150).await.unwrap();
      let _buf3 = pool.acquire_buffer(350).await.unwrap();

      assert_eq!(pool.total_in_use_count(), 3);
      assert!(pool.total_available_count() < initial_available);
    }

    // After dropping, buffers should be returned to pool
    assert_eq!(pool.total_available_count(), initial_available);
    assert_eq!(pool.total_in_use_count(), initial_in_use);
  }

  #[compio::test]
  async fn test_multithreaded_bucketed_pool() {
    // Test concurrent access to the bucketed pool
    let pool = Arc::new(BucketedPool::new_with_memory_budget(1024, 4096, 200 * 1024, 100, 2, 0.5));
    let mut handles = vec![];

    for i in 0..10 {
      let pool_clone = pool.clone();
      let handle = thread::spawn(move || {
        crate::runtime_block_on(async move {
          // Each thread requests a different size (max 4000 to stay within 4096 limit)
          let size = 400 + i * 350;
          let mut buf = pool_clone.acquire_buffer(size).await.unwrap();

          // Write some data
          buf.as_mut_slice()[0] = i as u8;

          // Verify we can read it back
          assert_eq!(buf.as_slice()[0], i as u8);
        });
      });
      handles.push(handle);
    }

    for handle in handles {
      handle.join().unwrap();
    }

    // All pool-managed buffers should be returned
    assert_eq!(pool.total_in_use_count(), 0);
  }

  #[compio::test]
  async fn test_stats_tracking() {
    // Test that statistics are properly tracked across all buckets
    let pool = BucketedPool::new_with_memory_budget(100, 800, 10 * 1024, 100, 2, 0.5);

    // Check initial state
    assert_eq!(pool.total_in_use_count(), 0);
    let initial_available = pool.total_available_count();
    assert!(initial_available > 0);

    // Acquire some buffers
    let buf1 = pool.acquire_buffer(50).await.unwrap(); // From first bucket
    let buf2 = pool.acquire_buffer(150).await.unwrap(); // From second bucket
    let buf3 = pool.acquire_buffer(350).await.unwrap(); // From third bucket

    assert_eq!(pool.total_in_use_count(), 3);
    assert_eq!(pool.total_available_count(), initial_available - 3);

    // Return one buffer
    drop(buf1);
    assert_eq!(pool.total_in_use_count(), 2);
    assert_eq!(pool.total_available_count(), initial_available - 2);

    // Return remaining buffers
    drop(buf2);
    drop(buf3);
    assert_eq!(pool.total_in_use_count(), 0);
    assert_eq!(pool.total_available_count(), initial_available);
  }

  #[compio::test]
  async fn test_edge_case_exact_size_match() {
    // Create pool with enough buffers for all requests
    let pool = BucketedPool::new_with_memory_budget(100, 400, 10 * 1024, 100, 2, 0.5);

    // Request exactly the bucket sizes
    let buf1 = pool.acquire_buffer(100).await.unwrap();
    assert_eq!(buf1.len(), 100);

    let buf2 = pool.acquire_buffer(200).await.unwrap();
    assert_eq!(buf2.len(), 200);

    let buf3 = pool.acquire_buffer(400).await.unwrap();
    assert_eq!(buf3.len(), 400);
  }

  #[compio::test]
  async fn test_large_growth_factor() {
    // Test with larger growth factor
    let pool = BucketedPool::new_with_memory_budget(100, 1600, 100 * 1024, 100, 4, 0.5);

    // Should create buckets with 4x growth
    assert!(pool.buckets.len() >= 2);

    // Verify bucket sizes follow the growth factor
    if pool.buckets.len() >= 2 {
      let first_bucket_size = pool.buckets[0].buffer_size();
      let second_bucket_size = pool.buckets[1].buffer_size();
      assert_eq!(second_bucket_size, first_bucket_size * 4);
    }
  }

  #[compio::test]
  async fn test_mixed_pool_and_none() {
    // Test mix of pool allocations and None for oversized
    let pool = BucketedPool::new_with_memory_budget(100, 200, 10 * 1024, 100, 2, 0.5);

    let initial_available = pool.total_available_count();

    // Mix of pool allocations and None
    let buf1 = pool.acquire_buffer(50).await.unwrap(); // Pool: bucket 0
    let buf2 = pool.acquire_buffer(150).await.unwrap(); // Pool: bucket 1
    let buf3 = pool.acquire_buffer(300).await; // None: too large

    assert_eq!(buf1.len(), 100);
    assert_eq!(buf2.len(), 200);
    assert!(buf3.is_none());

    // Track stats before dropping
    let in_use_before = pool.total_in_use_count();
    assert_eq!(in_use_before, 2); // Only pool-managed buffers

    // Drop pool-managed buffers
    drop(buf1);
    drop(buf2);
    assert_eq!(pool.total_in_use_count(), 0);
    assert_eq!(pool.total_available_count(), initial_available);
  }

  #[test]
  fn test_debug_formatting() {
    // Test that debug formatting works correctly
    let pool = BucketedPool::new_with_memory_budget(100, 400, 10 * 1024, 100, 2, 0.5);
    let debug_str = format!("{:?}", pool);

    // Should contain basic pool information
    assert!(debug_str.contains("BucketedPool"));
    assert!(debug_str.contains("num_buckets"));
    assert!(debug_str.contains("total_in_use"));
    assert!(debug_str.contains("total_available"));
  }

  #[compio::test]
  async fn test_clone_behavior() {
    // Test that cloning works correctly
    let pool = BucketedPool::new_with_memory_budget(100, 400, 10 * 1024, 100, 2, 0.5);
    let cloned_pool = pool.clone();

    // Both should have the same initial state
    assert_eq!(pool.total_available_count(), cloned_pool.total_available_count());
    assert_eq!(pool.total_in_use_count(), cloned_pool.total_in_use_count());

    // Acquire from one pool
    let buf = pool.acquire_buffer(50).await.unwrap();
    assert_eq!(pool.total_in_use_count(), 1);
    // Since BucketedPool uses Arc internally, both clones share the same state
    assert_eq!(cloned_pool.total_in_use_count(), 1);

    drop(buf);

    // After dropping, both should show 0 in use
    assert_eq!(pool.total_in_use_count(), 0);
    assert_eq!(cloned_pool.total_in_use_count(), 0);
  }

  #[compio::test]
  async fn test_fallback_to_next_bucket() {
    // Test that when a bucket is exhausted, we fall back to the next larger bucket
    // Create pool with small memory budget to ensure limited buffers per bucket
    let pool = BucketedPool::new_with_memory_budget(100, 400, 1000, 5, 2, 0.5);

    // With 1KB budget and 50% decay:
    // First bucket (100 bytes) should get ~500 bytes -> 5 buffers (capped at 5)
    // Second bucket (200 bytes) should get ~250 bytes -> 1 buffer
    // Third bucket (400 bytes) should get ~125 bytes -> 0 buffers (not enough)

    // Verify we have some buffers available
    let initial_available = pool.total_available_count();
    assert!(initial_available > 0);

    // Acquire multiple 50-byte buffers (should come from 100-byte bucket)
    let mut small_buffers = Vec::new();
    let mut count_from_100 = 0;

    // Keep acquiring until we get a 200-byte buffer (indicating 100-byte bucket is exhausted)
    loop {
      let buf = pool.acquire_buffer(50).await.unwrap();
      if buf.len() == 200 {
        // We've fallen back to the next bucket
        assert!(count_from_100 > 0, "should have gotten at least one buffer from 100-byte bucket");
        drop(buf);
        break;
      }
      assert_eq!(buf.len(), 100);
      count_from_100 += 1;
      small_buffers.push(buf);

      if count_from_100 > 20 {
        panic!("too many buffers from 100-byte bucket");
      }
    }

    // Now verify that subsequent small requests also fall back
    let buf2 = pool.acquire_buffer(50).await.unwrap();
    assert_eq!(buf2.len(), 200, "should still get from 200-byte bucket");
    drop(buf2);

    // Release the buffers so they can be reused
    drop(small_buffers);

    // Now that buffers are released, we should be able to acquire from the 100-byte bucket again
    let buf3 = pool.acquire_buffer(50).await.unwrap();
    assert_eq!(buf3.len(), 100, "should get from 100-byte bucket again after release");
  }

  #[compio::test]
  async fn test_excessive_budget_all_buckets_capped() {
    // Test scenario where budget is so large that all buckets hit max_buffers cap
    // This verifies that excess budget is properly handled and doesn't cause issues
    let max_buffers = 100;
    let pool = BucketedPool::new_with_memory_budget(
      1024,                    // 1KB min
      16384,                   // 16KB max
      10 * 1024 * 1024 * 1024, // 10GB budget (excessive)
      max_buffers,             // max 100 buffers per bucket
      2,                       // 2x growth
      0.5,                     // 50% decay
    );

    // Calculate expected buckets: 1KB, 2KB, 4KB, 8KB, 16KB = 5 buckets
    assert_eq!(pool.buckets.len(), 5, "should have 5 buckets");

    // Verify all buckets are capped at max_buffers
    let mut total_memory_used = 0;
    let mut all_capped = true;

    for bucket in pool.buckets.iter() {
      let count = bucket.available_count();
      let size = bucket.buffer_size();

      // Each bucket should be at max_buffers
      if count != max_buffers {
        all_capped = false;
      }
      total_memory_used += count * size;
    }

    assert!(all_capped, "all buckets should be capped at max_buffers");

    // Verify total memory used is much less than the budget
    let budget = 10 * 1024 * 1024 * 1024; // 10GB
    let expected_max_memory = (1024 + 2048 + 4096 + 8192 + 16384) * max_buffers; // ~3.1MB

    assert_eq!(total_memory_used, expected_max_memory, "total memory should be exactly the sum of all capped buckets");

    assert!(
      total_memory_used < budget / 1000,
      "memory used ({} MB) should be much less than budget ({} MB)",
      total_memory_used / 1024 / 1024,
      budget / 1024 / 1024
    );

    // Verify the pool still works correctly
    let buf1 = pool.acquire_buffer(500).await.unwrap();
    assert_eq!(buf1.len(), 1024);

    let buf2 = pool.acquire_buffer(3000).await.unwrap();
    assert_eq!(buf2.len(), 4096);

    let buf3 = pool.acquire_buffer(10000).await.unwrap();
    assert_eq!(buf3.len(), 16384);

    // Track available before cleanup
    let available_before = pool.total_available_count();

    drop(buf1);
    drop(buf2);
    drop(buf3);

    // Verify buffers are returned
    assert_eq!(pool.total_available_count(), available_before + 3);
    assert_eq!(pool.total_in_use_count(), 0);
  }
}
