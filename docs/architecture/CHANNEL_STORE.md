# Channel Store — Architecture Specification

> **Status:** Implemented.
> **Last updated:** 2026-04-17
> **Related PRs:** see `crates/server/src/channel/file_store.rs` history.

## Table of Contents

- [Overview](#overview)
- [Goals and Non-Goals](#goals-and-non-goals)
- [On-Disk Layout](#on-disk-layout)
  - [Directory Structure](#directory-structure)
  - [Storage Hash](#storage-hash)
  - [Metadata File Format](#metadata-file-format)
- [Trait API](#trait-api)
  - [ChannelStore Trait](#channelstore-trait)
  - [PersistedChannel](#persistedchannel)
- [Write Path](#write-path)
  - [Atomic Write Protocol](#atomic-write-protocol)
  - [When Saves Happen](#when-saves-happen)
- [Delete Path](#delete-path)
- [Restore Path](#restore-path)
  - [Hash Enumeration](#hash-enumeration)
  - [Shard Assignment](#shard-assignment)
  - [Per-Shard Restore](#per-shard-restore)
- [Relationship with the Message Log](#relationship-with-the-message-log)
- [Concurrency Model](#concurrency-model)
- [Constants](#constants)
- [Dependencies](#dependencies)
- [Testing](#testing)
- [File Locations](#file-locations)

---

## Overview

The channel store is the persistence layer for **channel metadata** — the
handler, owner, configuration, ACLs, and sorted member list of each persistent
channel. It is separate from, but colocated with, the
[message log](MESSAGE_LOG.md) that persists broadcast messages.

Each persistent channel lives in its own directory, identified by a SHA-256
hash of the channel handler. The directory holds a single `metadata.bin` file
alongside the channel's message log segments.

```
┌────────────────────────────────────────────────────────────┐
│                      ChannelShard                          │
│                                                            │
│   ┌─────────────┐       ┌──────────────────┐               │
│   │  Channel    │──────►│  ChannelStore    │── metadata.bin│
│   │ (in-memory) │       │  (FileChannelStore)              │
│   └──────┬──────┘       └──────────────────┘               │
│          │                                                 │
│          │               ┌──────────────────┐              │
│          └──────────────►│  MessageLog      │── *.log/*.idx│
│                          │  (FileMessageLog)│              │
│                          └──────────────────┘              │
└────────────────────────────────────────────────────────────┘
                        │
                        ▼
       <data_dir>/<sha256(handler)>/
           metadata.bin
           00000000000000000001.log
           00000000000000000001.idx
           ...
```

On startup, the channel store is scanned once to discover every persisted
channel; each channel is then re-hydrated into the shard that owns it (by
hashing the handler modulo the shard count).

## Goals and Non-Goals

**Goals:**
- Durable, crash-safe persistence of channel metadata (handler, owner, config,
  ACLs, member list).
- Survive process restart: rebuild the full membership graph and resume
  broadcasts at the correct sequence number.
- Atomic updates — a concurrent crash must leave either the old or the new
  version intact, never a half-written file.
- Share a single on-disk directory with the message log so that deleting a
  channel is a single-directory operation.

**Non-Goals:**
- Cross-channel queries (there is no global index beyond directory enumeration).
- Incremental metadata updates — each save rewrites the whole file.
- Version migration tooling (the current format has no version byte; see
  [Constants](#constants)).
- Replication or distributed consensus.

## On-Disk Layout

### Directory Structure

All persistent channel state is rooted at the configured `data_dir`
(TOML: `[c2s.storage] data_dir`, default `./data`).

```
<data_dir>/
  <sha256_hex(handler_A)>/
    metadata.bin              ← channel A's metadata (postcard-encoded PersistedChannel)
    metadata.bin.tmp          ← transient, only visible during an in-progress save
    00000000000000000001.log  ← message log segment(s), managed by FileMessageLog
    00000000000000000001.idx
    ...
  <sha256_hex(handler_B)>/
    metadata.bin
    ...
```

Each channel directory is entirely self-contained: `FileChannelStore` owns
`metadata.bin`, and [`FileMessageLog`](MESSAGE_LOG.md) owns the `*.log` /
`*.idx` files. They never touch each other's files.

### Storage Hash

The directory name is a hex-encoded SHA-256 of the handler bytes:

```rust
fn channel_hash(handler: &StringAtom) -> StringAtom {
    // lowercase hex of Sha256::digest(handler.as_bytes())
}
```

| Property | Value |
|----------|-------|
| Algorithm | SHA-256 |
| Encoding  | Lowercase hex, 64 characters |
| Inputs    | `handler.as_bytes()` — no salt, no normalization |
| Determinism | Same handler ⇒ same hash forever |

Using a hash (instead of the raw handler) gives a fixed-length, filesystem-safe
directory name regardless of what characters the handler contains. The hash is
a pure function of the handler, so `FileChannelStore` and `FileMessageLog`
independently derive the same directory without any shared state.

The server records the hash on the in-memory `Channel` (as `store_hash`) after
the first successful save, so later deletes can skip re-hashing.

### Metadata File Format

`metadata.bin` is a raw [postcard](https://github.com/jamesmunns/postcard)
encoding of the `PersistedChannel` struct:

```rust
#[derive(Serialize, Deserialize)]
pub struct PersistedChannel {
    pub handler: StringAtom,
    pub owner: Option<Nid>,
    pub config: ChannelConfig,     // max_clients, max_payload_size,
                                   // max_persist_messages, persist,
                                   // message_flush_interval
    pub acl: ChannelAcl,           // join_acl, publish_acl, read_acl
    pub members: Rc<[Nid]>,        // sorted by Nid
}
```

| Property | Value |
|----------|-------|
| Encoding | postcard (varint-length-prefixed, little-endian) |
| Header | None — the format has no magic bytes or version byte |
| Integrity | No CRC — file is written atomically (see below) |
| Max size | 64 MiB — enforced on load to refuse obviously corrupt files |

**Why no CRC:** the atomic write protocol (write-tmp → fsync → rename → fsync
parent dir) guarantees that readers always see either the previous successful
write or the new one, never a partial file. A checksum would only add value if
writes were in-place.

**Why no version byte:** the only deployed format is the current one. If a
schema change becomes necessary, a version byte can be added by bumping the
postcard struct (non-breaking field additions are already tolerated by
postcard when they're at the end and `Option`-typed).

## Trait API

### ChannelStore Trait

```rust
#[async_trait(?Send)]
pub trait ChannelStore: Clone + Send + Sync + 'static {
    /// Persist channel metadata. Called on creation and every metadata update.
    /// Returns the storage hash that identifies this channel on disk.
    async fn save_channel(&self, channel: &PersistedChannel)
        -> anyhow::Result<StringAtom>;

    /// Remove all persisted metadata for the given storage hash.
    async fn delete_channel(&self, hash: &StringAtom) -> anyhow::Result<()>;

    /// Enumerate the storage hashes of all persisted channels.
    async fn load_channel_hashes(&self) -> anyhow::Result<Arc<[StringAtom]>>;

    /// Load the persisted metadata of a single channel by its storage hash.
    async fn load_channel(&self, hash: &StringAtom)
        -> anyhow::Result<PersistedChannel>;
}
```

**Key design decisions:**
- `save_channel` accepts a *projected* `PersistedChannel` — callers build the
  post-change snapshot, and the store rewrites the file whole. No partial
  updates.
- `save_channel` returns the storage hash so the caller can cache it on the
  in-memory `Channel` without re-hashing on every mutation.
- `delete_channel` is **idempotent**: missing files/directories are treated as
  success. This keeps teardown paths (including best-effort cleanup) free of
  special-case branching.
- `load_channel_hashes` returns `Arc<[StringAtom]>` so it can be cheaply shared
  across shards during bootstrap.
- Lifecycle is decoupled from the message log: enabling `persist` saves
  metadata; disabling it deletes metadata *and* the log directory.

### PersistedChannel

The persisted struct mirrors the in-memory `Channel` but only the fields that
must survive a restart — ephemeral state (subscribers, broadcast sequence
counter, flush tasks) is reconstructed on restore.

| Field | Reconstructed on restore? |
|-------|---------------------------|
| `handler`, `owner`, `config`, `acl`, `members` | Persisted, loaded verbatim |
| `seq` (broadcast counter) | From `message_log.last_seq() + 1` |
| `subscribers` (active connections) | Empty until clients reconnect & `JOIN` |
| `flush_cancel_tx` | Spawned on first append when `message_flush_interval > 0` |
| `store_hash` | Set to the directory name (known at restore time) |

The `members` field is stored sorted by `Nid`, which the write path maintains
via `partition_point`-based inserts and `binary_search`-based removes. This
keeps the file byte-stable for unchanged membership, makes membership lookups
`O(log n)`, and makes restore deterministic.

## Write Path

### Atomic Write Protocol

Every save uses the classic **write-tmp → fsync → rename → fsync parent**
sequence:

```
save_channel(projected)
│
├─ 1. Compute hash       = sha256_hex(projected.handler)
├─ 2. Ensure directory   = <data_dir>/<hash>/           (compio::fs::create_dir_all)
├─ 3. Serialize          = postcard::to_allocvec(&projected)
├─ 4. Write tmp file     = <dir>/metadata.bin.tmp        (compio::fs::write)
├─ 5. fsync tmp file     = open → sync_all → close
├─ 6. Rename             = metadata.bin.tmp → metadata.bin (compio::fs::rename)
├─ 7. fsync parent dir   = open <dir> → sync_all → close
└─ 8. Return hash
```

Steps 5 and 7 are what make the write crash-safe:

| Step | If the process crashes immediately after… | Result after restart |
|------|-------------------------------------------|----------------------|
| 4 | tmp file written but not fsynced | `metadata.bin.tmp` may be zero-length or partial; ignored (only `metadata.bin` is loaded) |
| 5 | tmp contents are durable | Ditto — `metadata.bin.tmp` ignored |
| 6 | rename durable in directory cache but parent not fsynced | On most filesystems the rename may not have reached disk; the *previous* `metadata.bin` remains visible |
| 7 | parent dir fsynced | New `metadata.bin` is guaranteed visible post-crash |

A stale `metadata.bin.tmp` left by a pre-step-6 crash is a benign leftover;
subsequent saves overwrite it, and `delete_channel` removes it explicitly.

### When Saves Happen

`ChannelManager` (specifically `ChannelShard`) calls `save_channel` on every
change that mutates the persisted view, **before** applying the change
in-memory. This "persist-then-apply" ordering means a failed save leaves the
in-memory state untouched and returns the error to the client.

| Trigger | Call site | Projected change |
|---------|-----------|------------------|
| `JOIN` on a persistent channel | `join_channel` | Add member to sorted list; set owner if none |
| Owner leaves / `LEAVE` on persistent channel | `do_leave` | Remove member; promote new owner if applicable |
| `SET_CHANNEL_ACL` on persistent channel | `set_channel_acl` | Replace one of join/publish/read ACLs |
| `SET_CHANNEL_CONFIGURATION` with `persist=true` | `set_channel_configuration` | Replace config |

Each save is instrumented by `ChannelManagerMetrics`:
`store_saves{result}` and `store_save_duration_seconds` (histogram).

**Not persisted:** ephemeral ops like broadcast messages (those go to the
message log) and subscriber/connection events.

## Delete Path

`delete_channel(hash)` removes every file `FileChannelStore` owns, then the
directory itself:

```
delete_channel(hash)
│
├─ 1. remove_file(<dir>/metadata.bin)       ← NotFound = success
├─ 2. remove_file(<dir>/metadata.bin.tmp)   ← NotFound = success
└─ 3. remove_dir(<dir>)                     ← NotFound = success
```

Step 3 succeeds only if the directory is empty. If the channel still has
message log segments, `remove_dir` fails — which is the intended behavior:
the caller (`ChannelShard::delete_persistent_storage`) always calls
`message_log.delete()` first, so log files are gone by the time
`delete_channel` runs. If the log delete fails, the metadata delete will
cleanly remove `metadata.bin` but leave the directory with the orphaned log
files; startup then treats that directory as an abandoned log and skips it
(see [Hash Enumeration](#hash-enumeration)).

Delete is **best-effort**: `ChannelShard` logs failures but does not block
in-memory teardown on them. Once a persistent channel is dropped from memory,
stray on-disk state is cleaned up on the next successful delete attempt or on
the next restart's stale-directory sweep.

## Restore Path

### Hash Enumeration

On startup, `load_channel_hashes` lists the data directory:

```
load_channel_hashes()
│
├─ read_dir(<data_dir>)                    ← NotFound → return []
│   (uses std::fs — no compio read_dir)
│
├─ For each directory entry:
│   ├─ If <entry>/metadata.bin exists:
│   │   └─ Push entry name (the hash) into the result
│   │
│   └─ Else:
│       ├─ Warn: "removing stale channel directory (missing metadata.bin)"
│       ├─ Delete every file inside
│       └─ remove_dir(<entry>)
│
└─ Return Arc<[StringAtom]>
```

A directory **without** a `metadata.bin` is by definition not a live channel —
either a crashed-at-creation leftover or a residue of a failed delete. The
sweep keeps the data directory tidy so repeated crashes don't accumulate
garbage.

### Shard Assignment

`ChannelManager::bootstrap` sorts the loaded channels into their owning shards
before spawning shard actors:

```
bootstrap(core_dispatcher, restore_channels):
│
├─ hashes = if restore_channels { store.load_channel_hashes() } else { [] }
│
├─ For each hash:
│   ├─ persisted = store.load_channel(hash)
│   ├─ shard_id  = shard_for(&persisted.handler, shard_count)   // SipHash % N
│   └─ shard_hashes[shard_id].push(hash)
│
└─ For each shard:
    └─ spawn ChannelShard with its assigned hashes
```

This pre-load uses two round trips per channel (`load_channel_hashes` then
`load_channel`) but runs only once, on the coordinating thread, before any
shard starts. The hashes (not the full metadata) are what get dispatched to
shards — each shard re-loads its assigned channels on its own thread during
`restore_and_run`. This keeps `PersistedChannel`'s `Rc<[Nid]>` thread-local.

`restore_channels` is `false` when auth is disabled — without authentication,
memberships are session-scoped and have no meaning across restarts, so
restoring them would produce orphaned channels.

### Per-Shard Restore

Each shard restores its channels sequentially:

```
restore(hashes):
│
For each hash:
  ├─ persisted = store.load_channel(hash)                    ← skip on error
  ├─ message_log = message_log_factory.create(&handler)      ← scans *.log/*.idx
  ├─ channel = Channel::new(handler, config, notifier, message_log)
  ├─ channel.{owner, acl, members} = persisted.{owner, acl, members}
  ├─ channel.store_hash = Some(hash)
  ├─ channel.seq = message_log.last_seq() + 1
  │   // Restart broadcast numbering after the last persisted seq.
  ├─ For each member in channel.members:
  │   └─ membership.reserve_slot(username, handler, u32::MAX)
  │     // u32::MAX bypasses max_channels_per_client — persisted membership
  │     // is authoritative; a post-hoc limit reduction must not evict existing
  │     // members from their channels.
  ├─ channels.insert(handler, channel)
  └─ metrics.channels_active.inc()
```

**Guarantees:**
- A channel whose `metadata.bin` fails to load is **skipped** (with a warning),
  not fatal — one corrupt channel cannot prevent the server from starting.
- Broadcast sequence numbers are continuous across restarts: every new
  broadcast has `seq > last_seq` of whatever survived in the message log.
- Per-client channel limits are only enforced on *new* joins, never on
  restored state.

## Relationship with the Message Log

The channel store and the message log are independent persistence layers that
share a directory by convention:

| Aspect | `FileChannelStore` | [`FileMessageLog`](MESSAGE_LOG.md) |
|--------|--------------------|------------------------------------|
| Owns | `metadata.bin`, `metadata.bin.tmp` | `*.log`, `*.idx` |
| Write pattern | Whole-file atomic rewrite | Append-only positioned writes |
| Integrity | Atomic rename (no CRC) | CRC32 per entry |
| Update frequency | Rare (join/leave/config/ACL) | Every broadcast |
| Directory derivation | `sha256_hex(handler)` | `sha256_hex(handler)` (same function) |

The shared hashing function is the only coupling between the two — everything
else (file naming, I/O model, recovery) is independent. `FileMessageLog`
never reads `metadata.bin`, and `FileChannelStore` never reads log segments.

Deletion ordering is enforced by the caller (`ChannelShard`): log files are
deleted first (`message_log.delete()`), then metadata
(`store.delete_channel(hash)`). This guarantees that a mid-delete crash leaves
at worst an empty directory, which the stale-directory sweep cleans up on the
next start.

## Concurrency Model

- The channel store is invoked **only** from shard actor code, which runs
  single-threaded on the owning shard's thread.
- `FileChannelStore` is `Clone + Send + Sync`: cloned into each shard at
  bootstrap, each shard uses its own copy.
- Internal state is just `Arc<PathBuf>` — no mutability, no locks, no atomics.
- Two shards can save different channels concurrently (different directories),
  but never the **same** channel, because each channel is pinned to exactly
  one shard.
- Within a shard, `save_channel` calls are naturally serialized by the
  single-threaded actor loop; there is no in-flight-save coalescing — each
  mutation triggers one save.

## Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `METADATA_FILE` | `metadata.bin` | Name of the persisted metadata file |
| `METADATA_TMP_FILE` | `metadata.bin.tmp` | Temp file used during atomic writes |
| `MAX_METADATA_SIZE` | 64 MiB | Refuses to load oversized metadata (corruption guard) |
| Hash algorithm | SHA-256 | Directory-name hash |
| Hash encoding | Lowercase hex | 64-character directory name |

The 64 MiB limit is a defensive ceiling; real `metadata.bin` files are a few
hundred bytes to low kilobytes, dominated by the member list.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `compio` | 0.18 | Async file I/O via io_uring (`write`, `rename`, `sync_all`) |
| `postcard` | (workspace) | Compact self-describing serialization of `PersistedChannel` |
| `sha2` | (workspace) | SHA-256 hashing of handler → directory name |
| `serde` | (workspace) | `Serialize` / `Deserialize` derives on `PersistedChannel` and nested types |

`std::fs::read_dir` is used for hash enumeration because compio does not
provide an async `read_dir`. The remaining operations are all async.

## Testing

Unit tests live in `crates/server/src/channel/file_store.rs` and cover:

| Case | What it verifies |
|------|-----------------|
| `save_and_load_round_trip` | Every field of `PersistedChannel` survives a round trip |
| `load_channel_hashes` | Directory enumeration returns every channel's hash |
| `delete_channel` | Metadata, tmp file, and directory all removed |
| `delete_nonexistent_channel` | Delete is idempotent (NotFound = success) |
| `save_overwrites_existing` | Re-saving the same handler updates the file in place |
| `load_corrupted_file_returns_error` | Malformed `metadata.bin` surfaces an error |
| `channel_hash_deterministic` | Same handler → same hash; different handler → different hash |

Integration tests in `crates/server/tests/c2s_channel_persistence.rs` and
`crates/server/tests/c2s_channel_persistence_failure.rs` verify end-to-end
behavior: restart survivability, restore with reconnecting clients, and
persist-toggle teardown.

All tests use `tempfile::TempDir` for isolated filesystem state.

## File Locations

| File | Purpose |
|------|---------|
| `crates/server/src/channel/file_store.rs` | `FileChannelStore` implementation + `channel_hash` |
| `crates/server/src/channel/store.rs` | `ChannelStore` trait + `PersistedChannel` |
| `crates/server/src/channel/manager.rs` | `ChannelShard` call sites for save/delete/restore |
| `crates/server/src/c2s/config.rs` | `StorageConfig` (`data_dir` setting) |
| `crates/server/src/lib.rs` | Wires `FileChannelStore` + `FileMessageLogFactory` into `ChannelManager` |
| `crates/server/tests/c2s_channel_persistence.rs` | End-to-end persistence tests |
| `crates/server/tests/c2s_channel_persistence_failure.rs` | Failure-mode persistence tests |
