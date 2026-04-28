# Message Log — Architecture Specification

> **Status:** Implemented.
> **Related PRs:** [#221](https://github.com/narwhal-io/narwhal/pull/221) (HISTORY/CHAN_SEQ protocol), [#227](https://github.com/narwhal-io/narwhal/pull/227) (FileMessageLog implementation)

## Table of Contents

- [Overview](#overview)
- [Goals and Non-Goals](#goals-and-non-goals)
- [On-Disk Layout](#on-disk-layout)
  - [Directory Structure](#directory-structure)
  - [Segment Files](#segment-files)
  - [Log Entry Format](#log-entry-format)
  - [Sparse Index Files](#sparse-index-files)
- [Trait API](#trait-api)
  - [MessageLog Trait](#messagelog-trait)
  - [LogVisitor Pattern](#logvisitor-pattern)
  - [MessageLogFactory](#messagelogfactory)
- [Write Path](#write-path)
  - [Append](#append)
  - [Flush](#flush)
  - [Segment Roll](#segment-roll)
- [Read Path](#read-path)
  - [Index Lookup](#index-lookup)
  - [EntryReader — Zero-Allocation Positioned Reads](#entryreader--zero-allocation-positioned-reads)
  - [Visitor Invocation](#visitor-invocation)
- [Eviction](#eviction)
- [Recovery](#recovery)
- [Integration](#integration)
  - [HISTORY Flow](#history-flow)
  - [CHAN_SEQ Flow](#chan_seq-flow)
- [Constants](#constants)
- [Testing](#testing)

---

## Overview

The message log is a per-channel, segmented, append-only storage engine that
persists broadcast messages for channels with persistence enabled. It serves two
protocol operations introduced in PR #221:

- **`HISTORY`** — retrieve archived messages from a channel.
- **`CHAN_SEQ`** — query the available sequence range of a channel's log.

Each persistent channel maintains its own message log as a set of segment files
with companion sparse index files, stored in the channel's existing directory.

```
┌──────────────────────────────────────────────────────────┐
│                     Client                               │
│                                                          │
│   CHAN_SEQ ──► first_seq / last_seq                      │
│   HISTORY ──► MESSAGE(history_id) stream + HISTORY_ACK   │
└────────────────────────┬─────────────────────────────────┘
                         │
                         ▼
┌──────────────────────────────────────────────────────────┐
│                  ChannelShard                            │
│                                                          │
│   ┌─────────────┐    ┌──────────────┐                    │
│   │   Channel    │───►│  MessageLog   │                  │
│   │  (in-memory) │    │  (segments)   │                  │
│   └─────────────┘    └──────┬───────┘                    │
│                             │                            │
│              ┌──────────────┼──────────────┐             │
│              ▼              ▼              ▼             │
│         .log + .idx    .log + .idx    .log + .idx        │
│         (segment 1)    (segment 2)    (segment N)        │
└──────────────────────────────────────────────────────────┘
```

## Goals and Non-Goals

**Goals:**
- Durable, ordered storage of broadcast payloads for persistent channels.
- Efficient seq-based lookups via sparse indexing.
- Zero-allocation read path (`EntryReader` with pre-allocated buffers, borrowed data in visitor).
- Crash-safe: detect and truncate partial writes on recovery.
- Simple eviction: delete entire segment files.

**Non-Goals:**
- Time-based retention (eviction is count-based via `max_persist_messages`).
- Cross-channel queries or global indexing.
- Replication or distributed log semantics.

## On-Disk Layout

### Directory Structure

Message log files share the channel directory established by `FileChannelStore`.
The directory is derived from a SHA-256 hash of the channel handler name:

```
<base_dir>/<sha256(handler)>/
  metadata.bin                        ← channel metadata (FileChannelStore)
  00000000000000000001.log            ← first segment (named by first seq, 20-digit zero-padded)
  00000000000000000001.idx            ← first segment's sparse index
  00000000000000000257.log            ← second segment
  00000000000000000257.idx            ← second segment's sparse index
  ...
```

A shared path utility computes `<sha256(handler)>` so both `FileChannelStore`
and `FileMessageLog` always agree on the directory.

### Segment Files

Segments are append-only binary files. A new segment is created when the active
segment exceeds the size threshold.

| Property         | Value              |
|------------------|--------------------|
| Max size         | 128 MiB            |
| Naming           | First seq, 20-digit zero-padded (e.g., `00000000000000000001.log`) |
| Roll trigger     | Checked **after** each append |
| Overshoot        | Up to one entry beyond 128 MiB (bounded by `max_payload_size`) |

### Log Entry Format

Each entry is a self-contained binary record:

```
┌─────────┬───────────┬──────────┬─────────────┬──────┬─────────┬────────┐
│ seq     │ timestamp │ from_len │ payload_len │ from │ payload │ crc32  │
│ 8 bytes │ 8 bytes   │ 2 bytes  │ 4 bytes     │ var  │ var     │ 4 bytes│
└─────────┴───────────┴──────────┴─────────────┴──────┴─────────┴────────┘
```

| Field         | Type   | Description |
|---------------|--------|-------------|
| `seq`         | `u64`  | Monotonically increasing sequence number |
| `timestamp`   | `u64`  | Unix timestamp at broadcast time |
| `from_len`    | `u16`  | Length of the `from` field in bytes |
| `payload_len` | `u32`  | Length of the `payload` field in bytes |
| `from`        | bytes  | Sender NID (e.g., `user@domain`) |
| `payload`     | bytes  | Message payload (raw bytes) |
| `crc32`       | `u32`  | CRC32 checksum over all preceding fields in the entry |

**Fixed overhead per entry: 26 bytes.**

**Omitted fields:**
- **`channel`** — implicit from the directory/file path.
- **`history_id`** — injected by the caller at read time, not a property of the stored message.

### Sparse Index Files

Each segment has a companion `.idx` file following the
[Kafka](https://kafka.apache.org/) sparse index model, managed via
memory-mapped files (`mmap`) for efficient access.

| Property         | Value              |
|------------------|--------------------|
| Interval         | Every **4096 bytes** of log data written |
| Entry-0 rule     | An index entry is always written at offset 0 of each segment, regardless of the interval |
| Header           | None (Kafka-style) |
| CRC              | None (derived data; rebuilt from `.log` if corrupt) |

**Index entry format:**

```
┌────────────────┬───────────────┐
│ relative_seq   │ offset        │
│ 4 bytes (u32)  │ 8 bytes (u64) │
└────────────────┴───────────────┘
```

| Field          | Type  | Description |
|----------------|-------|-------------|
| `relative_seq` | `u32` | Seq number relative to the segment's base seq (from filename) |
| `offset`       | `u64` | Byte offset within the `.log` file |

**12 bytes per index entry.**

The first seq of a segment is derived from its filename. The last seq of a
sealed segment is determined by scanning the `.log` file with CRC validation
(populating `SegmentInfo.last_seq`). The active segment's last seq is tracked
in-memory and updated on each append.

#### Memory-Mapped Index Management (Kafka-style)

Index files are accessed via `mmap` (using the `memmap2` crate), mirroring how
Apache Kafka manages its offset indexes. There are two distinct modes depending
on whether the segment is active or sealed:

**Active segment — read-write `MmapMut`:**

When a new segment is created, its `.idx` file is **pre-allocated** to the
maximum capacity it could ever need, then memory-mapped read-write:

```
capacity = (segment_max_bytes / INDEX_INTERVAL_BYTES + 1) * INDEX_ENTRY_SIZE
```

For the default 128 MiB segments with 4096-byte index interval:
`(128 * 1024 * 1024 / 4096 + 1) * 12 ≈ 384 KB`.

The `+ 1` term accounts for the entry-0 rule above: every segment writes an
index entry at offset 0 regardless of the interval, so the upper bound on
entries is `(segment_max_bytes / INDEX_INTERVAL_BYTES) + 1`. Changing either
the formula or the entry-0 rule without updating the other can silently
overflow the pre-allocated capacity.

New index entries are written directly into the mmap at the current write
position (`active_idx_write_pos`). This avoids `write()` syscalls for index
updates — the kernel handles page dirtying and writeback. The active `.idx`
file handle is kept open and durability is enforced with async
`sync_all().await` on that file (instead of synchronous `mmap.flush()`), so
flush/roll do not block the shard runtime thread.

**Sealed segments — read-only `Mmap`:**

When a segment is rolled (finalized), the index lifecycle is:

1. `sync_all()` the active `.idx` file.
2. Drop the active `MmapMut`.
3. Truncate the `.idx` file from its pre-allocated size to the actual written
   size (`active_idx_write_pos` bytes).
4. Re-open and memory-map read-only (`Mmap`) for the now-sealed segment.

Sealed index mmaps remain mapped for the lifetime of the segment. They are
unmapped (dropped) before the segment files are deleted during eviction.

**Recovery:**

On startup, sealed segments get read-only mmaps. The active (last) segment's
index is extended back to its pre-allocated capacity and mapped read-write,
with `active_idx_write_pos` set to the actual (pre-extension) file size so
new entries continue from where the previous session left off.

**Binary search:**

Both `Mmap` (sealed) and `MmapMut` (active) are searched with the same
`index_lookup_in(&[u8], target_relative_seq)` function — a standard binary
search for the largest `relative_seq <= target`. For the active index, the
slice is bounded to `&mmap[..active_idx_write_pos]` to exclude the
zero-filled pre-allocated tail.

**Comparison with Kafka:**

| Aspect | Kafka | Narwhal |
|--------|-------|---------|
| Index file format | `relative_offset(4) + position(4)` = 8 bytes | `relative_seq(4) + offset(8)` = 12 bytes |
| Active index | Pre-allocated, `MmapMut` | Pre-allocated, `MmapMut` |
| Sealed index | Truncated to actual size, `Mmap` | Truncated to actual size, `Mmap` |
| Index interval | Configurable (`index.interval.bytes`, default 4096) | Hardcoded 4096 |
| Offset width | 4-byte relative (Kafka batches limit segment to 2 GB) | 8-byte absolute (simple, supports arbitrary segment sizes) |
| Warm-up | `mmap` + `madvise(WILLNEED)` | `mmap` (no explicit `madvise`) |

## Trait API

### MessageLog Trait

```rust
pub trait MessageLog: 'static {
    /// Append a message and its payload to the log.
    /// When message count exceeds `max_messages`, oldest segments are evicted.
    async fn append(
        &self,
        message: &Message,
        payload: &PoolBuffer,
        max_messages: u32,
    ) -> anyhow::Result<()>;

    /// Delete the entire log (all segments and index files).
    async fn delete(&self) -> anyhow::Result<()>;

    /// Flush buffered writes to durable storage (fsync).
    async fn flush(&self) -> anyhow::Result<()>;

    /// First retained sequence number, or 0 if the log is empty.
    /// In-memory value — cannot fail.
    fn first_seq(&self) -> u64;

    /// Last written sequence number, or 0 if the log is empty.
    /// In-memory value — cannot fail.
    fn last_seq(&self) -> u64;

    /// Read entries starting at `from_seq`, up to `limit` entries.
    /// Calls `visitor.visit()` for each entry. Returns the number of
    /// entries visited.
    /// Async because file reads use io_uring (compio). The visitor callback
    /// is also async, borrowing data from the EntryReader's buffers.
    async fn read(
        &self,
        from_seq: u64,
        limit: u32,
        visitor: &mut impl LogVisitor,
    ) -> anyhow::Result<u32>;
}
```

**Key design decisions:**
- `first_seq()` and `last_seq()` return `u64` directly (not `Result`) because
  they are in-memory values updated on append/eviction.
- `read` is async (io_uring positioned reads via `EntryReader`) and the visitor
  is also async, allowing it to perform I/O (e.g., sending messages) between
  entry reads.
- No `direction` parameter — the client computes the appropriate `from_seq`
  using `first_seq`/`last_seq` from the `CHAN_SEQ` response. The log always
  reads forward.

### LogVisitor Pattern

```rust
pub struct LogEntry<'a> {
    pub seq: u64,
    pub timestamp: u64,
    pub from: &'a [u8],
    pub payload: &'a [u8],
}

#[async_trait(?Send)]
pub trait LogVisitor {
    async fn visit(&mut self, entry: LogEntry<'_>) -> anyhow::Result<()>;
}
```

The visitor borrows entry data directly from the `EntryReader`'s pre-allocated
buffers — **zero heap allocations** on the read hot path. The `async` signature
allows visitors to perform async work (e.g., sending messages over the network)
without blocking.

The concrete `HistoryVisitor` used by `ChannelShard::history()` holds references
to the transmitter, channel name, `history_id`, and the payload pool:

```
┌────────────────────────────────────────────────────────────┐
│                    HistoryVisitor                          │
│                                                            │
│  For each LogEntry:                                        │
│    1. Construct Message::Message { history_id, ... }       │
│    2. Copy payload into PoolBuffer (no heap alloc)         │
│    3. Call transmitter.send_message_with_payload()         │
└────────────────────────────────────────────────────────────┘
```

### MessageLogFactory

```rust
#[async_trait(?Send)]
pub trait MessageLogFactory: Clone + Send + Sync + 'static {
    type Log: MessageLog;
    async fn create(&self, handler: &StringAtom) -> Self::Log;
}
```

The factory holds `base_dir` and `max_payload_size` at construction time.
`create()` is async because it performs recovery (scanning segment files,
validating CRC checksums, rebuilding indexes) using `compio::fs` I/O. It
derives the channel directory using the shared SHA-256 path utility (same as
`FileChannelStore`).

## Write Path

### Append

```
append(message, payload, max_messages)
│
├─ 1. Serialize entry: seq | timestamp | from_len | payload_len | from | payload | crc32
├─ 2. write_all_at(entry, seg.file_size) via io_uring (compio positioned write)
├─ 3. If 4096+ bytes written since last index entry → write index entry into .idx mmap
│     (checked before updating bytes_since_index so the threshold reflects pre-write state)
├─ 4. Update in-memory state (bytes_since_index, last_seq, segment byte count)
├─ 5. If segment exceeds 128 MiB → roll to new segment (see Segment Roll)
└─ 6. If (last_seq - first_seq + 1) > max_messages → evict oldest segment(s)
```

Writes use **positioned I/O** via `compio::fs::File::write_all_at()` (io_uring
`WRITE` op). There is no append mode in compio — the file offset is tracked
in-memory as `seg.file_size` and passed explicitly to each write. This is safe
under the single-threaded shard actor model. Data is visible to subsequent reads
immediately (even before fsync) because both reads and writes operate on the
same file through the kernel page cache.

### Flush

`flush()` calls async `sync_all().await` on both active files:

- the active segment's `.log` file
- the active segment's `.idx` file (whose dirty pages come from `MmapMut` writes)

Both syncs run through compio/io_uring and avoid synchronous `mmap.flush()`
inside async shard code.

**Durability guarantee:** same as Kafka — data survives process crashes (data is
in the kernel page cache) but not power loss without fsync. The existing flush
mechanism (immediate when `message_flush_interval=0`, or periodic via background
task) is unchanged.

### Segment Roll

Checked **after** each append:

```
After write completes:
│
├─ Active segment > 128 MiB?
│   ├─ No  → done
│   └─ Yes → roll:
│       1. Close active .log file handle
│       2. sync_all() active .idx file
│       3. Drop active .idx MmapMut
│       4. Truncate .idx from pre-allocated size to actual written size
│       5. Re-open .idx as read-only Mmap (now a sealed segment)
│       6. Create new .log file (named after next seq to be written)
│       7. Pre-allocate new .idx to max capacity and MmapMut it
│       8. New segment becomes the active segment
```

## Read Path

### Index Lookup

To read from `from_seq`:

```
1. Binary search the in-memory segment list for the last segment
   whose first_seq <= from_seq

2. Binary search the segment's .idx for the largest
   relative_seq <= (from_seq - segment_base_seq)

3. Start positioned reads at the offset from the index entry

4. Scan forward entry-by-entry until seq >= from_seq

5. Begin visiting entries until limit is reached
   or the segment ends (then continue to next segment)
```

### EntryReader — Zero-Allocation Positioned Reads

Instead of chunk-based buffered reading, the implementation uses an `EntryReader`
struct that performs **two positioned reads per entry** via `compio::fs::File`
(io_uring). Pre-allocated buffers eliminate per-entry heap allocation.

```rust
struct EntryReader {
    header: Vec<u8>,    // always ENTRY_HEADER_SIZE (22 bytes)
    body: Vec<u8>,      // pre-allocated to NID_MAX_LENGTH + max_payload_size + CRC_SIZE
    seq: u64,
    timestamp: u64,
    from_len: usize,
    payload_len: usize,
    entry_size: u64,
}
```

| Property     | Value |
|-------------|-------|
| Header buffer | 22 bytes (fixed) |
| Body buffer | `NID_MAX_LENGTH` (510) + `max_payload_size` + 4 bytes |
| Lifetime    | Created once at construction, reused across all operations (reads, recovery, index rebuilds) |
| Guarantee   | Always fits any valid entry's body (from + payload + CRC) |

`NID_MAX_LENGTH` (510 bytes) is derived from the protocol's maximum NID size:
`USERNAME_MAX_LENGTH` (256) + 1 (`@`) + `DOMAIN_MAX_LENGTH` (253).

**Read loop:**

```
┌──────────────────────────────────────────────────────────────┐
│  For each entry at position `pos`:                           │
│                                                              │
│  1. read_exact_at(header, pos)          ← 22-byte header     │
│     Parse seq, timestamp, from_len, payload_len              │
│                                                              │
│  2. read_exact_at(body[..body_size], pos + 22)               │
│     Uses IoBuf::slice(..body_size) to read exactly           │
│     from_len + payload_len + 4 bytes into the pre-allocated  │
│     body buffer without touching remaining capacity          │
│                                                              │
│  3. Verify CRC32 over header + body                          │
│                                                              │
│  4. If seq >= from_seq:                                      │
│     visitor.visit(LogEntry { seq, from, payload }).await     │
│     (from and payload borrow directly from body buffer)      │
│                                                              │
│  5. Advance pos += entry_size                                │
└──────────────────────────────────────────────────────────────┘
```

Buffers are moved into compio via `std::mem::take()` for each I/O op and
reclaimed from `BufResult` — the standard compio buffer-ownership pattern.

### Visitor Invocation

The visitor's `visit()` method is called between positioned reads. The
`LogEntry` borrows `from` and `payload` directly from the `EntryReader`'s body
buffer. The same `EntryReader` is reused for recovery scanning
(`scan_and_validate`, `rebuild_index`).

## Eviction

Eviction is **count-based**, driven by `max_persist_messages`.

```
After each append:
│
├─ Compute logical first_seq = last_seq - max_persist_messages + 1
├─ For each segment (oldest first):
│   └─ Is segment's last_seq < logical first_seq?
│       ├─ Yes → delete .log + .idx, update in-memory segment list
│       └─ No  → stop (all remaining segments have retained messages)
```

A segment is deleted only when **every** message in it falls outside the
retention window. This means actual disk usage may slightly exceed
`max_persist_messages` worth of data (by up to one segment's worth of extra
messages), but the trade-off is clean O(1) eviction with no rewriting.

## Recovery

On startup, the message log restores its state from disk. Recovery is **fully
async** — all file I/O uses `compio::fs` (io_uring), except for `read_dir`
(no compio equivalent, uses `std::fs`).

An `EntryReader` and an index rebuild buffer (`Vec<u8>`) are created once at the
start of recovery and reused across all segments to avoid per-segment allocation.

```
1. List *.log files in channel directory, sorted by first_seq (from filename)
   (std::fs::read_dir — no compio equivalent)

2. For each sealed segment (all except the last):
   ├─ Scan .log with EntryReader (CRC validation) to determine last_seq
   │   Segments with zero valid entries are deleted.
   ├─ .idx exists? → memory-map read-only (Mmap)
   └─ .idx missing? → rebuild by scanning .log with EntryReader,
   │                   write index via write_all_at, then mmap read-only

3. For the active (last) segment:
   ├─ Scan forward with EntryReader, validating CRC32 per entry
   ├─ Truncate at the first invalid/partial entry (file.set_len().await)
   ├─ Rebuild .idx from valid entries (reusing index buffer)
   ├─ Compute bytes_since_index: read the last index entry to find its offset,
   │   scan forward from there to count bytes written after it, so the index
   │   interval resumes correctly on the next append
   ├─ Open .log for writes (positioned I/O at seg.file_size)
   ├─ Extend .idx to pre-allocated capacity
   └─ Memory-map read-write (MmapMut), set write_pos to actual index size

4. Empty/zero-byte .log files → delete (compio::fs::remove_file().await)

5. Derive in-memory state:
   ├─ last_seq  → from last valid entry of newest segment
   ├─ first_seq → from oldest segment's filename
   └─ Per-segment metadata (first_seq, last_seq, file_size)
```

**Guarantees:**
- A crash mid-append loses at most the in-progress entry (CRC detects the partial write).
- A crash during segment roll leaves at most an empty segment file, which is cleaned up.
- Index corruption is always recoverable by scanning the log.

## Integration

### HISTORY Flow

```
Client                        Server (ChannelShard)
  │                               │
  │  HISTORY id=1 channel=!ch     │
  │  history_id=h1 from_seq=50    │
  │  limit=10                     │
  │──────────────────────────────►│
  │                               ├─ Validate: local domain, channel exists,
  │                               │   membership, read ACL, persistence enabled
  │                               │
  │                               ├─ Clamp limit to max_history_limit
  │                               │
  │                               ├─ message_log.read(from_seq, limit, &mut visitor)
  │                               │     │
  │                               │     ├─ For each entry:
  │  MESSAGE from=... channel=... │     │   visitor constructs MESSAGE with history_id=h1,
  │  history_id=h1 seq=50 ...     │◄────│   copies payload to PoolBuffer,
  │  <payload>                    │     │   calls transmitter.send_message_with_payload()
  │                               │     │
  │  MESSAGE ... seq=51 ...       │◄────│
  │  ...                          │     │
  │                               │     └─ Returns count
  │                               │
  │  HISTORY_ACK id=1 channel=!ch │
  │  history_id=h1 count=10       │◄──── Send HISTORY_ACK with count
  │                               │
```

### CHAN_SEQ Flow

```
Client                        Server (ChannelShard)
  │                               │
  │  CHAN_SEQ id=1 channel=!ch    │
  │──────────────────────────────►│
  │                               ├─ Validate: local domain, channel exists,
  │                               │   membership, read ACL, persistence enabled
  │                               │
  │  CHAN_SEQ_ACK id=1            │
  │  channel=!ch                  │◄──── first_seq = message_log.first_seq()
  │  first_seq=1 last_seq=500     │      last_seq  = message_log.last_seq()
  │                               │
```

## Constants

| Constant                | Value     | Description |
|------------------------|-----------|-------------|
| `SEGMENT_MAX_BYTES`    | 128 MiB   | Maximum segment file size before rolling |
| `INDEX_INTERVAL_BYTES` | 4096      | Bytes of log data between sparse index entries |
| `INDEX_ENTRY_SIZE`     | 12 bytes  | Size of one index entry (`relative_seq` + `offset`) |
| `ENTRY_HEADER_SIZE`    | 22 bytes  | Fixed header: seq(8) + timestamp(8) + from_len(2) + payload_len(4) |
| `CRC_SIZE`             | 4 bytes   | CRC32 checksum appended to each entry |
| `NID_MAX_LENGTH`       | 510 bytes | Maximum NID size: USERNAME_MAX_LENGTH(256) + 1 + DOMAIN_MAX_LENGTH(253) |

All constants are hardcoded. They can be promoted to per-channel configuration
if a real need arises.

## Dependencies

| Crate       | Version | Purpose |
|-------------|---------|---------|
| `compio`    | 0.18    | Async file I/O via io_uring (`compio::fs::File` for positioned reads/writes) |
| `memmap2`   | 0.9     | Memory-mapped index files (`Mmap` for sealed, `MmapMut` for active) |
| `crc32fast` | 1       | CRC32 checksums for log entry integrity |

## Concurrency Model

The message log operates in a **single-threaded** context. Each channel is
owned by a shard actor, and the shard processes commands sequentially. There is
no concurrent read/write access to a channel's message log.

- No locks, no atomics.
- `Rc<MessageLog>` (not `Arc`) — consistent with the existing `Channel` struct.
- Interior mutability via `RefCell` — provides runtime borrow checking that
  catches violations in debug builds. The `MessageLog` trait exposes `&self`
  methods; `FileMessageLog` uses `RefCell<Inner>` to mutate internal state.
- `read()` and `append()` are never called concurrently for the same channel.

## Testing

Integration tests live in `crates/server/tests/c2s_channel_persistence.rs`.

**Test categories:**

| Category | What it verifies |
|----------|-----------------|
| Append + read | Single entry, multiple entries, round-trip correctness |
| Sparse index | Binary search finds the correct offset |
| Segment roll | Cross-segment reads return continuous data |
| Eviction | Segment deletion when all messages fall outside retention |
| Seq tracking | `first_seq` / `last_seq` correctness through append and eviction |
| CRC validation | Corrupt entries detected, partial writes rejected |
| Recovery | Truncation at corrupt tail, index rebuild from log |
| Edge cases | Empty log, `from_seq` beyond `last_seq`, single-entry segments |

Tests use `tempfile::TempDir` for isolated file system state.

Existing tests using `NoopMessageLog` remain unchanged — they verify channel
manager logic independently of persistence.

## File Locations

| File | Purpose |
|------|---------|
| `crates/server/src/channel/file_message_log.rs` | `FileMessageLog` implementation |
| `crates/server/src/channel/store.rs` | `MessageLog` and `MessageLogFactory` trait definitions |
| `crates/server/src/channel/file_store.rs` | `FileChannelStore` (shares path utility) |
| `crates/server/src/channel/manager.rs` | `ChannelShard::history()` and `channel_seq()` integration |
| `crates/server/tests/c2s_channel_persistence.rs` | Integration tests |
