// SPDX-License-Identifier: BSD-3-Clause

use std::cell::RefCell;
use std::fs as std_fs; // only for read_dir (no compio equivalent)
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use compio::BufResult;
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};
use memmap2::{Mmap, MmapMut};
use narwhal_protocol::{Message, NID_MAX_LENGTH};
use narwhal_util::pool::PoolBuffer;
use narwhal_util::string_atom::StringAtom;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use super::file_store::channel_hash;
use super::store::{LogEntry, LogVisitor, MessageLog, MessageLogFactory};

/// Maximum segment file size before rolling to a new segment.
const SEGMENT_MAX_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB

/// Bytes of log data between sparse index entries.
const INDEX_INTERVAL_BYTES: u64 = 4096;

/// Size of one index entry on disk: relative_seq(4) + offset(8).
const INDEX_ENTRY_SIZE: usize = 12;

/// Size of the fixed portion of a log entry header.
/// seq(8) + timestamp(8) + from_len(2) + payload_len(4) = 22 bytes.
const ENTRY_HEADER_SIZE: usize = 22;

/// Size of the CRC32 trailer.
const CRC_SIZE: usize = 4;

/// Segment file extension.
const SEGMENT_EXT: &str = "log";

/// Index file extension.
const INDEX_EXT: &str = "idx";

/// Metric handles for `FileMessageLog` operations.
///
/// Cloneable — each `FileMessageLog` instance holds a clone, so a single set of
/// handles is shared across all logs created by a factory.
#[derive(Clone)]
pub struct MessageLogMetrics {
  recovery_duration_seconds: Histogram,
  append_duration_seconds: Histogram,
  segments_rolled: Counter,
  segments_evicted: Counter,
  evicted_bytes: Counter,
  crc_failures: Counter,
  /// Incremented once per sealed segment whose CRC scan terminates before
  /// the file's actual end during recovery. Indicates bit rot or other
  /// mid-segment data corruption: the segment's `last_seq` is set to the
  /// last valid entry, and any valid entries past the corruption boundary
  /// are silently invisible to subsequent reads. Operators should treat a
  /// non-zero value as a signal to investigate disk health.
  sealed_segment_truncations: Counter,
}

impl std::fmt::Debug for MessageLogMetrics {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MessageLogMetrics").finish_non_exhaustive()
  }
}

impl MessageLogMetrics {
  /// Registers the metric handles on `registry` and returns them.
  pub fn register(registry: &mut Registry) -> Self {
    let recovery_duration_seconds =
      Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(0.001, 2.0, 16));
    registry.register(
      "message_log_recovery_duration_seconds",
      "Duration of message log recovery at startup in seconds",
      recovery_duration_seconds.clone(),
    );
    let append_duration_seconds =
      Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(0.0001, 2.0, 16));
    registry.register(
      "message_log_append_duration_seconds",
      "Duration of message log append operations in seconds",
      append_duration_seconds.clone(),
    );
    let segments_rolled = Counter::default();
    registry.register("message_log_segments_rolled", "Message log segment rolls", segments_rolled.clone());
    let segments_evicted = Counter::default();
    registry.register("message_log_segments_evicted", "Message log segments evicted", segments_evicted.clone());
    let evicted_bytes = Counter::default();
    registry.register("message_log_evicted_bytes", "Bytes reclaimed by message log eviction", evicted_bytes.clone());
    let crc_failures = Counter::default();
    registry.register(
      "message_log_crc_failures",
      "Message log entries rejected due to CRC mismatch",
      crc_failures.clone(),
    );
    let sealed_segment_truncations = Counter::default();
    registry.register(
      "message_log_sealed_segment_truncations",
      "Sealed segments whose CRC scan terminated before EOF on recovery (likely bit rot or mid-segment corruption)",
      sealed_segment_truncations.clone(),
    );

    Self {
      recovery_duration_seconds,
      append_duration_seconds,
      segments_rolled,
      segments_evicted,
      evicted_bytes,
      crc_failures,
      sealed_segment_truncations,
    }
  }

  /// Returns an instance with unregistered handles — useful for tests that do
  /// not care about observing metric values.
  pub fn noop() -> Self {
    Self {
      recovery_duration_seconds: Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(
        0.001, 2.0, 16,
      )),
      append_duration_seconds: Histogram::new(prometheus_client::metrics::histogram::exponential_buckets(
        0.0001, 2.0, 16,
      )),
      segments_rolled: Counter::default(),
      segments_evicted: Counter::default(),
      evicted_bytes: Counter::default(),
      crc_failures: Counter::default(),
      sealed_segment_truncations: Counter::default(),
    }
  }
}

/// RAII guard that observes elapsed time on a `Histogram` when dropped.
/// Ensures the histogram is updated on both `Ok` and `Err` paths without
/// requiring every early return to record the observation explicitly.
struct ObserveOnDrop {
  histogram: Histogram,
  start: Instant,
}

impl Drop for ObserveOnDrop {
  fn drop(&mut self) {
    self.histogram.observe(self.start.elapsed().as_secs_f64());
  }
}

/// Reusable, zero-allocation entry reader for the message log.
///
/// Pre-allocates header and body buffers once at construction.  Buffers are
/// moved into compio positioned reads via `std::mem::take` and reclaimed from
/// `BufResult`, so no per-entry heap allocation occurs after the first read.
struct EntryReader {
  /// Fixed 22-byte header buffer (seq + timestamp + from_len + payload_len).
  header: Vec<u8>,
  /// Body buffer pre-allocated to `NID_MAX_LENGTH + max_payload_size + CRC_SIZE`.
  body: Vec<u8>,

  // Parsed fields from the most recent successful `read_at`:
  seq: u64,
  timestamp: u64,
  from_len: usize,
  payload_len: usize,
  entry_size: u64,

  /// Counter incremented whenever `read_at` detects a CRC mismatch.
  crc_failures: Counter,
}

impl EntryReader {
  fn new(max_payload_size: u32, crc_failures: Counter) -> Self {
    Self {
      header: Vec::with_capacity(ENTRY_HEADER_SIZE),
      body: Vec::with_capacity(NID_MAX_LENGTH + max_payload_size as usize + CRC_SIZE),
      seq: 0,
      timestamp: 0,
      from_len: 0,
      payload_len: 0,
      entry_size: 0,
      crc_failures,
    }
  }

  /// Read one entry from `file` at byte offset `pos`.
  ///
  /// Returns `true` if the entry was read and CRC-validated successfully.
  /// On success the parsed fields (`seq`, `timestamp`, etc.) and the body
  /// buffer are updated; callers access the data via [`get_from()`] and
  /// [`payload_bytes()`].
  async fn read_at(&mut self, file: &compio::fs::File, pos: u64, remaining: u64) -> bool {
    if remaining < ENTRY_HEADER_SIZE as u64 {
      return false;
    }

    // --- read header (22 bytes) ---
    let mut header = std::mem::take(&mut self.header);
    header.clear(); // len → 0, capacity stays ENTRY_HEADER_SIZE
    let BufResult(result, header) = file.read_exact_at(header, pos).await;
    self.header = header;
    if result.is_err() {
      return false;
    }

    // --- parse header ---
    let seq = u64::from_le_bytes(self.header[0..8].try_into().unwrap());
    let timestamp = u64::from_le_bytes(self.header[8..16].try_into().unwrap());
    let from_len = u16::from_le_bytes(self.header[16..18].try_into().unwrap()) as usize;
    let payload_len = u32::from_le_bytes(self.header[18..22].try_into().unwrap()) as usize;

    let body_size = from_len + payload_len + CRC_SIZE;
    if body_size > self.body.capacity() {
      return false;
    }
    let entry_size = ENTRY_HEADER_SIZE as u64 + body_size as u64;
    if remaining < entry_size {
      return false;
    }

    // --- read body (from + payload + crc) ---
    let mut body = std::mem::take(&mut self.body);
    body.clear(); // len → 0, capacity preserved

    // Use `IoBuf::slice(..body_size)` to read exactly `body_size` bytes
    // into the pre-allocated buffer without touching the rest of its capacity.
    use compio::buf::{IntoInner, IoBuf};
    let sliced = body.slice(..body_size);
    let BufResult(result, sliced) = file.read_exact_at(sliced, pos + ENTRY_HEADER_SIZE as u64).await;
    self.body = sliced.into_inner();
    if result.is_err() {
      return false;
    }

    // --- verify CRC ---
    let crc_offset = from_len + payload_len;
    let stored_crc = u32::from_le_bytes(self.body[crc_offset..crc_offset + CRC_SIZE].try_into().unwrap());

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&self.header);
    hasher.update(&self.body[..crc_offset]);
    let computed_crc = hasher.finalize();

    if stored_crc != computed_crc {
      self.crc_failures.inc();
      return false;
    }

    self.seq = seq;
    self.timestamp = timestamp;
    self.from_len = from_len;
    self.payload_len = payload_len;
    self.entry_size = entry_size;
    true
  }

  /// Borrow the `from` field from the last successfully read entry.
  fn get_from(&self) -> &[u8] {
    &self.body[..self.from_len]
  }

  /// Borrow the `payload` field from the last successfully read entry.
  fn payload_bytes(&self) -> &[u8] {
    &self.body[self.from_len..self.from_len + self.payload_len]
  }
}

struct SegmentInfo {
  first_seq: u64,
  last_seq: u64,
  file_size: u64,
  /// Memory-mapped index for sealed segments. `None` for the active segment.
  idx_mmap: Option<Mmap>,
}

impl std::fmt::Debug for SegmentInfo {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SegmentInfo")
      .field("first_seq", &self.first_seq)
      .field("last_seq", &self.last_seq)
      .field("file_size", &self.file_size)
      .field("idx_mmap_len", &self.idx_mmap.as_ref().map(|m| m.len()))
      .finish()
  }
}

/// A file-based message log implementation using segmented append-only log files
/// with per-segment sparse index files.
pub struct FileMessageLog {
  inner: RefCell<Inner>,
}

struct Inner {
  /// Channel directory path.
  channel_dir: PathBuf,

  /// Maximum payload size accepted by `append`. Entries with a larger payload
  /// would not fit in the read-side `EntryReader::body` buffer, so we reject
  /// them at the write boundary.
  max_payload_size: u32,

  /// Maximum segment file size before rolling to a new segment.
  segment_max_bytes: u64,

  /// Metadata for every segment (sorted by first_seq).
  segments: Vec<SegmentInfo>,

  /// Open file handle for the active (latest) segment's .log.
  active_log: Option<compio::fs::File>,

  /// Open file handle for the active segment's pre-allocated `.idx` file.
  /// Kept open so flushes can use async `sync_all()` instead of blocking
  /// `mmap.flush()` on the shard runtime thread.
  active_idx_file: Option<compio::fs::File>,

  /// Read-write mmap of the active segment's pre-allocated `.idx` file.
  /// Entries are written directly into this mapping, Kafka-style.
  active_idx_mmap: Option<MmapMut>,

  /// Number of valid bytes written into the active index mmap.
  active_idx_write_pos: usize,

  /// Bytes written to the active segment since the last index entry was emitted.
  bytes_since_index: u64,

  /// Global first retained seq (from oldest segment), 0 if empty.
  cached_first_seq: u64,
  /// Global last written seq, 0 if empty.
  cached_last_seq: u64,

  /// Pre-allocated buffer for serializing entries on the write path.
  write_buf: Vec<u8>,

  /// Pre-allocated entry reader for the read path and recovery.
  reader: EntryReader,

  /// Metric handles for operations performed by the log.
  metrics: MessageLogMetrics,
}

// === impl FileMessageLog ===

impl FileMessageLog {
  /// Create a new (or recover an existing) message log rooted at `channel_dir`.
  ///
  /// Returns an error if recovery cannot bring the channel into a consistent
  /// writable state (e.g. the active segment's `.log` cannot be opened for
  /// writes, the active `.idx` cannot be opened or memory-mapped). Callers
  /// should refuse to bring the affected channel online rather than continue
  /// with a half-initialized log that would silently drop index updates.
  async fn open(channel_dir: PathBuf, max_payload_size: u32, metrics: MessageLogMetrics) -> anyhow::Result<Self> {
    Self::open_with_segment_max(channel_dir, max_payload_size, SEGMENT_MAX_BYTES, metrics).await
  }

  async fn open_with_segment_max(
    channel_dir: PathBuf,
    max_payload_size: u32,
    segment_max_bytes: u64,
    metrics: MessageLogMetrics,
  ) -> anyhow::Result<Self> {
    let mut inner = Inner {
      channel_dir,
      max_payload_size,
      segment_max_bytes,
      segments: Vec::new(),
      active_log: None,
      active_idx_file: None,
      active_idx_mmap: None,
      active_idx_write_pos: 0,
      bytes_since_index: 0,
      cached_first_seq: 0,
      cached_last_seq: 0,
      write_buf: Vec::with_capacity(ENTRY_HEADER_SIZE + NID_MAX_LENGTH + max_payload_size as usize + CRC_SIZE),
      reader: EntryReader::new(max_payload_size, metrics.crc_failures.clone()),
      metrics,
    };
    let start = Instant::now();
    let result = inner.recover().await;
    inner.metrics.recovery_duration_seconds.observe(start.elapsed().as_secs_f64());
    result?;
    Ok(FileMessageLog { inner: RefCell::new(inner) })
  }
}

// === impl Inner ===

impl Inner {
  async fn recover(&mut self) -> anyhow::Result<()> {
    // list_segment_seqs uses std_fs::read_dir (no compio equivalent).
    // If the directory doesn't exist, read_dir fails and returns an empty Vec.
    let mut seg_seqs = Self::list_segment_seqs(&self.channel_dir);
    if seg_seqs.is_empty() {
      return Ok(());
    }
    seg_seqs.sort();

    let mut idx_buf = Vec::with_capacity(self.idx_capacity() as usize);

    // Tracks whether the last seg_seqs entry was successfully recovered as the
    // active segment. If false (e.g. zero-byte .log, or every entry failed CRC),
    // we must NOT promote a preceding sealed segment to active — that would
    // open a sealed .log for writes and double-map its .idx (read-only +
    // writable) to the same file.
    let mut active_segment_recovered = false;

    // Process each segment.
    for (i, &base_seq) in seg_seqs.iter().enumerate() {
      let log_path = self.segment_log_path(base_seq);
      let idx_path = self.segment_idx_path(base_seq);
      let is_last = i == seg_seqs.len() - 1;

      let file_size = match compio::fs::metadata(&log_path).await {
        Ok(m) => m.len(),
        Err(e) => {
          tracing::warn!(path = %log_path.display(), error = %e, "skipping segment: failed to read metadata");
          continue;
        },
      };

      if file_size == 0 {
        // Empty segment file — leftover from crash during roll.
        let _ = compio::fs::remove_file(&log_path).await;
        let _ = compio::fs::remove_file(&idx_path).await;
        continue;
      }

      if is_last {
        // Active segment: validate CRC, truncate at first bad entry, rebuild index.
        let (last_seq, valid_size) = Self::scan_and_validate(&log_path, base_seq, &mut self.reader).await;
        if valid_size < file_size {
          // Truncate partial/corrupt tail.
          if let Ok(f) = compio::fs::OpenOptions::new().write(true).open(&log_path).await {
            let _ = f.set_len(valid_size).await;
          }
        }
        if last_seq > 0 {
          // Rebuild the index from the validated log.
          Self::rebuild_index(&log_path, &idx_path, base_seq, &mut self.reader, &mut idx_buf).await;
          self.segments.push(SegmentInfo { first_seq: base_seq, last_seq, file_size: valid_size, idx_mmap: None });
          active_segment_recovered = true;
        } else {
          // No valid entries at all — remove the segment.
          let _ = compio::fs::remove_file(&log_path).await;
          let _ = compio::fs::remove_file(&idx_path).await;
        }

        // Determine bytes_since_index for resuming appends.
        self.bytes_since_index =
          Self::compute_bytes_since_last_index(&log_path, &idx_path, valid_size, &mut self.reader).await;
      } else {
        // Sealed segment: validate, rebuild the index if missing or visibly corrupt.
        let (last_seq, valid_size) = Self::scan_and_validate(&log_path, base_seq, &mut self.reader).await;
        if last_seq == 0 {
          let _ = compio::fs::remove_file(&log_path).await;
          let _ = compio::fs::remove_file(&idx_path).await;
          continue;
        }
        // Mid-segment CRC failure on a sealed segment is silent data loss:
        // the .log keeps the corrupted bytes and any valid entries past
        // them, but `last_seq` is pinned to the pre-corruption position
        // and reads stop there. Surface this to operators via a metric +
        // warn log so disk health can be investigated rather than
        // discovered later via missing history.
        if valid_size < file_size {
          self.metrics.sealed_segment_truncations.inc();
          tracing::warn!(
            path = %log_path.display(),
            file_size,
            valid_size,
            last_valid_seq = last_seq,
            lost_bytes = file_size - valid_size,
            "sealed message-log segment failed CRC mid-file; entries past the failure are unreachable"
          );
        }
        if !Self::looks_like_valid_index(&idx_path, valid_size, self.idx_capacity()).await {
          Self::rebuild_index(&log_path, &idx_path, base_seq, &mut self.reader, &mut idx_buf).await;
        }
        let idx_mmap = Self::mmap_index(&idx_path).await;
        // Use `valid_size` rather than the on-disk `file_size` so subsequent
        // reads stop at the last validated entry and never attempt to
        // decode bytes past the CRC failure boundary.
        self.segments.push(SegmentInfo { first_seq: base_seq, last_seq, file_size: valid_size, idx_mmap });
      }
    }

    // Derive cached state.
    if let Some(first) = self.segments.first() {
      self.cached_first_seq = first.first_seq;
    }
    if let Some(last) = self.segments.last() {
      self.cached_last_seq = last.last_seq;
    }

    // Open active segment for appending — only when the last seg_seqs entry
    // was actually recovered. Otherwise leave active_log = None so the next
    // append creates a fresh segment via Inner::create_segment.
    //
    // Failures here are fatal: if we can't open the active log for writes or
    // memory-map its index, the channel cannot accept appends without
    // silently dropping index updates (which would corrupt subsequent reads
    // on this segment). Bubble the error so the caller refuses to bring the
    // channel online; other channels keep running.
    if active_segment_recovered && let Some(seg) = self.segments.last() {
      let log_path = self.segment_log_path(seg.first_seq);
      let idx_path = self.segment_idx_path(seg.first_seq);

      // No append mode in compio — use positioned writes at seg.file_size.
      let log_file = compio::fs::OpenOptions::new()
        .write(true)
        .open(&log_path)
        .await
        .with_context(|| format!("opening active segment .log for writes: {}", log_path.display()))?;

      // Extend the active index file to its pre-allocated capacity and mmap read-write.
      // Read the size from the already-open handle (no TOCTOU; metadata errors
      // propagate instead of silently masking the resume position to 0).
      let idx_file = compio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&idx_path)
        .await
        .with_context(|| format!("opening active segment .idx: {}", idx_path.display()))?;
      let actual_size = idx_file
        .metadata()
        .await
        .with_context(|| format!("reading active segment .idx size: {}", idx_path.display()))?
        .len() as usize;
      let capacity = self.idx_capacity();
      if (actual_size as u64) < capacity {
        idx_file.set_len(capacity).await.with_context(|| {
          format!("extending active segment .idx to pre-allocated capacity: {}", idx_path.display())
        })?;
      }
      // SAFETY: Single-threaded shard actor; no concurrent access. The active index
      // file is only written to through this mmap.
      let mmap = unsafe { MmapMut::map_mut(&idx_file) }
        .with_context(|| format!("memory-mapping active segment .idx: {}", idx_path.display()))?;

      self.active_log = Some(log_file);
      self.active_idx_file = Some(idx_file);
      self.active_idx_mmap = Some(mmap);
      self.active_idx_write_pos = actual_size;
    }

    Ok(())
  }

  /// Sanity-check a sealed segment's `.idx` file before mmapping it.
  ///
  /// The format is unprotected by a checksum, so a corrupted or truncated
  /// `.idx` would be silently mapped and produce bogus offsets at read time
  /// (worst case: empty reads from a valid `.log`). These cheap structural
  /// checks catch the obvious cases; if any fails, the caller rebuilds the
  /// index by scanning the log:
  /// - file exists and its size fits in `(0, idx_capacity]` (oversized `.idx`
  ///   files most often indicate corruption, but can also legitimately appear
  ///   if `segment_max_bytes` or `INDEX_INTERVAL_BYTES` was reduced between
  ///   runs — rebuilding from the log handles both cases, and bounding the
  ///   size first also bounds the buffer we load below),
  /// - the size is a multiple of `INDEX_ENTRY_SIZE`,
  /// - the first entry is `(relative_seq=0, offset=0)` (entry-0 rule),
  /// - both `relative_seq` and `offset` are strictly monotonically increasing
  ///   across consecutive entries,
  /// - the last entry's `offset` leaves room for at least one full entry
  ///   header in the `.log` file (i.e. `offset + ENTRY_HEADER_SIZE <=
  ///   log_file_size`); otherwise an indexed read at that offset would fail
  ///   `EntryReader`'s remaining-bytes check anyway.
  ///
  /// Returns `false` if the file is missing, malformed, or fails any check.
  async fn looks_like_valid_index(idx_path: &Path, log_file_size: u64, idx_capacity: u64) -> bool {
    let Ok(metadata) = compio::fs::metadata(idx_path).await else {
      return false;
    };
    let size = metadata.len();
    if size == 0 || size > idx_capacity || size % INDEX_ENTRY_SIZE as u64 != 0 {
      return false;
    }

    // Size is bounded by idx_capacity above, so loading the whole file is
    // safe and lets us walk all entries without per-entry syscalls.
    let Ok(data) = compio::fs::read(idx_path).await else {
      return false;
    };
    // Guard against a race or short read between metadata() and read(): bail
    // before any subsequent slicing is allowed to assume validated bounds.
    if data.len() as u64 != size {
      return false;
    }

    // Entry-0 rule: the first index entry is always written by
    // maybe_write_index_entry / rebuild_index as (relative_seq=0, offset=0).
    let first_rel_seq = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let first_offset = u64::from_le_bytes(data[4..12].try_into().unwrap());
    if first_rel_seq != 0 || first_offset != 0 {
      return false;
    }

    let entry_count = data.len() / INDEX_ENTRY_SIZE;
    let mut prev_rel_seq = first_rel_seq;
    let mut prev_offset = first_offset;
    for i in 1..entry_count {
      let off = i * INDEX_ENTRY_SIZE;
      let rel_seq = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
      let offset = u64::from_le_bytes(data[off + 4..off + 12].try_into().unwrap());
      if rel_seq <= prev_rel_seq || offset <= prev_offset {
        return false;
      }
      prev_rel_seq = rel_seq;
      prev_offset = offset;
    }

    // After the loop `prev_offset` is the last entry's offset. It must point
    // at a position where a full entry header fits in the .log file.
    prev_offset.saturating_add(ENTRY_HEADER_SIZE as u64) <= log_file_size
  }

  /// Compute bytes written since the last index entry for the active segment.
  async fn compute_bytes_since_last_index(
    log_path: &Path,
    idx_path: &Path,
    valid_size: u64,
    reader: &mut EntryReader,
  ) -> u64 {
    if valid_size == 0 {
      return 0;
    }
    // Read the last index entry to find the offset it covers.
    if let Ok(idx_data) = compio::fs::read(idx_path).await {
      let entry_count = idx_data.len() / INDEX_ENTRY_SIZE;
      if entry_count > 0 {
        let last_entry_offset = (entry_count - 1) * INDEX_ENTRY_SIZE + 4; // skip relative_seq
        if last_entry_offset + 8 <= idx_data.len() {
          let offset = u64::from_le_bytes(idx_data[last_entry_offset..last_entry_offset + 8].try_into().unwrap());
          // Scan from that offset to compute bytes written after it.
          if let Ok(file) = compio::fs::File::open(log_path).await {
            let mut bytes_after = 0u64;
            let mut pos = offset;
            let mut first_entry = true;
            while pos < valid_size {
              let remaining = valid_size - pos;
              if !reader.read_at(&file, pos, remaining).await {
                break;
              }
              if first_entry {
                // The indexed entry itself — skip it but count bytes after.
                first_entry = false;
                pos += reader.entry_size;
                continue;
              }
              bytes_after += reader.entry_size;
              pos += reader.entry_size;
            }
            return bytes_after;
          }
        }
      }
    }
    // No index entries — all bytes count.
    valid_size
  }

  /// Maximum pre-allocated size for an index file, in bytes.
  /// Computed from the segment size threshold and index interval.
  fn idx_capacity(&self) -> u64 {
    (self.segment_max_bytes / INDEX_INTERVAL_BYTES + 1) * INDEX_ENTRY_SIZE as u64
  }

  /// List segment base sequences from `.log` filenames.
  /// Uses `std_fs::read_dir` — no compio equivalent for directory listing.
  fn list_segment_seqs(dir: &Path) -> Vec<u64> {
    let Ok(entries) = std_fs::read_dir(dir) else {
      return Vec::new();
    };
    entries
      .filter_map(|e| e.ok())
      .filter_map(|e| {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) == Some(SEGMENT_EXT) {
          path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok())
        } else {
          None
        }
      })
      .collect()
  }

  fn segment_log_path(&self, first_seq: u64) -> PathBuf {
    self.channel_dir.join(format!("{first_seq:020}.{SEGMENT_EXT}"))
  }

  fn segment_idx_path(&self, first_seq: u64) -> PathBuf {
    self.channel_dir.join(format!("{first_seq:020}.{INDEX_EXT}"))
  }

  fn serialize_entry(&mut self, seq: u64, timestamp: u64, from: &[u8], payload: &[u8]) {
    let from_len = from.len() as u16;
    let payload_len = payload.len() as u32;

    self.write_buf.clear();
    self.write_buf.extend_from_slice(&seq.to_le_bytes());
    self.write_buf.extend_from_slice(&timestamp.to_le_bytes());
    self.write_buf.extend_from_slice(&from_len.to_le_bytes());
    self.write_buf.extend_from_slice(&payload_len.to_le_bytes());
    self.write_buf.extend_from_slice(from);
    self.write_buf.extend_from_slice(payload);

    let crc = crc32fast::hash(&self.write_buf);
    self.write_buf.extend_from_slice(&crc.to_le_bytes());
  }

  /// Scan the log file validating CRC per entry. Returns (last_seq, valid_size).
  async fn scan_and_validate(log_path: &Path, base_seq: u64, reader: &mut EntryReader) -> (u64, u64) {
    let file = match compio::fs::File::open(log_path).await {
      Ok(f) => f,
      Err(_) => return (0, 0),
    };
    let file_size = match file.metadata().await {
      Ok(m) => m.len(),
      Err(_) => return (0, 0),
    };

    let mut pos = 0u64;
    let mut last_seq = 0u64;
    let _ = base_seq; // base_seq used for context; actual seq read from entries.

    while pos < file_size {
      let remaining = file_size - pos;
      if !reader.read_at(&file, pos, remaining).await {
        break;
      }
      last_seq = reader.seq;
      pos += reader.entry_size;
    }

    (last_seq, pos)
  }

  /// Rebuild the .idx file by scanning the .log file.
  ///
  /// The buffer is built in-memory, written to a temp sibling
  /// (`<first_seq>.tmp`), fsync'd, and then atomically renamed to
  /// `<first_seq>.idx`. A crash mid-rebuild leaves either the previous
  /// `.idx` intact (if the rename hasn't happened yet) or the new one in
  /// place (if it has) — never a half-written `.idx`. On failure both the
  /// temp file and the existing `.idx` are removed, so the read path falls
  /// back to scanning the segment from the start (`mmap_index` will return
  /// `None`) instead of trusting a stale or partial index.
  ///
  /// Reuses the caller-provided `idx_buf` to avoid per-call allocation.
  async fn rebuild_index(
    log_path: &Path,
    idx_path: &Path,
    base_seq: u64,
    reader: &mut EntryReader,
    idx_buf: &mut Vec<u8>,
  ) {
    // Helper: drop the existing .idx and (if present) the temp sibling so
    // mmap_index returns None on the next call and reads fall back to a
    // full-segment scan rather than trusting a stale or partial index.
    async fn drop_indexes(idx_path: &Path, tmp_path: Option<&Path>) {
      if let Some(tmp) = tmp_path {
        let _ = compio::fs::remove_file(tmp).await;
      }
      let _ = compio::fs::remove_file(idx_path).await;
    }

    let file = match compio::fs::File::open(log_path).await {
      Ok(f) => f,
      Err(_) => {
        drop_indexes(idx_path, None).await;
        return;
      },
    };
    let file_size = match file.metadata().await {
      Ok(m) => m.len(),
      Err(_) => {
        drop_indexes(idx_path, None).await;
        return;
      },
    };

    idx_buf.clear();

    let mut pos = 0u64;
    let mut bytes_since_index = 0u64;
    let mut first_entry = true;

    while pos < file_size {
      let remaining = file_size - pos;
      let entry_offset = pos;
      if !reader.read_at(&file, pos, remaining).await {
        break;
      }
      if first_entry || bytes_since_index >= INDEX_INTERVAL_BYTES {
        let relative_seq = (reader.seq - base_seq) as u32;
        idx_buf.extend_from_slice(&relative_seq.to_le_bytes());
        idx_buf.extend_from_slice(&entry_offset.to_le_bytes());
        bytes_since_index = 0;
        first_entry = false;
      }
      bytes_since_index += reader.entry_size;
      pos += reader.entry_size;
    }

    // The previous `.idx` (if any) stays on disk untouched until the rename
    // succeeds. On any failure during the atomic write, also drop the
    // existing `.idx` so the read path falls back to a full-segment scan
    // (`mmap_index` returns `None`) instead of trusting a stale index.
    let tmp_path = idx_path.with_extension("tmp");
    let buf = std::mem::take(idx_buf);
    let (result, buf) =
      crate::util::file::atomic_write(idx_path, &tmp_path, buf, crate::util::file::DirSync::BestEffort).await;
    *idx_buf = buf;
    if result.is_err() {
      drop_indexes(idx_path, Some(&tmp_path)).await;
    }
  }

  /// Memory-map an index file. Returns `None` if the file is empty or doesn't exist.
  async fn mmap_index(idx_path: &Path) -> Option<Mmap> {
    let file = compio::fs::File::open(idx_path).await.ok()?;
    let metadata = file.metadata().await.ok()?;
    if metadata.len() == 0 {
      return None;
    }
    // SAFETY: Sealed index files are never modified or truncated. The `FileMessageLog`
    // is `!Send` and single-threaded; eviction drops the `SegmentInfo` (and its `Mmap`)
    // before deleting the underlying file.
    unsafe { Mmap::map(&file).ok() }
  }

  /// Binary search an index buffer for the largest entry with relative_seq <= target.
  fn index_lookup_in(data: &[u8], target_relative_seq: u32) -> Option<u64> {
    let entry_count = data.len() / INDEX_ENTRY_SIZE;
    if entry_count == 0 {
      return None;
    }

    let get_entry = |i: usize| -> (u32, u64) {
      let off = i * INDEX_ENTRY_SIZE;
      let rel_seq = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
      let offset = u64::from_le_bytes(data[off + 4..off + 12].try_into().unwrap());
      (rel_seq, offset)
    };

    let mut lo = 0usize;
    let mut hi = entry_count;
    while lo < hi {
      let mid = lo + (hi - lo) / 2;
      let (rel_seq, _) = get_entry(mid);
      if rel_seq <= target_relative_seq {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }

    if lo == 0 {
      return Some(0);
    }

    let (_, offset) = get_entry(lo - 1);
    Some(offset)
  }

  async fn create_segment(&mut self, first_seq: u64) -> anyhow::Result<()> {
    compio::fs::create_dir_all(&self.channel_dir).await?;

    let log_path = self.segment_log_path(first_seq);
    let idx_path = self.segment_idx_path(first_seq);

    // No append mode in compio — use positioned writes at seg.file_size.
    self.active_log =
      Some(compio::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log_path).await?);

    // Pre-allocate the index file to its maximum capacity and mmap read-write.
    let idx_file =
      compio::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&idx_path).await?;
    let capacity = self.idx_capacity();
    idx_file.set_len(capacity).await?;
    // SAFETY: Single-threaded shard actor; no concurrent access. The active index
    // file is only written to through this mmap.
    self.active_idx_mmap = Some(unsafe { MmapMut::map_mut(&idx_file)? });
    self.active_idx_file = Some(idx_file);
    self.active_idx_write_pos = 0;

    self.segments.push(SegmentInfo { first_seq, last_seq: 0, file_size: 0, idx_mmap: None });
    self.bytes_since_index = 0;

    Ok(())
  }

  async fn roll_segment(&mut self, next_seq: u64) -> anyhow::Result<()> {
    // Close the log file handle.
    self.active_log = None;

    // Sync the active index file, drop the writable mapping, truncate the file
    // to its actual size, then remap as a read-only Mmap for the sealed segment.
    let write_pos = self.active_idx_write_pos;
    if let Some(ref idx_file) = self.active_idx_file {
      idx_file.sync_all().await?;
    }
    self.active_idx_mmap = None;

    if let Some(ref idx_file) = self.active_idx_file {
      idx_file.set_len(write_pos as u64).await?;
      idx_file.sync_all().await?;
    }
    self.active_idx_file = None;

    if let Some(seg) = self.segments.last() {
      let idx_path = self.channel_dir.join(format!("{:020}.{INDEX_EXT}", seg.first_seq));
      let mmap = Self::mmap_index(&idx_path).await;
      self.segments.last_mut().unwrap().idx_mmap = mmap;
    }

    // Create new segment.
    self.create_segment(next_seq).await?;
    self.metrics.segments_rolled.inc();
    Ok(())
  }

  /// Evict oldest segments whose last_seq is entirely outside the retention window.
  async fn evict_segments(&mut self, max_messages: u32) {
    if max_messages == 0 || self.cached_last_seq == 0 {
      return;
    }

    let retain_from = self.cached_last_seq.saturating_sub(max_messages as u64 - 1);

    // The active (last) segment is never evicted — there must always be a
    // segment to append into.
    while self.segments.len() > 1 {
      if self.segments[0].last_seq < retain_from {
        let removed = self.segments.remove(0);
        let log_path = self.segment_log_path(removed.first_seq);
        let idx_path = self.segment_idx_path(removed.first_seq);
        // Use the on-disk file size for the evicted-bytes metric so that
        // truncated sealed segments (where `removed.file_size == valid_size`,
        // smaller than the physical file) still report the actual disk space
        // reclaimed. Fall back to `removed.file_size` if stat fails.
        let fallback_size = removed.file_size;
        let physical_size = compio::fs::metadata(&log_path).await.map(|m| m.len()).unwrap_or(fallback_size);
        self.metrics.segments_evicted.inc();
        self.metrics.evicted_bytes.inc_by(physical_size);
        // Explicitly drop the segment (and its idx mmap) before unlinking
        // the underlying files. Linux is fine with unlinking a mapped file,
        // but releasing the mapping first keeps the eviction safe on
        // platforms where unlink fails while a mapping is alive.
        drop(removed);
        let _ = compio::fs::remove_file(log_path).await;
        let _ = compio::fs::remove_file(idx_path).await;
      } else {
        break;
      }
    }

    // Update cached first_seq.
    self.cached_first_seq = self.segments.first().map_or(0, |s| s.first_seq);
  }

  fn maybe_write_index_entry(&mut self, seq: u64, entry_offset: u64) {
    // Callers (only `append`) ensure a segment exists before invoking this.
    // Failing loudly here is preferable to a `1` fallback that would underflow
    // `(seq - base_seq) as u32` if the invariant ever changed.
    let base_seq = self.segments.last().expect("maybe_write_index_entry called with no active segment").first_seq;
    let is_first_in_segment = entry_offset == 0;

    if is_first_in_segment || self.bytes_since_index >= INDEX_INTERVAL_BYTES {
      if let Some(ref mut mmap) = self.active_idx_mmap {
        let pos = self.active_idx_write_pos;
        if pos + INDEX_ENTRY_SIZE <= mmap.len() {
          let relative_seq = (seq - base_seq) as u32;
          mmap[pos..pos + 4].copy_from_slice(&relative_seq.to_le_bytes());
          mmap[pos + 4..pos + 12].copy_from_slice(&entry_offset.to_le_bytes());
          self.active_idx_write_pos = pos + INDEX_ENTRY_SIZE;
        }
      }
      self.bytes_since_index = 0;
    }
  }

  /// Find the segment that contains `target_seq`.
  fn find_segment_for_seq(&self, target_seq: u64) -> Option<usize> {
    // Binary search: find the last segment whose first_seq <= target_seq.
    let mut lo = 0usize;
    let mut hi = self.segments.len();
    while lo < hi {
      let mid = lo + (hi - lo) / 2;
      if self.segments[mid].first_seq <= target_seq {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    if lo == 0 { None } else { Some(lo - 1) }
  }
}

// The shard actor model guarantees single-threaded, non-reentrant access —
// holding a RefCell borrow across .await is safe in this context.
#[allow(clippy::await_holding_refcell_ref)]
#[async_trait(?Send)]
impl MessageLog for FileMessageLog {
  async fn append(&self, message: &Message, payload: &PoolBuffer, max_messages: u32) -> anyhow::Result<()> {
    let inner = &mut *self.inner.borrow_mut();
    let _observe = ObserveOnDrop { histogram: inner.metrics.append_duration_seconds.clone(), start: Instant::now() };

    let params = match message {
      Message::Message(params) => params,
      _ => anyhow::bail!("expected Message::Message"),
    };

    let seq = params.seq;
    let timestamp = params.timestamp;
    let from = params.from.as_ref().as_bytes();
    let payload_bytes = payload.as_slice();

    // Reject `seq == 0` and non-strictly-monotonic sequences before any state
    // change. `0` is the empty-log sentinel returned by `first_seq`/`last_seq`,
    // so writing an entry with `seq == 0` would persist data while leaving
    // both cached values at 0 — the log would appear empty afterwards. A
    // `seq <= cached_last_seq` would corrupt the sparse index, whose binary
    // search assumes strictly monotonic `relative_seq` per segment; a
    // subsequent recovery would mark the index as visibly corrupt and rebuild
    // it from the same broken log on every startup.
    //
    // These are caller-contract violations rather than expected runtime
    // states, so we surface them loudly instead of silently corrupting on
    // disk.
    if seq == 0 {
      anyhow::bail!("seq must be > 0 (0 is reserved as the empty-log sentinel)");
    }
    if seq <= inner.cached_last_seq {
      anyhow::bail!("seq must be strictly greater than last_seq: got {seq}, last_seq is {}", inner.cached_last_seq);
    }

    // Reject oversized entries before any state change. The read-side
    // `EntryReader::body` buffer is sized for `NID_MAX_LENGTH + max_payload_size
    // + CRC_SIZE`; anything larger would be unreadable on recovery and the
    // segment tail would be silently truncated.
    if from.len() > NID_MAX_LENGTH {
      anyhow::bail!("from field too large: {} > {NID_MAX_LENGTH}", from.len());
    }
    if payload_bytes.len() > inner.max_payload_size as usize {
      anyhow::bail!("payload too large: {} > {}", payload_bytes.len(), inner.max_payload_size);
    }

    // Ensure we have an active segment.
    if inner.active_log.is_none() {
      inner.create_segment(seq).await?;
    }

    // Get the current offset before writing (for index).
    let seg = inner.segments.last().unwrap();
    let entry_offset = seg.file_size;

    // Serialize into pre-allocated buffer and write using positioned I/O.
    inner.serialize_entry(seq, timestamp, from, payload_bytes);
    let entry_len = inner.write_buf.len() as u64;

    if let Some(ref log_file) = inner.active_log {
      let buf = std::mem::take(&mut inner.write_buf);
      let mut file_ref = log_file;
      let BufResult(result, buf) = file_ref.write_all_at(buf, entry_offset).await;
      inner.write_buf = buf;
      result?;
    }

    // Write index entry if needed (before updating bytes_since_index).
    inner.maybe_write_index_entry(seq, entry_offset);

    // Update in-memory state.
    inner.bytes_since_index += entry_len;
    if let Some(seg) = inner.segments.last_mut() {
      seg.last_seq = seq;
      seg.file_size += entry_len;
    }

    if inner.cached_first_seq == 0 {
      inner.cached_first_seq = seq;
    }
    inner.cached_last_seq = seq;

    // Check if segment needs to roll (after append).
    let should_roll = inner.segments.last().is_some_and(|s| s.file_size > inner.segment_max_bytes);
    if should_roll {
      inner.roll_segment(seq + 1).await?;
    }

    // Evict old segments if needed.
    inner.evict_segments(max_messages).await;

    Ok(())
  }

  async fn delete(&self) -> anyhow::Result<()> {
    let inner = &mut *self.inner.borrow_mut();

    // Close active handles and drop the active index mmap.
    inner.active_log = None;
    inner.active_idx_file = None;
    inner.active_idx_mmap = None;
    inner.active_idx_write_pos = 0;

    // Collect paths before clearing segments (which drops mmaps).
    let paths: Vec<_> = inner
      .segments
      .iter()
      .map(|seg| (inner.segment_log_path(seg.first_seq), inner.segment_idx_path(seg.first_seq)))
      .collect();

    // Drop all segments (and their mmaps).
    inner.segments.clear();

    // Remove files after mmaps have been unmapped.
    for (log_path, idx_path) in paths {
      let _ = compio::fs::remove_file(&log_path).await;
      let _ = compio::fs::remove_file(&idx_path).await;
    }

    inner.cached_first_seq = 0;
    inner.cached_last_seq = 0;
    inner.bytes_since_index = 0;

    Ok(())
  }

  async fn flush(&self) -> anyhow::Result<()> {
    let inner = &mut *self.inner.borrow_mut();

    if let Some(ref log_file) = inner.active_log {
      log_file.sync_all().await?;
    }
    if inner.active_idx_mmap.is_some() {
      let idx_file = inner
        .active_idx_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("active index mmap is present without an active index file handle"))?;
      idx_file.sync_all().await?;
    }

    Ok(())
  }

  fn first_seq(&self) -> u64 {
    self.inner.borrow().cached_first_seq
  }

  fn last_seq(&self) -> u64 {
    self.inner.borrow().cached_last_seq
  }

  async fn read(&self, from_seq: u64, limit: u32, visitor: &mut impl LogVisitor) -> anyhow::Result<u32>
  where
    Self: Sized,
  {
    let this = &mut *self.inner.borrow_mut();

    if limit == 0 || this.cached_last_seq == 0 || from_seq > this.cached_last_seq {
      return Ok(0);
    }

    let from_seq = from_seq.max(this.cached_first_seq);

    // Find which segment contains from_seq.
    let seg_idx = match this.find_segment_for_seq(from_seq) {
      Some(idx) => idx,
      None => return Ok(0),
    };

    let mut count = 0u32;

    for i in seg_idx..this.segments.len() {
      if count >= limit {
        break;
      }

      let seg = &this.segments[i];
      let log_path = this.segment_log_path(seg.first_seq);

      let file = match compio::fs::File::open(&log_path).await {
        Ok(f) => f,
        Err(_) => continue,
      };

      // Use index to seek close to from_seq within this segment.
      // Discriminate by position, not by `seg.idx_mmap.is_some()`: a sealed
      // segment with `idx_mmap == None` (e.g., a previous mmap_index failure)
      // must NOT fall through to `active_idx_mmap`, which belongs to a
      // different segment and would yield offsets outside this segment.
      let start_offset = if i == seg_idx && from_seq > seg.first_seq {
        let target_rel = (from_seq - seg.first_seq) as u32;
        // Treat a segment as active only when an active mmap is actually
        // open. After a recovery that left the on-disk active segment empty
        // and removed it, `segments.last()` is sealed and has its own
        // `idx_mmap`; we must use that rather than fall through to a
        // (None) `active_idx_mmap` and force a full scan.
        let is_active = i == this.segments.len() - 1 && this.active_idx_mmap.is_some();
        if is_active {
          // Active segment: read-write mmap, bounded to the actual write position.
          let mmap = this.active_idx_mmap.as_ref().expect("active_idx_mmap must be Some when is_active is true");
          Inner::index_lookup_in(&mmap[..this.active_idx_write_pos], target_rel).unwrap_or(0)
        } else {
          // Sealed segment: read-only mmap; if missing, fall back to scanning
          // from the segment start (correctness preserved at the cost of a
          // linear scan).
          if let Some(ref mmap) = seg.idx_mmap { Inner::index_lookup_in(mmap, target_rel).unwrap_or(0) } else { 0 }
        }
      } else {
        0
      };

      let file_size = seg.file_size;
      let mut pos = start_offset;

      while pos < file_size && count < limit {
        let remaining = file_size - pos;
        if !this.reader.read_at(&file, pos, remaining).await {
          break;
        }
        pos += this.reader.entry_size;
        if this.reader.seq < from_seq {
          continue; // Skip entries before from_seq.
        }
        visitor
          .visit(LogEntry {
            seq: this.reader.seq,
            timestamp: this.reader.timestamp,
            from: this.reader.get_from(),
            payload: this.reader.payload_bytes(),
          })
          .await?;
        count += 1;
      }
    }

    Ok(count)
  }
}

/// Factory for creating `FileMessageLog` instances.
///
/// Holds the base data directory. Each call to `create` derives the channel
/// directory using the same SHA-256 hash of the handler name that
/// `FileChannelStore` uses.
#[derive(Clone)]
pub struct FileMessageLogFactory {
  data_dir: Arc<PathBuf>,
  max_payload_size: u32,
  segment_max_bytes: Option<u64>,
  metrics: MessageLogMetrics,
}

// === impl FileMessageLogFactory ===

impl FileMessageLogFactory {
  pub fn new(data_dir: PathBuf, max_payload_size: u32, metrics: MessageLogMetrics) -> Self {
    Self { data_dir: Arc::new(data_dir), max_payload_size, segment_max_bytes: None, metrics }
  }

  /// Creates a factory that produces logs with a custom segment size threshold.
  /// Useful for testing eviction without needing to fill 128 MiB segments.
  pub fn with_segment_max(
    data_dir: PathBuf,
    max_payload_size: u32,
    segment_max_bytes: u64,
    metrics: MessageLogMetrics,
  ) -> Self {
    Self { data_dir: Arc::new(data_dir), max_payload_size, segment_max_bytes: Some(segment_max_bytes), metrics }
  }
}

#[async_trait(?Send)]
impl MessageLogFactory for FileMessageLogFactory {
  type Log = FileMessageLog;

  async fn create(&self, handler: &StringAtom) -> anyhow::Result<FileMessageLog> {
    let hash = channel_hash(handler);
    let channel_dir = self.data_dir.join(hash.as_ref());
    match self.segment_max_bytes {
      Some(max) => {
        FileMessageLog::open_with_segment_max(channel_dir, self.max_payload_size, max, self.metrics.clone()).await
      },
      None => FileMessageLog::open(channel_dir, self.max_payload_size, self.metrics.clone()).await,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use narwhal_protocol::MessageParameters;
  use narwhal_util::pool::Pool;

  /// Default max payload size for tests.
  const TEST_MAX_PAYLOAD_SIZE: u32 = 8192;

  /// Collected entry from a LogVisitor for test assertions.
  #[derive(Debug, Clone)]
  struct CollectedEntry {
    seq: u64,
    timestamp: u64,
    from: String,
    payload: Vec<u8>,
  }

  /// A test visitor that collects all visited entries.
  struct CollectingVisitor {
    entries: Vec<CollectedEntry>,
  }

  impl CollectingVisitor {
    fn new() -> Self {
      Self { entries: Vec::new() }
    }
  }

  #[async_trait(?Send)]
  impl LogVisitor for CollectingVisitor {
    async fn visit(&mut self, entry: LogEntry<'_>) -> anyhow::Result<()> {
      self.entries.push(CollectedEntry {
        seq: entry.seq,
        timestamp: entry.timestamp,
        from: String::from_utf8_lossy(entry.from).to_string(),
        payload: entry.payload.to_vec(),
      });
      Ok(())
    }
  }

  /// Helper: create a Message::Message with the given fields.
  fn make_message(from: &str, channel: &str, seq: u64, timestamp: u64, payload_len: u32) -> Message {
    Message::Message(MessageParameters {
      from: StringAtom::from(from),
      channel: StringAtom::from(channel),
      length: payload_len,
      seq,
      timestamp,
      history_id: None,
    })
  }

  /// Helper: create a PoolBuffer containing the given bytes.
  async fn make_payload(data: &[u8]) -> PoolBuffer {
    let pool = Pool::new(1, data.len().max(1));
    let mut buf = pool.acquire_buffer().await;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);
    buf.freeze(data.len())
  }

  /// Helper: create a FileMessageLog in a temp directory.
  async fn create_log(dir: &std::path::Path) -> FileMessageLog {
    let factory = FileMessageLogFactory::new(dir.to_path_buf(), TEST_MAX_PAYLOAD_SIZE, MessageLogMetrics::noop());
    factory.create(&StringAtom::from("test_channel")).await.expect("recovery should succeed in tests")
  }

  /// Helper: create a FileMessageLog with a custom segment size threshold.
  async fn create_log_with_segment_max(dir: &std::path::Path, segment_max_bytes: u64) -> FileMessageLog {
    create_log_with_segment_max_and_metrics(dir, segment_max_bytes, MessageLogMetrics::noop()).await
  }

  /// Helper: create a FileMessageLog with a custom segment size threshold and a
  /// caller-supplied metrics struct, so tests can observe metric increments.
  async fn create_log_with_segment_max_and_metrics(
    dir: &std::path::Path,
    segment_max_bytes: u64,
    metrics: MessageLogMetrics,
  ) -> FileMessageLog {
    let hash = channel_hash(&StringAtom::from("test_channel"));
    let channel_dir = dir.join(hash.as_ref());
    FileMessageLog::open_with_segment_max(channel_dir, TEST_MAX_PAYLOAD_SIZE, segment_max_bytes, metrics)
      .await
      .expect("recovery should succeed in tests")
  }

  /// Helper: append a single message with the given seq, from, and payload.
  async fn append_message(log: &FileMessageLog, seq: u64, from: &str, payload: &[u8], max_messages: u32) {
    let msg = make_message(from, "!test@localhost", seq, 1000 + seq, payload.len() as u32);
    let pool_buf = make_payload(payload).await;
    log.append(&msg, &pool_buf, max_messages).await.unwrap();
  }

  // ===== Basic append and read =====

  #[compio::test]
  async fn test_append_single_entry_and_read_back() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 1, "alice@localhost", b"hello world", 100).await;
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 10, &mut visitor).await.unwrap();

    assert_eq!(count, 1);
    assert_eq!(visitor.entries.len(), 1);
    assert_eq!(visitor.entries[0].seq, 1);
    assert_eq!(visitor.entries[0].timestamp, 1001);
    assert_eq!(visitor.entries[0].from, "alice@localhost");
    assert_eq!(visitor.entries[0].payload, b"hello world");
  }

  #[compio::test]
  async fn test_append_multiple_entries_and_read_all() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    for seq in 1..=10 {
      let payload = format!("msg_{seq}");
      append_message(&log, seq, "alice@localhost", payload.as_bytes(), 100).await;
    }
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 100, &mut visitor).await.unwrap();

    assert_eq!(count, 10);
    for (i, entry) in visitor.entries.iter().enumerate() {
      let expected_seq = (i + 1) as u64;
      assert_eq!(entry.seq, expected_seq);
      assert_eq!(entry.payload, format!("msg_{expected_seq}").as_bytes());
    }
  }

  #[compio::test]
  async fn test_read_with_from_seq_skips_earlier_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    for seq in 1..=10 {
      append_message(&log, seq, "alice@localhost", b"data", 100).await;
    }
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    let count = log.read(5, 100, &mut visitor).await.unwrap();

    assert_eq!(count, 6); // seqs 5, 6, 7, 8, 9, 10
    assert_eq!(visitor.entries[0].seq, 5);
    assert_eq!(visitor.entries[5].seq, 10);
  }

  #[compio::test]
  async fn test_read_with_limit_caps_results() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    for seq in 1..=10 {
      append_message(&log, seq, "alice@localhost", b"data", 100).await;
    }
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 3, &mut visitor).await.unwrap();

    assert_eq!(count, 3);
    assert_eq!(visitor.entries[0].seq, 1);
    assert_eq!(visitor.entries[2].seq, 3);
  }

  // ===== first_seq / last_seq tracking =====

  #[compio::test]
  async fn test_empty_log_returns_zero_seqs() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    assert_eq!(log.first_seq(), 0);
    assert_eq!(log.last_seq(), 0);
  }

  #[compio::test]
  async fn test_first_and_last_seq_after_appends() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 1, "alice@localhost", b"a", 100).await;
    assert_eq!(log.first_seq(), 1);
    assert_eq!(log.last_seq(), 1);

    append_message(&log, 2, "alice@localhost", b"b", 100).await;
    assert_eq!(log.first_seq(), 1);
    assert_eq!(log.last_seq(), 2);

    append_message(&log, 3, "alice@localhost", b"c", 100).await;
    assert_eq!(log.first_seq(), 1);
    assert_eq!(log.last_seq(), 3);
  }

  // ===== Eviction =====

  #[compio::test]
  async fn test_eviction_advances_first_seq() {
    let tmp = tempfile::tempdir().unwrap();
    // Use a tiny segment (256 bytes) so that a few entries trigger a roll.
    let log = create_log_with_segment_max(tmp.path(), 256).await;

    // Append 30 messages with max_messages=5. With ~44 bytes per entry and
    // 256-byte segments, we get many segments. Eviction drops whole segments
    // whose last_seq falls outside the retention window.
    for seq in 1..=30 {
      append_message(&log, seq, "alice@localhost", b"data", 5).await;
    }
    log.flush().await.unwrap();

    // first_seq should have advanced (oldest segments evicted).
    assert!(log.first_seq() > 1, "first_seq should advance after eviction, got {}", log.first_seq());
    assert_eq!(log.last_seq(), 30);

    // Reading from first_seq should return the retained messages.
    let mut visitor = CollectingVisitor::new();
    let count = log.read(log.first_seq(), 100, &mut visitor).await.unwrap();
    assert!(count > 0);
    assert_eq!(visitor.entries.last().unwrap().seq, 30);
  }

  #[compio::test]
  async fn test_eviction_removes_segment_files() {
    let tmp = tempfile::tempdir().unwrap();
    // Use a tiny segment (256 bytes) so segment rolls happen frequently.
    let log = create_log_with_segment_max(tmp.path(), 256).await;

    // Use a very small max_messages to force eviction across many appends.
    for seq in 1..=100 {
      append_message(&log, seq, "alice@localhost", b"payload_data_here", 10).await;
    }
    log.flush().await.unwrap();

    assert_eq!(log.last_seq(), 100);
    assert!(log.first_seq() > 1);

    // Only retained messages should be readable.
    let mut visitor = CollectingVisitor::new();
    let count = log.read(log.first_seq(), 1000, &mut visitor).await.unwrap();
    assert!(count <= 10);
  }

  // ===== Segment roll =====

  #[compio::test]
  async fn test_segment_roll_on_size_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    // Use a small segment (512 bytes) so a few entries trigger a roll.
    let log = create_log_with_segment_max(tmp.path(), 512).await;

    for seq in 1..=20 {
      append_message(&log, seq, "alice@localhost", b"segment_roll_data", 10_000).await;
    }
    log.flush().await.unwrap();

    assert_eq!(log.first_seq(), 1);
    assert_eq!(log.last_seq(), 20);

    // Read across segment boundary should return all messages.
    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 100, &mut visitor).await.unwrap();
    assert_eq!(count, 20);
    assert_eq!(visitor.entries[0].seq, 1);
    assert_eq!(visitor.entries[19].seq, 20);
  }

  #[compio::test]
  async fn test_cross_segment_read() {
    let tmp = tempfile::tempdir().unwrap();
    // Use a small segment (256 bytes) so entries span multiple segments.
    let log = create_log_with_segment_max(tmp.path(), 256).await;

    for seq in 1..=50 {
      let payload = format!("cross_seg_{seq:04}");
      append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
    }
    log.flush().await.unwrap();

    // Read starting mid-way through what should be a later segment.
    let mut visitor = CollectingVisitor::new();
    let count = log.read(20, 15, &mut visitor).await.unwrap();

    assert_eq!(count, 15);
    assert_eq!(visitor.entries[0].seq, 20);
    assert_eq!(visitor.entries[14].seq, 34);
  }

  // ===== Sparse index =====

  #[compio::test]
  async fn test_sparse_index_enables_efficient_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    // Append many small messages so the sparse index has multiple entries.
    for seq in 1..=200 {
      let payload = format!("payload_{seq:04}");
      append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
    }
    log.flush().await.unwrap();

    // Read from a seq that should require index lookup (not scanning from start).
    let mut visitor = CollectingVisitor::new();
    let count = log.read(150, 10, &mut visitor).await.unwrap();

    assert_eq!(count, 10);
    assert_eq!(visitor.entries[0].seq, 150);
    assert_eq!(visitor.entries[0].payload, b"payload_0150");
    assert_eq!(visitor.entries[9].seq, 159);
  }

  // ===== Edge cases =====

  #[compio::test]
  async fn test_read_empty_log() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 100, &mut visitor).await.unwrap();

    assert_eq!(count, 0);
    assert!(visitor.entries.is_empty());
  }

  #[compio::test]
  async fn test_read_from_seq_beyond_last_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    for seq in 1..=5 {
      append_message(&log, seq, "alice@localhost", b"data", 100).await;
    }
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    let count = log.read(100, 10, &mut visitor).await.unwrap();

    assert_eq!(count, 0);
    assert!(visitor.entries.is_empty());
  }

  #[compio::test]
  async fn test_read_with_zero_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 1, "alice@localhost", b"data", 100).await;
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 0, &mut visitor).await.unwrap();

    assert_eq!(count, 0);
    assert!(visitor.entries.is_empty());
  }

  #[compio::test]
  async fn test_read_preserves_different_from_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 1, "alice@localhost", b"msg1", 100).await;
    append_message(&log, 2, "bob@localhost", b"msg2", 100).await;
    append_message(&log, 3, "charlie@example.com", b"msg3", 100).await;
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    log.read(1, 100, &mut visitor).await.unwrap();

    assert_eq!(visitor.entries[0].from, "alice@localhost");
    assert_eq!(visitor.entries[1].from, "bob@localhost");
    assert_eq!(visitor.entries[2].from, "charlie@example.com");
  }

  #[compio::test]
  async fn test_read_preserves_variable_payload_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 1, "alice@localhost", b"", 100).await;
    append_message(&log, 2, "alice@localhost", b"short", 100).await;
    let large = vec![0xFFu8; 4096];
    append_message(&log, 3, "alice@localhost", &large, 100).await;
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    log.read(1, 100, &mut visitor).await.unwrap();

    assert_eq!(visitor.entries[0].payload.len(), 0);
    assert_eq!(visitor.entries[1].payload, b"short");
    assert_eq!(visitor.entries[2].payload.len(), 4096);
  }

  // ===== Size validation =====

  #[compio::test]
  async fn test_append_rejects_oversized_from() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    let oversized_from = "a".repeat(NID_MAX_LENGTH + 1);
    let msg = make_message(&oversized_from, "!test@localhost", 1, 1000, 4);
    let pool_buf = make_payload(b"data").await;
    let err = log.append(&msg, &pool_buf, 100).await.unwrap_err();
    assert!(err.to_string().contains("from field too large"), "unexpected error: {err}");

    // Log state is unchanged: a subsequent valid append still uses seq 1.
    assert_eq!(log.last_seq(), 0);
    append_message(&log, 1, "alice@localhost", b"data", 100).await;
    assert_eq!(log.last_seq(), 1);
  }

  #[compio::test]
  async fn test_append_rejects_oversized_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    let oversized = vec![0xABu8; TEST_MAX_PAYLOAD_SIZE as usize + 1];
    let msg = make_message("alice@localhost", "!test@localhost", 1, 1000, oversized.len() as u32);
    let pool_buf = make_payload(&oversized).await;
    let err = log.append(&msg, &pool_buf, 100).await.unwrap_err();
    assert!(err.to_string().contains("payload too large"), "unexpected error: {err}");

    // Log state is unchanged: a subsequent valid append still uses seq 1.
    assert_eq!(log.last_seq(), 0);
    append_message(&log, 1, "alice@localhost", b"data", 100).await;
    assert_eq!(log.last_seq(), 1);
  }

  #[compio::test]
  async fn test_append_rejects_zero_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    let msg = make_message("alice@localhost", "!test@localhost", 0, 1000, 4);
    let pool_buf = make_payload(b"data").await;
    let err = log.append(&msg, &pool_buf, 100).await.unwrap_err();
    assert!(err.to_string().contains("seq must be > 0"), "unexpected error: {err}");

    // Log state is unchanged: still empty, so a fresh append at seq=1 works.
    assert_eq!(log.first_seq(), 0);
    assert_eq!(log.last_seq(), 0);
    append_message(&log, 1, "alice@localhost", b"data", 100).await;
    assert_eq!(log.last_seq(), 1);
  }

  #[compio::test]
  async fn test_append_rejects_non_monotonic_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 5, "alice@localhost", b"first", 100).await;

    // Same seq.
    let dup = make_message("alice@localhost", "!test@localhost", 5, 1000, 4);
    let buf = make_payload(b"dup!").await;
    let err = log.append(&dup, &buf, 100).await.unwrap_err();
    assert!(err.to_string().contains("strictly greater than last_seq"), "unexpected error: {err}");

    // Lower seq.
    let lower = make_message("alice@localhost", "!test@localhost", 3, 1000, 4);
    let buf = make_payload(b"low!").await;
    let err = log.append(&lower, &buf, 100).await.unwrap_err();
    assert!(err.to_string().contains("strictly greater than last_seq"), "unexpected error: {err}");

    // last_seq unchanged.
    assert_eq!(log.last_seq(), 5);

    // Higher seq still works.
    append_message(&log, 6, "alice@localhost", b"next", 100).await;
    assert_eq!(log.last_seq(), 6);
  }

  #[compio::test]
  async fn test_append_accepts_max_sized_from_and_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    let max_from = "a".repeat(NID_MAX_LENGTH);
    let max_payload = vec![0xCDu8; TEST_MAX_PAYLOAD_SIZE as usize];
    let msg = make_message(&max_from, "!test@localhost", 1, 1000, max_payload.len() as u32);
    let pool_buf = make_payload(&max_payload).await;
    log.append(&msg, &pool_buf, 100).await.unwrap();
    log.flush().await.unwrap();

    let mut visitor = CollectingVisitor::new();
    log.read(1, 10, &mut visitor).await.unwrap();
    assert_eq!(visitor.entries.len(), 1);
    assert_eq!(visitor.entries[0].from.len(), NID_MAX_LENGTH);
    assert_eq!(visitor.entries[0].payload.len(), TEST_MAX_PAYLOAD_SIZE as usize);
  }

  // ===== Delete =====

  #[compio::test]
  async fn test_delete_clears_all_state() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    for seq in 1..=10 {
      append_message(&log, seq, "alice@localhost", b"data", 100).await;
    }
    log.flush().await.unwrap();

    assert_eq!(log.last_seq(), 10);

    log.delete().await.unwrap();

    assert_eq!(log.first_seq(), 0);
    assert_eq!(log.last_seq(), 0);

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 100, &mut visitor).await.unwrap();
    assert_eq!(count, 0);
  }

  // ===== Flush =====

  #[compio::test]
  async fn test_unflushed_data_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    append_message(&log, 1, "alice@localhost", b"no flush yet", 100).await;
    // Intentionally do NOT call flush.

    let mut visitor = CollectingVisitor::new();
    let count = log.read(1, 10, &mut visitor).await.unwrap();

    assert_eq!(count, 1);
    assert_eq!(visitor.entries[0].payload, b"no flush yet");
  }

  // ===== CRC32 integrity =====

  #[compio::test]
  async fn test_corrupt_entry_detected_on_read() {
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log(tmp.path()).await;

    for seq in 1..=5 {
      append_message(&log, seq, "alice@localhost", b"payload", 100).await;
    }
    log.flush().await.unwrap();

    // Corrupt the middle of the log file by flipping some bytes.
    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    // Find the first .log file and corrupt bytes in the middle.
    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
      .collect();
    log_files.sort_by_key(|e| e.file_name());

    if let Some(log_file) = log_files.first() {
      let mut data = std::fs::read(log_file.path()).unwrap();
      // Corrupt bytes somewhere in the middle of the file.
      let mid = data.len() / 2;
      if mid < data.len() {
        data[mid] ^= 0xFF;
        data[mid + 1] ^= 0xFF;
        std::fs::write(log_file.path(), &data).unwrap();
      }
    }

    // Reading should either return fewer entries (truncated at corruption)
    // or return an error.
    let mut visitor = CollectingVisitor::new();
    let result = log.read(1, 100, &mut visitor).await;

    // Either it errors or it returns fewer entries than we wrote.
    if let Ok(count) = result {
      assert!(count < 5, "corrupt entry should truncate results, got {count}");
    }
  }

  // ===== Recovery =====

  #[compio::test]
  async fn test_recovery_after_simulated_crash() {
    let tmp = tempfile::tempdir().unwrap();

    // First session: append messages and flush.
    {
      let log = create_log(tmp.path()).await;
      for seq in 1..=10 {
        append_message(&log, seq, "alice@localhost", b"persistent", 100).await;
      }
      log.flush().await.unwrap();
    }

    // Second session: create a new log from the same directory — should recover.
    {
      let log = create_log(tmp.path()).await;

      assert_eq!(log.first_seq(), 1);
      assert_eq!(log.last_seq(), 10);

      let mut visitor = CollectingVisitor::new();
      let count = log.read(1, 100, &mut visitor).await.unwrap();

      assert_eq!(count, 10);
      assert_eq!(visitor.entries[0].seq, 1);
      assert_eq!(visitor.entries[9].seq, 10);
      assert_eq!(visitor.entries[0].payload, b"persistent");
    }
  }

  #[compio::test]
  async fn test_recovery_truncates_partial_entry() {
    let tmp = tempfile::tempdir().unwrap();

    // First session: append messages and flush.
    {
      let log = create_log(tmp.path()).await;
      for seq in 1..=5 {
        append_message(&log, seq, "alice@localhost", b"complete", 100).await;
      }
      log.flush().await.unwrap();
    }

    // Simulate crash by appending garbage to the end of the log file.
    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
      .collect();
    log_files.sort_by_key(|e| e.file_name());

    if let Some(log_file) = log_files.last() {
      use std::io::Write;
      let mut f = std::fs::OpenOptions::new().append(true).open(log_file.path()).unwrap();
      // Write a partial/garbage entry at the tail.
      f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02]).unwrap();
    }

    // Second session: should recover, truncating the partial entry.
    {
      let log = create_log(tmp.path()).await;

      assert_eq!(log.last_seq(), 5);

      let mut visitor = CollectingVisitor::new();
      let count = log.read(1, 100, &mut visitor).await.unwrap();

      assert_eq!(count, 5);
      assert_eq!(visitor.entries[4].seq, 5);
      assert_eq!(visitor.entries[4].payload, b"complete");
    }
  }

  #[compio::test]
  async fn test_recovery_increments_metric_on_sealed_segment_corruption() {
    // Regression test: a CRC failure mid-way through a sealed segment is
    // silent data loss — the bytes past the failure are unreachable but the
    // file stays on disk. Recovery must surface this as both a metric
    // increment and a tracing warning so disk health can be investigated.
    //
    // The test uses a small `segment_max_bytes` to roll into a sealed
    // segment quickly, corrupts a byte in the middle of that sealed
    // segment's `.log`, then re-opens with a shared metrics handle and
    // asserts the truncation counter advanced and `last_seq` stops at the
    // pre-corruption entry.
    let tmp = tempfile::tempdir().unwrap();
    let metrics = MessageLogMetrics::noop();

    let appended = 8u64;
    {
      let log = create_log_with_segment_max_and_metrics(tmp.path(), 4096, metrics.clone()).await;
      for seq in 1..=appended {
        // ~1054-byte entries → roll fires after ~4 entries so segment 1 ends
        // up sealed with multiple entries we can corrupt mid-file.
        append_message(&log, seq, "alice@localhost", &vec![0u8; 1024], 10_000).await;
      }
      log.flush().await.unwrap();
    }

    // The shared truncation counter is incremented during recovery when a
    // sealed segment is truncated. The first recovery saw clean segments,
    // so it must still be zero here.
    assert_eq!(metrics.sealed_segment_truncations.get(), 0);

    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    // Find sealed segments (every .log except the newest).
    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
      .collect();
    log_files.sort_by_key(|e| e.file_name());
    assert!(log_files.len() >= 2, "test setup expected at least one sealed segment + one active");

    // Flip a byte deep inside the first sealed segment so the CRC of an
    // entry in the middle of the file fails. The file's tail bytes are
    // still bit-perfect on disk, so recovery's mid-segment scan will stop
    // before EOF.
    let sealed_path = log_files[0].path();
    let sealed_size = std::fs::metadata(&sealed_path).unwrap().len();
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).read(true).open(&sealed_path).unwrap();
    f.seek(SeekFrom::Start(sealed_size / 2)).unwrap();
    f.write_all(&[0xFF]).unwrap();
    f.sync_all().unwrap();

    {
      let log = create_log_with_segment_max_and_metrics(tmp.path(), 4096, metrics.clone()).await;
      // Recovery should have detected the truncation on the sealed segment.
      assert_eq!(metrics.sealed_segment_truncations.get(), 1, "expected one sealed-segment truncation to be recorded");

      // Reads of the corrupted segment terminate at the last validated
      // entry, so a sweep over the full range returns fewer entries than
      // were originally appended. The active segment is untouched, so its
      // entries remain visible — `last_seq()` reports the highest seq, but
      // the *count* visited is short by the entries lost past the
      // corruption boundary.
      let mut visitor = CollectingVisitor::new();
      let visited = log.read(1, 1_000, &mut visitor).await.unwrap();
      assert!(
        (visited as u64) < appended,
        "visited ({visited}) should be fewer than appended ({appended}) after sealed corruption"
      );
    }
  }

  #[compio::test]
  async fn test_recovery_does_not_promote_sealed_segment_when_active_invalid() {
    // Regression test: if the active (last) segment validates to zero entries,
    // recovery must delete it without "promoting" the previous sealed segment
    // — otherwise subsequent appends would extend a sealed segment's .log and
    // remap its .idx as MmapMut, corrupting filename-derived first_seq.
    let tmp = tempfile::tempdir().unwrap();

    // Session 1: create several segments.
    let appended = 30u64;
    {
      let log = create_log_with_segment_max(tmp.path(), 256).await;
      for seq in 1..=appended {
        let payload = format!("msg_{seq:03}");
        append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
      }
      log.flush().await.unwrap();
    }

    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    // Find sealed (penultimate) and active (last) segment paths.
    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == SEGMENT_EXT))
      .collect();
    log_files.sort_by_key(|e| e.file_name());
    assert!(log_files.len() >= 2, "expected multiple segments, got {}", log_files.len());

    let active_log_path = log_files.last().unwrap().path();
    let active_idx_path = active_log_path.with_extension(INDEX_EXT);
    let sealed_log_path = log_files[log_files.len() - 2].path();
    let sealed_idx_path = sealed_log_path.with_extension(INDEX_EXT);

    // Snapshot the sealed segment's bytes before corrupting the active. The
    // assertions below compare full file contents (not just sizes) so the
    // test catches same-size mutations as well as resizes.
    let sealed_log_before = std::fs::read(&sealed_log_path).unwrap();
    let sealed_idx_before = std::fs::read(&sealed_idx_path).unwrap();

    // Truncate the active .log to a partial header (< ENTRY_HEADER_SIZE) so
    // scan_and_validate reports zero valid entries.
    {
      let f = std::fs::OpenOptions::new().write(true).open(&active_log_path).unwrap();
      f.set_len(10).unwrap();
    }

    // Session 2: recover.
    let log = create_log(tmp.path()).await;

    // Active segment files should be deleted.
    assert!(!active_log_path.exists(), "active .log should have been removed");
    assert!(!active_idx_path.exists(), "active .idx should have been removed");

    // Sealed segment files must be byte-for-byte unchanged. Without the fix,
    // recovery would (a) open the sealed .log for writes and (b) extend the
    // sealed .idx to ~384 KB via set_len(capacity).
    assert_eq!(std::fs::read(&sealed_log_path).unwrap(), sealed_log_before, "sealed .log was modified");
    assert_eq!(std::fs::read(&sealed_idx_path).unwrap(), sealed_idx_before, "sealed .idx was modified");

    // Append after recovery must create a NEW segment, not extend the sealed one.
    let new_seq = appended + 100;
    append_message(&log, new_seq, "alice@localhost", b"after_recovery", 10_000).await;
    log.flush().await.unwrap();

    // Sealed segment is still byte-for-byte unchanged on disk.
    assert_eq!(std::fs::read(&sealed_log_path).unwrap(), sealed_log_before);
    assert_eq!(std::fs::read(&sealed_idx_path).unwrap(), sealed_idx_before);

    // A new segment file exists named after `new_seq`.
    let new_seg_path = channel_dir.join(format!("{new_seq:020}.{SEGMENT_EXT}"));
    assert!(new_seg_path.exists(), "expected a fresh segment file at {new_seg_path:?}");

    // last_seq reflects the appended value, and the new entry is readable.
    assert_eq!(log.last_seq(), new_seq);
    let mut visitor = CollectingVisitor::new();
    log.read(new_seq, 10, &mut visitor).await.unwrap();
    assert_eq!(visitor.entries.len(), 1);
    assert_eq!(visitor.entries[0].seq, new_seq);
    assert_eq!(visitor.entries[0].payload, b"after_recovery");
  }

  #[compio::test]
  async fn test_recovery_rebuilds_missing_index() {
    let tmp = tempfile::tempdir().unwrap();

    // First session: append enough messages to generate index entries.
    {
      let log = create_log(tmp.path()).await;
      for seq in 1..=100 {
        let payload = format!("msg_{seq:04}");
        append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
      }
      log.flush().await.unwrap();
    }

    // Delete all .idx files.
    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    for entry in std::fs::read_dir(&channel_dir).unwrap().flatten() {
      if entry.path().extension().is_some_and(|ext| ext == "idx") {
        std::fs::remove_file(entry.path()).unwrap();
      }
    }

    // Second session: should rebuild the index and work correctly.
    {
      let log = create_log(tmp.path()).await;

      assert_eq!(log.first_seq(), 1);
      assert_eq!(log.last_seq(), 100);

      // Verify reads still work with rebuilt index.
      let mut visitor = CollectingVisitor::new();
      let count = log.read(50, 10, &mut visitor).await.unwrap();

      assert_eq!(count, 10);
      assert_eq!(visitor.entries[0].seq, 50);
      assert_eq!(visitor.entries[0].payload, b"msg_0050");
    }
  }

  #[compio::test]
  async fn test_recovery_rebuilds_corrupt_sealed_index() {
    // Spec ("Recovery / Guarantees") promises index corruption is recoverable
    // by scanning the log. Without sanity-checking the .idx at load time, a
    // corrupted-but-present sealed index would be silently mmapped and the
    // read path would seek past EOF, returning empty results from valid .log
    // data. Verify that recovery rebuilds an index whose offsets are bogus.
    let tmp = tempfile::tempdir().unwrap();

    // Session 1: generate several sealed segments by rolling on a small max.
    {
      let log = create_log_with_segment_max(tmp.path(), 256).await;
      for seq in 1..=40 {
        let payload = format!("msg_{seq:03}");
        append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
      }
      log.flush().await.unwrap();
    }

    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    // Pick a sealed (non-last) .idx and corrupt it so the last offset points
    // past the corresponding .log file size.
    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
      .collect();
    log_files.sort_by_key(|e| e.file_name());
    assert!(log_files.len() >= 3, "expected at least 3 segments, got {}", log_files.len());

    let sealed_log_path = log_files[0].path();
    let sealed_idx_path = sealed_log_path.with_extension("idx");
    let sealed_log_size = std::fs::metadata(&sealed_log_path).unwrap().len();

    // Overwrite the .idx with a single entry whose offset is past EOF.
    let bogus_offset = sealed_log_size + 1_000_000;
    let mut bogus_idx = Vec::with_capacity(INDEX_ENTRY_SIZE);
    bogus_idx.extend_from_slice(&0u32.to_le_bytes());
    bogus_idx.extend_from_slice(&bogus_offset.to_le_bytes());
    std::fs::write(&sealed_idx_path, &bogus_idx).unwrap();

    // Session 2: recovery should detect the bad offset and rebuild the index.
    let log = create_log(tmp.path()).await;

    assert_eq!(log.first_seq(), 1);
    assert_eq!(log.last_seq(), 40);

    // Read starting mid-segment (from_seq > segment's first_seq) so that the
    // read path actually consults the index. Without the rebuild, the bogus
    // offset lands past EOF and the corrupted segment yields zero entries —
    // visitor.entries[0].seq would be the next segment's first seq instead
    // of 2.
    let mut visitor = CollectingVisitor::new();
    let count = log.read(2, 10, &mut visitor).await.unwrap();
    assert!(count >= 10, "expected at least 10 entries, got {count}");
    assert_eq!(visitor.entries[0].seq, 2, "first segment was skipped (corrupt index not rebuilt)");
    assert_eq!(visitor.entries[0].payload, b"msg_002");
  }

  #[compio::test]
  async fn test_recovery_rebuilds_non_monotonic_sealed_index() {
    // A non-monotonic .idx is invalid by construction even if every offset
    // is in-range. Binary search may incidentally tolerate some misorderings,
    // so we assert that the structural check actually rebuilt the file
    // (its bytes no longer match the bogus content we wrote).
    let tmp = tempfile::tempdir().unwrap();

    {
      let log = create_log_with_segment_max(tmp.path(), 256).await;
      for seq in 1..=20 {
        let payload = format!("msg_{seq:03}");
        append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
      }
      log.flush().await.unwrap();
    }

    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    // Pick the first sealed segment.
    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == SEGMENT_EXT))
      .collect();
    log_files.sort_by_key(|e| e.file_name());
    assert!(log_files.len() >= 2, "expected at least 2 segments");

    let sealed_log_path = log_files[0].path();
    let sealed_idx_path = sealed_log_path.with_extension(INDEX_EXT);
    let sealed_log_size = std::fs::metadata(&sealed_log_path).unwrap().len();
    assert!(sealed_log_size > 40, "test assumes sealed log > 40 bytes");

    // Three entries, all in-range, but rel_seq goes backwards (0 → 5 → 3)
    // and so does offset (0 → 40 → 20). Passes every other check.
    let mut bad_idx = Vec::with_capacity(3 * INDEX_ENTRY_SIZE);
    bad_idx.extend_from_slice(&0u32.to_le_bytes());
    bad_idx.extend_from_slice(&0u64.to_le_bytes());
    bad_idx.extend_from_slice(&5u32.to_le_bytes());
    bad_idx.extend_from_slice(&40u64.to_le_bytes());
    bad_idx.extend_from_slice(&3u32.to_le_bytes());
    bad_idx.extend_from_slice(&20u64.to_le_bytes());
    std::fs::write(&sealed_idx_path, &bad_idx).unwrap();

    // Session 2: recovery must rebuild this index.
    let _log = create_log(tmp.path()).await;
    let after = std::fs::read(&sealed_idx_path).unwrap();
    assert_ne!(after, bad_idx, "non-monotonic .idx should have been rebuilt");
  }

  #[compio::test]
  async fn test_recovery_rebuilds_sealed_index_when_last_offset_no_room_for_header() {
    // Regression for the tightened bound `offset + ENTRY_HEADER_SIZE <=
    // log_file_size`. An offset that is < log_file_size but leaves no room
    // for a full entry header passes the older `< log_file_size` check, but
    // EntryReader::read_at would fail at runtime. The new bound rejects the
    // index; recovery rebuilds it so reads still work.
    let tmp = tempfile::tempdir().unwrap();
    {
      let log = create_log_with_segment_max(tmp.path(), 256).await;
      for seq in 1..=20 {
        let payload = format!("msg_{seq:03}");
        append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
      }
      log.flush().await.unwrap();
    }

    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == SEGMENT_EXT))
      .collect();
    log_files.sort_by_key(|e| e.file_name());
    assert!(log_files.len() >= 2);
    let sealed_log_path = log_files[0].path();
    let sealed_idx_path = sealed_log_path.with_extension(INDEX_EXT);
    let sealed_log_size = std::fs::metadata(&sealed_log_path).unwrap().len();
    assert!(
      sealed_log_size > ENTRY_HEADER_SIZE as u64,
      "test assumes the sealed segment holds at least one full entry"
    );

    // Two valid-looking entries (monotonic, in-range), but the second's
    // offset is the smallest value that's `< log_size` while still failing
    // `+ ENTRY_HEADER_SIZE <= log_size`. Derive it from `ENTRY_HEADER_SIZE`
    // so the test stays correct if the header layout ever changes.
    let bad_offset = sealed_log_size - ENTRY_HEADER_SIZE as u64 + 1;
    let mut bad_idx = Vec::with_capacity(2 * INDEX_ENTRY_SIZE);
    bad_idx.extend_from_slice(&0u32.to_le_bytes());
    bad_idx.extend_from_slice(&0u64.to_le_bytes());
    bad_idx.extend_from_slice(&1u32.to_le_bytes());
    bad_idx.extend_from_slice(&bad_offset.to_le_bytes());
    std::fs::write(&sealed_idx_path, &bad_idx).unwrap();

    // Recovery must rebuild this index.
    let _log = create_log(tmp.path()).await;
    let after = std::fs::read(&sealed_idx_path).unwrap();
    assert_ne!(after, bad_idx, ".idx whose last offset leaves no room for a header should have been rebuilt");
  }

  #[compio::test]
  async fn test_read_does_not_fall_through_to_active_idx_when_sealed_idx_missing() {
    // Regression test: when a sealed segment has `idx_mmap == None` (e.g.
    // mmap_index failed earlier) the read path must NOT consult
    // active_idx_mmap, whose offsets belong to a different segment. We force
    // that scenario in-process by:
    //   - building a multi-segment log,
    //   - clearing the first sealed segment's `idx_mmap`,
    //   - planting a deliberately bogus offset (past EOF for segment 0) into
    //     active_idx_mmap entry-0.
    // Without the fix, the read path uses the bogus offset and yields zero
    // entries from the affected segment; with the fix it falls back to a
    // full scan and returns the correct entries.
    let tmp = tempfile::tempdir().unwrap();
    let log = create_log_with_segment_max(tmp.path(), 256).await;
    for seq in 1..=20 {
      let payload = format!("msg_{seq:03}");
      append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
    }
    log.flush().await.unwrap();

    {
      let mut inner = log.inner.borrow_mut();
      assert!(inner.segments.len() >= 2, "need multiple segments");
      // Clear the first sealed segment's mmap.
      inner.segments[0].idx_mmap = None;

      // Plant a bogus offset (past segment 0's file_size) in active_idx_mmap
      // entry-0 so any fallthrough lookup returns it.
      let seg0_size = inner.segments[0].file_size;
      let bogus = seg0_size + 1_000_000;
      let mmap = inner.active_idx_mmap.as_mut().expect("active mmap should be set in tests");
      // entry-0: relative_seq(4) at [0..4] = 0, offset(8) at [4..12] = bogus
      mmap[0..4].copy_from_slice(&0u32.to_le_bytes());
      mmap[4..12].copy_from_slice(&bogus.to_le_bytes());
      // Make sure the binary search sees at least the planted entry.
      if inner.active_idx_write_pos < INDEX_ENTRY_SIZE {
        inner.active_idx_write_pos = INDEX_ENTRY_SIZE;
      }
    }

    // Read mid-segment to force an index lookup on segment 0.
    let mut visitor = CollectingVisitor::new();
    let count = log.read(2, 5, &mut visitor).await.unwrap();
    assert_eq!(count, 5);
    assert_eq!(visitor.entries[0].seq, 2, "fall-through to active_idx_mmap dropped segment 0 reads");
    assert_eq!(visitor.entries[0].payload, b"msg_002");
  }

  #[compio::test]
  async fn test_rebuild_index_uses_atomic_rename() {
    // The rebuild path writes through a `.tmp` sibling and renames it to the
    // final `.idx`. After a successful rebuild the `.tmp` must not be left
    // on disk, and a stale `.tmp` from a prior crashed run must not prevent
    // the new rebuild from succeeding.
    let tmp = tempfile::tempdir().unwrap();
    {
      let log = create_log_with_segment_max(tmp.path(), 256).await;
      for seq in 1..=20 {
        let payload = format!("msg_{seq:03}");
        append_message(&log, seq, "alice@localhost", payload.as_bytes(), 10_000).await;
      }
      log.flush().await.unwrap();
    }

    let channel_dir = {
      let hash = channel_hash(&StringAtom::from("test_channel"));
      tmp.path().join(hash.as_ref())
    };

    // Pick the first sealed segment, corrupt its .idx, and plant a stale
    // .tmp from a prior run at the path the rebuild would use.
    let mut log_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == SEGMENT_EXT))
      .collect();
    log_files.sort_by_key(|e| e.file_name());
    assert!(log_files.len() >= 2);
    let sealed_log_path = log_files[0].path();
    let sealed_idx_path = sealed_log_path.with_extension(INDEX_EXT);
    let stale_tmp_path = sealed_log_path.with_extension("tmp");

    // Corrupt the .idx so recovery triggers a rebuild.
    let mut bad_idx = Vec::with_capacity(INDEX_ENTRY_SIZE);
    bad_idx.extend_from_slice(&0u32.to_le_bytes());
    bad_idx.extend_from_slice(&u64::MAX.to_le_bytes());
    std::fs::write(&sealed_idx_path, &bad_idx).unwrap();
    std::fs::write(&stale_tmp_path, b"junk left over from a prior crash").unwrap();

    // Recovery should rebuild the .idx atomically.
    let _log = create_log(tmp.path()).await;

    // The rebuild produced a valid .idx (different from the bogus content).
    let after = std::fs::read(&sealed_idx_path).unwrap();
    assert_ne!(after, bad_idx, ".idx should have been rebuilt");

    // No .tmp files should be left in the channel directory after a
    // successful rebuild — the rename consumes the temp file.
    let tmp_files: Vec<_> = std::fs::read_dir(&channel_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
      .collect();
    assert!(tmp_files.is_empty(), "leftover .tmp files: {tmp_files:?}");
  }

  // ===== Factory =====

  #[compio::test]
  async fn test_factory_creates_independent_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let factory =
      FileMessageLogFactory::new(tmp.path().to_path_buf(), TEST_MAX_PAYLOAD_SIZE, MessageLogMetrics::noop());

    let log_a = factory.create(&StringAtom::from("channel_a")).await.unwrap();
    let log_b = factory.create(&StringAtom::from("channel_b")).await.unwrap();

    append_message(&log_a, 1, "alice@localhost", b"for_a", 100).await;
    append_message(&log_b, 1, "bob@localhost", b"for_b", 100).await;
    log_a.flush().await.unwrap();
    log_b.flush().await.unwrap();

    let mut visitor_a = CollectingVisitor::new();
    log_a.read(1, 10, &mut visitor_a).await.unwrap();
    assert_eq!(visitor_a.entries[0].from, "alice@localhost");
    assert_eq!(visitor_a.entries[0].payload, b"for_a");

    let mut visitor_b = CollectingVisitor::new();
    log_b.read(1, 10, &mut visitor_b).await.unwrap();
    assert_eq!(visitor_b.entries[0].from, "bob@localhost");
    assert_eq!(visitor_b.entries[0].payload, b"for_b");
  }
}
