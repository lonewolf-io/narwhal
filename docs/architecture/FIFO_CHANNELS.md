# FIFO Channels — Architecture Specification

> **Status:** Proposed. Not yet implemented.
> **Related design:** [Message Log](MESSAGE_LOG.md), [Channel Store](CHANNEL_STORE.md)

## Table of Contents

- [Overview](#overview)
- [Goals and Non-Goals](#goals-and-non-goals)
- [Semantics](#semantics)
  - [Delivery Model](#delivery-model)
  - [Ordering and Concurrency](#ordering-and-concurrency)
  - [Loss Model](#loss-model)
- [Lifecycle](#lifecycle)
  - [Becoming a FIFO Channel](#becoming-a-fifo-channel)
  - [Living as a FIFO Channel](#living-as-a-fifo-channel)
  - [Destroying a FIFO Channel](#destroying-a-fifo-channel)
- [Protocol Additions](#protocol-additions)
  - [Configuration messages — `type` field](#configuration-messages--type-field)
  - [`PUSH` / `PUSH_ACK`](#push--push_ack)
  - [`POP` / `POP_ACK`](#pop--pop_ack)
  - [`GET_CHAN_LEN` / `CHAN_LEN`](#get_chan_len--chan_len)
  - [Event: `CHANNEL_RECONFIGURED`](#event-channel_reconfigured)
  - [New Error Reasons](#new-error-reasons)
- [Authorization Matrix](#authorization-matrix)
- [Storage](#storage)
  - [Reusing the Message Log](#reusing-the-message-log)
  - [Head Cursor](#head-cursor)
  - [Cursor Durability](#cursor-durability)
  - [Segment Eviction](#segment-eviction)
  - [Channel Metadata](#channel-metadata)
- [Behavior Overrides vs. Pub/Sub](#behavior-overrides-vs-pubsub)
- [Operational Notes](#operational-notes)
- [Out of Scope (v1)](#out-of-scope-v1)
- [Implementation Checklist](#implementation-checklist)

---

## Overview

A **FIFO channel** is a work-queue style channel where:

- The channel **owner** is the only client that can publish elements (via `PUSH`).
- Any **JOINed, read-authorized member** can consume elements (via `POP`).
  The existing read ACL applies — the same gate used for `MESSAGE` delivery
  on pub/sub channels.
- Each element is **returned by at most one successful `POP`**
  (competing-consumers model). Under the at-most-once loss model an element
  may be returned by zero successful `POP`s if the server crashes between
  cursor fsync and `POP_ACK` socket write — see
  [Cursor Durability](#cursor-durability).
- Consumption is **destructive**: once `POP` returns an element, it is gone from the queue.

FIFO channels coexist with the existing pub/sub channel type in a single namespace.
A channel is one or the other for the lifetime of the channel; a pub/sub channel may
be transitioned to FIFO once, but the reverse is not allowed.

## Goals and Non-Goals

**Goals**

- Reuse the existing channel manager, ACL system, message log, and channel store.
- Single-element atomic `POP` (fetch + remove in one round trip, no ack protocol).
- Durable head cursor: at-most-once delivery survives server restart.
- Hard backpressure: producer is told `QUEUE_FULL` rather than blocked indefinitely.

**Non-Goals (v1)**

- Wake-on-push consumers (long-polling). Empty `POP` returns immediately with an error.
- At-least-once / exactly-once semantics. We accept message loss when a consumer
  crashes between TCP write and processing.
- Multi-owner / transferable ownership.
- Reverting a FIFO channel back to pub/sub.
- Bulk `PUSH` / `POP` (single-element only).
- FIFO across non-local domains. `channel_id.domain != local_domain` is
  rejected with `NOT_IMPLEMENTED`, matching the existing data-plane handlers
  (`JOIN`, `BROADCAST`, `HISTORY`, `CHAN_SEQ`, `DELETE`). The configuration /
  ACL handlers historically use `NOT_ALLOWED` for the same condition; FIFO
  follows the data-plane convention since `PUSH`/`POP`/`GET_CHAN_LEN` are
  data-plane operations.

## Semantics

### Delivery Model

FIFO channels implement **competing consumers**: when N JOINed clients are issuing
`POP`s concurrently against the same FIFO channel, each element is returned by
**at most one** successful `POP` (zero if the server crashes between cursor fsync
and `POP_ACK` socket write — see the [loss model](#loss-model)). Two clients
calling `POP` simultaneously will receive different elements (or one of them will
receive `QUEUE_EMPTY`); they will never both receive the same element.

The shard actor that owns the channel serializes all commands on its mailbox, so
`POP` requests are processed in arrival order against a single advancing head
cursor. There is no fairness scheduler beyond mailbox arrival order
(first-come-first-served).

### Ordering and Concurrency

`PUSH` and `POP` are both processed on the channel's shard actor (existing
sharding by channel handler — see [ACTOR_SHARDING.md](ACTOR_SHARDING.md)).
This serialization gives:

- `PUSH`es from the owner are appended to the log in arrival order.
- `POP`s read from the head cursor in arrival order.
- `seq` is assigned monotonically by the channel's sequencing logic
  (`Channel::next_seq()` in `crates/server/src/channel/manager.rs`) before
  the entry is appended; the message log persists whatever `seq` it is
  given. This matches the pub/sub `BROADCAST` path. **`seq` is not exposed
  on the wire** for FIFO commands — it is an internal invariant only.

### Loss Model

**At-most-once.** `POP` is "fetch and remove atomically" with no client
acknowledgement. If the server flushes the cursor advance and writes the
`POP_ACK` to the socket, the element is considered delivered. If the consumer
process crashes after the kernel has accepted the bytes but before the
application processes them, the element is lost. This is an explicit tradeoff
to keep the protocol stateless on the consumer side.

A consumer that wants stronger semantics must build its own application-level
ack/redelivery on top, or wait for a future at-least-once mode.

## Lifecycle

### Becoming a FIFO Channel

There is **no** `CREATE` command. A FIFO channel is born from an ordinary
pub/sub channel via reconfiguration:

1. A client `JOIN`s a non-existent channel name. The channel is auto-created as
   pub/sub. The first joiner becomes the owner (existing behavior — see
   `Channel.owner`, `crates/server/src/channel/manager.rs`).
2. The owner sends `SET_CHAN_CONFIG id=<corr> channel=<name> type=fifo
   persist=true max_persist_messages=<N>` to convert the channel.
3. The conversion is **one-way**. Subsequent `SET_CHAN_CONFIG` calls cannot set
   `type=pubsub` on a FIFO channel; the server returns `BAD_REQUEST`.

**Required config when transitioning to FIFO.** The transition is rejected
unless the merged-config result satisfies all of:

- `type = fifo`
- `persist = true` (FIFO channels are inherently persistent — the head cursor
  and segment files must survive restart).
- `max_persist_messages > 0` (the queue depth cap; without it, `QUEUE_FULL`
  cannot be enforced).

Existing pub/sub state at transition time:

- **Members stay JOINed.** No re-JOIN required. Existing members become
  consumers eligible to `POP`.
- **Buffered messages are purged logically (no physical truncation).** Any
  messages held in the message log at transition time become unreachable to
  consumers — they belong to the pub/sub semantics and have already been
  delivered (or evicted) under those rules. Head-eviction reclaims the
  now-unreachable segments lazily as the cursor stays past them, modulo
  the [tail-segment retention invariant](#segment-eviction) which keeps
  `log.last_seq()` recoverable.

- **Crash-safe transition ordering.** The transition durably touches two
  artifacts: `cursor.bin` (new — initial head) and `metadata.bin`
  (updated — `channel_type=fifo`, possibly `persist`/`max_persist_messages`).
  Order: **flush log → write cursor → write metadata**.

  1. **Establish the durable log tail.** Synchronously flush the message
     log and fsync, so `log.last_seq()` reflects on-disk state and is not
     just an in-memory value. (`MessageLog::last_seq()` can otherwise
     include unflushed appends — initializing the cursor from a
     non-durable tail risks `cursor.next_seq > log.last_seq() + 1` after
     a crash, which the recovery model treats as corruption.)
  2. Atomic write of `cursor.bin` with `next_seq = log.last_seq() + 1`
     (now durable), fsynced (tmp → fsync → rename → fsync parent).
  3. Atomic write of `metadata.bin` with `channel_type=fifo` (and any
     other config changes from the same `SET_CHAN_CONFIG`), fsynced.
  4. Send `SET_CHAN_CONFIG_ACK`.

  Crash semantics:

  - **Crash before (2)** (during or after the log flush, before the
    cursor write): no FIFO state has been committed. On restart the
    channel restores from whatever `metadata.bin` exists (or doesn't —
    see below). The transition simply did not happen; the operator
    retries `SET_CHAN_CONFIG`.
  - **Crash between (2) and (3)** has two sub-cases depending on whether
    the channel had pre-existing persistent state:
    - **Was already persistent (`persist=true` before this
      `SET_CHAN_CONFIG`).** `metadata.bin` exists on disk and still
      declares the channel as pub/sub. On restart the channel restores
      as pub/sub; the stranded `cursor.bin` is ignored (pub/sub never
      reads it). The operator retries `SET_CHAN_CONFIG`; the stranded
      `cursor.bin` is overwritten on retry or removed on `DELETE`.
    - **Was transient (`persist=false`, the auto-create default).**
      No `metadata.bin` exists on disk — the channel store only saves
      metadata when the merged config is persistent (see
      `set_channel_configuration` in
      `crates/server/src/channel/manager.rs`, `save_channel` is gated
      on `is_persistent`). On restart there is no persisted channel;
      the channel directory contains an orphaned `cursor.bin` and
      possibly some `*.log` segments from any persistent moment that
      never happened. The original transient channel state is gone (it
      was in-memory only), so a re-`JOIN` creates a fresh pub/sub
      channel and the operator retries the `SET_CHAN_CONFIG`. The
      orphaned `cursor.bin` should be cleaned up — either by the
      channel directory's lazy-init path on the next save, or via an
      explicit startup sweep (operator may also remove the channel
      directory manually).
  - **Crash between (3) and (4):** on restart the channel comes up as
    FIFO with a valid cursor anchored at the durable log tail. The
    operator never received `SET_CHAN_CONFIG_ACK` and may retry; the
    second `SET_CHAN_CONFIG type=fifo` is a no-op (already FIFO).

  The reverse order (metadata first, cursor second) is **wrong**: a
  crash between the metadata write and the cursor write would restore
  a FIFO channel with no cursor sidecar, which fails recovery with
  `CURSOR_RECOVERY_REQUIRED` and requires operator intervention to fix
  what was a clean retry case.

- **Seq-counter alignment.** As part of the transition, the channel's
  in-memory seq counter (`channel.seq`, the next-to-assign seq used by
  `BROADCAST` and now by `PUSH`) is set to `log.last_seq() + 1`. In a
  persistent-throughout pub/sub channel this is a no-op:
  `channel.seq == log.last_seq() + 1` is the steady-state invariant when
  every `BROADCAST` writes to the log. The alignment is meaningful only
  when the channel was **non-persistent at some point** during its
  pub/sub life: `BROADCAST` advances `channel.seq` regardless of
  `persist`, but only writes to the log when `persist == true` (see
  `crates/server/src/channel/manager.rs` `broadcast_payload`). Transient
  non-persistent broadcasts therefore leave `channel.seq` ahead of
  `log.last_seq() + 1`. Without the alignment, those phantom seq numbers
  would survive into FIFO: the first FIFO `PUSH` would be assigned a seq
  far ahead of the cursor, breaking the recovery invariant
  `cursor.next_seq <= log.last_seq() + 1` and producing a hole `POP`
  could never read.

  Pre-transition seq numbers are ephemeral; the type change invalidates
  any in-flight pub/sub seq references anyway (`BROADCAST`, `HISTORY`,
  and `CHAN_SEQ` all return `WRONG_TYPE` once the channel is FIFO), so
  rewinding `channel.seq` is a safe operation.
- **ACLs are preserved.** The existing publish/read ACLs still apply: read ACL
  gates `POP`, but publish ACL is effectively superseded by the owner-only PUSH
  rule (see [Authorization Matrix](#authorization-matrix)).

### Living as a FIFO Channel

While a channel is FIFO:

- `BROADCAST` is rejected (`WRONG_TYPE`).
- `HISTORY` and `CHAN_SEQ` are rejected (`WRONG_TYPE`) — destructive consumption
  has no archive contract.
- `PUSH` and `POP` are valid.
- `MEMBER_JOINED` / `MEMBER_LEFT` events still fire on JOIN/LEAVE.
- `MESSAGE` is **never** pushed — `PUSH` does not fan out to subscribers.
  Consumers must explicitly `POP`.
- The owner cannot `LEAVE` (returns `OWNER_CANNOT_LEAVE`). The only way out is
  `DELETE`. (Because the owner is always a member, the existing pub/sub
  auto-delete-on-empty path can never fire on a FIFO channel; the channel
  persists until explicit `DELETE`.)

### Destroying a FIFO Channel

`DELETE` (existing message) is the only path. Authorization is owner-only —
already enforced by `delete_channel` in `manager.rs`. On DELETE:

- `CHANNEL_DELETED` event is broadcast to all *other* JOINed members
  (existing path — `notify_channel_deleted` excludes the deleter's
  resource); the deleting owner receives `DELETE_ACK` instead.
- Channel metadata, message log segments, head cursor sidecar, and the
  channel's persistence directory are removed.
- After successful DELETE, the name becomes available for re-creation.

## Protocol Additions

### Configuration messages — `type` field

Two existing structs in `crates/protocol/src/message.rs` are extended:

```rust
// Mutating request — partial update; omitted means "leave unchanged".
pub struct SetChannelConfigurationParameters {
  // ...existing fields...
  pub r#type: Option<StringAtom>, // "pubsub" | "fifo"
}

// Read response (to GET_CHAN_CONFIG) — always carries the current type.
pub struct ChannelConfigurationParameters {
  // ...existing fields...
  pub r#type: StringAtom, // "pubsub" | "fifo"
}
```

The mutating message is `SET_CHAN_CONFIG` (request) →
`SET_CHAN_CONFIG_ACK` (response). `CHAN_CONFIG` is the read response to
`GET_CHAN_CONFIG`, not a mutating verb.

The server validates on `SET_CHAN_CONFIG`:

- Unknown values → `BAD_REQUEST`.
- `type=pubsub` on a channel currently `fifo` → `BAD_REQUEST` ("FIFO is one-way").
- `type=fifo` requires the merged config to satisfy `persist=true` and
  `max_persist_messages > 0` (see above).

A successful change of any field (including `type`) emits a
`CHANNEL_RECONFIGURED` event to all JOINed members.

### `PUSH` / `PUSH_ACK`

```
PUSH id=<corr> channel=<name> length=<N>
<N bytes of binary payload>

→ PUSH_ACK id=<corr>
```

`length` is required (u32, non-zero) and follows the existing
payload-bearing message convention (see `BROADCAST` in `docs/PROTOCOL.md`).

Errors:

- `CHANNEL_NOT_FOUND` if the channel does not exist.
- `USER_NOT_IN_CHANNEL` if the caller is not a member.
- `FORBIDDEN` if the caller is not the owner.
- `WRONG_TYPE` if the channel is pub/sub.
- `QUEUE_FULL` if the logical queue depth is at `max_persist_messages`.
- `POLICY_VIOLATION` if payload exceeds the channel's `max_payload_size`
  (matches the existing `BROADCAST` behavior for the same condition).
- `CURSOR_RECOVERY_REQUIRED` if the channel's head cursor is unrecoverable
  (see [Head Cursor](#head-cursor)).
- `NOT_IMPLEMENTED` if `channel_id.domain != local_domain`.

`PUSH_ACK` carries no `seq` — server-side ordering is implicit and not useful
to the client.

### `POP` / `POP_ACK`

```
POP id=<corr> channel=<name>

→ POP_ACK id=<corr> length=<N> timestamp=<unix-ms>
<N bytes of binary payload>
```

`length` is required (u32, non-zero) on `POP_ACK` and follows the same
payload-framing convention as `MESSAGE`.

Errors:

- `CHANNEL_NOT_FOUND` if the channel does not exist.
- `USER_NOT_IN_CHANNEL` if the caller is not a member.
- `NOT_ALLOWED` if the caller is a member but the read ACL denies them.
- `WRONG_TYPE` if the channel is pub/sub.
- `QUEUE_EMPTY` if the head cursor is at the tail.
- `CURSOR_RECOVERY_REQUIRED` if the channel's head cursor is unrecoverable
  (see [Head Cursor](#head-cursor)).
- `NOT_IMPLEMENTED` if `channel_id.domain != local_domain`.

`POP_ACK` does **not** carry `seq` (no client-visible ordering need; no HISTORY
on FIFO) and does **not** carry `from` (always the owner; redundant).

The cursor advance is fsynced **before** the `POP_ACK` is written to the
socket. This guarantees no duplication: any element whose advance has been
fsynced is gone from the queue forever, even if the server crashes
immediately after. The reverse is **not** guaranteed — a crash between the
cursor fsync and the socket write loses that element (the queue believes it
was delivered; the client never received it). This is at-most-once: an
element may be lost, but never duplicated.

### `GET_CHAN_LEN` / `CHAN_LEN`

```
GET_CHAN_LEN id=<corr> channel=<name>

→ CHAN_LEN id=<corr> channel=<name> count=<N>
```

Returns the number of elements currently between the head cursor and the
tail. The parameter is named `count` (not `length`) because by convention
in this protocol `length` always denotes the byte length of an attached
binary payload (see `BROADCAST`, `MESSAGE`, `MOD_DIRECT`); `CHAN_LEN`
carries no payload, only a queue-depth integer.

Errors:

- `CHANNEL_NOT_FOUND` if the channel does not exist.
- `USER_NOT_IN_CHANNEL` if the caller is not a member.
- `FORBIDDEN` if the caller is not the owner.
- `WRONG_TYPE` if the channel is pub/sub.
- `CURSOR_RECOVERY_REQUIRED` if the channel's head cursor is unrecoverable
  (see [Head Cursor](#head-cursor)).
- `NOT_IMPLEMENTED` if `channel_id.domain != local_domain`.

Only the owner may query the length. Consumers do not need it — they discover
emptiness via `QUEUE_EMPTY` on `POP`. The owner uses it to gauge backpressure
before risking `QUEUE_FULL`.

### Event: `CHANNEL_RECONFIGURED`

A new `EventKind` variant fired to all JOINed members on any successful
`SET_CHAN_CONFIG` request (not only type transitions — also `max_clients`,
`max_payload_size`, `persist`, etc.).

```
EVENT kind=CHANNEL_RECONFIGURED channel=<name>
```

Payload carries only the channel name. Clients re-query
`GET_CHAN_CONFIG` if they care about which fields changed. This keeps event
payloads small and avoids duplicating config state on the event channel.

### New Error Reasons

Added to `narwhal-protocol`:

| Reason             | When                                                              |
| ------------------ | ----------------------------------------------------------------- |
| `QUEUE_EMPTY`       | `POP` against an empty FIFO channel.                              |
| `QUEUE_FULL`        | `PUSH` against a full FIFO channel (at `max_persist_messages`).   |
| `OWNER_CANNOT_LEAVE` | `LEAVE` issued by the owner of a FIFO channel.                    |
| `WRONG_TYPE`        | Operation incompatible with channel type (e.g. `BROADCAST` on FIFO, `PUSH` on pub/sub). |
| `CURSOR_RECOVERY_REQUIRED` | `PUSH` / `POP` / `GET_CHAN_LEN` on a FIFO channel whose head cursor is missing or corrupt; awaiting operator action. |

`FORBIDDEN` (existing) covers non-owner `PUSH` / `DELETE` / `GET_CHAN_LEN` /
`SET_CHAN_CONFIG`. No new reason needed for those.

## Authorization Matrix

| Op             | Pub/Sub                       | FIFO                          |
| -------------- | ----------------------------- | ----------------------------- |
| `JOIN`         | Anyone (subject to join ACL)  | Anyone (subject to join ACL)  |
| `LEAVE`        | Any member                    | Members ✓ / **Owner ✗**       |
| `BROADCAST`    | Members (subject to publish ACL) | **Rejected (`WRONG_TYPE`)** |
| `MESSAGE`      | Sent to read-ACL members      | Never sent                    |
| `HISTORY`      | Members (subject to read ACL) | **Rejected (`WRONG_TYPE`)**    |
| `PUSH`         | **Rejected (`WRONG_TYPE`)**    | Owner only                    |
| `POP`          | **Rejected (`WRONG_TYPE`)**    | Members (subject to read ACL) |
| `GET_CHAN_LEN` | **Rejected (`WRONG_TYPE`)**    | Owner only                    |
| `GET_CHAN_CONFIG` | Any member                  | Any member                    |
| `SET_CHAN_CONFIG` | Owner only (existing)       | Owner only (existing)         |
| `DELETE`       | Owner only (existing)         | Owner only (existing)         |

## Storage

### Reusing the Message Log

FIFO channels reuse the existing per-channel segmented append-only message log
(see [MESSAGE_LOG.md](MESSAGE_LOG.md)) without modification to its on-disk
format. Each `PUSH` is appended exactly like a pub/sub `BROADCAST`:

- 22-byte header + `from` (owner Nid) + payload + CRC32.
- `seq` assigned monotonically by the channel manager (`Channel::next_seq()`)
  before append; the log persists what it is given.
- Index entries every 4096 bytes as before.

This means: the same recovery path, the same CRC handling, the same segment
sizing, the same metrics. We add only a small head-cursor sidecar and a
type-aware filter on the read path.

### Head Cursor

A FIFO channel additionally maintains a **head cursor**: the seq of the next
element to be delivered by `POP`. Stored as a sidecar file inside the
channel's directory:

```
<channels_root>/<channel_hash>/cursor.bin
```

On-disk format (16 bytes, fixed):

```
┌──────────────────┬──────────────────┐
│ next_seq (u64-LE)│ crc32   (u32-LE) │
└──────────────────┴──────────────────┘
                    + 4 bytes pad to 16 (reserved, must be zero)
```

CRC32 is computed over the first 8 bytes (`next_seq`, little-endian).

The sidecar holds only the **head** of the queue. The **tail** (`last_seq` —
the most recently appended seq) is recovered from the message log; the
[tail-segment retention invariant](#segment-eviction) ensures it is always
on disk.

Fields:

- `next_seq` — the lowest seq not yet **committed-as-consumed**: the seq
  the next `POP` will read. Advances when `POP` durably commits the
  consumption (cursor fsync), which happens **before** `POP_ACK` is
  written to the socket. The cursor advance, not `POP_ACK` reaching the
  client, is the commit point — a crash between cursor fsync and socket
  write loses that element on the consumer side (cursor past it; client
  never received bytes). This is the at-most-once consumer-side loss
  model documented in [Cursor Durability](#cursor-durability).

**Invariants** (on disk, after every cursor fsync):

- `next_seq >= 1` (sequence numbers are 1-based; `0` is never valid).
- `next_seq <= log.last_seq() + 1` — the cursor never advances past the
  most recent durable PUSH (see [Cursor Durability](#cursor-durability)).
- `next_seq == log.last_seq() + 1` iff the queue is empty.
- For every seq in `[next_seq ..= log.last_seq()]`, the entry is present
  and CRC-valid in the message log (head-eviction must never delete an
  entry whose `seq >= next_seq`).

**Initial state** (just after `SET_CHAN_CONFIG type=fifo`, before any FIFO
`PUSH`):

- For a channel with no prior pub/sub messages: `next_seq = 1`,
  `log.last_seq() = 0`.
- For a channel that had pub/sub messages with last assigned seq `L`:
  `next_seq = L + 1`, `log.last_seq() = L`.

**Recovery on startup:**

1. Load `cursor.bin`. If the file is missing or its CRC fails →
   `CURSOR_RECOVERY_REQUIRED`.
2. Decode `next_seq`. If `next_seq < 1` → `CURSOR_RECOVERY_REQUIRED`.
3. Cross-validate against the message log:
   - `next_seq <= log.last_seq() + 1` must hold. The cursor cannot legally
     advance past the latest durable log entry, because `POP` ensures log
     durability up to the consumed seq before fsyncing the cursor (see
     [Cursor Durability](#cursor-durability)). A violation indicates log
     corruption (a fsynced entry was lost) → `CURSOR_RECOVERY_REQUIRED`.
   - If the queue is non-empty (`next_seq <= log.last_seq()`):
     `log.first_seq() <= next_seq` must also hold. If
     `log.first_seq() > next_seq`, head-eviction has gone past the cursor
     and unread entries are gone → `CURSOR_RECOVERY_REQUIRED`.
   - If the queue is empty (`next_seq == log.last_seq() + 1`): nothing
     further to validate.
4. If all checks pass, the channel is up.

**PUSH durability and silent loss.** `PUSH` durability follows the
existing pub/sub `BROADCAST` rules driven by `message_flush_interval`
(see [Cursor Durability](#cursor-durability) for the full path):

- With `message_flush_interval == 0` (the auto-create default), the
  log entry is fsynced before `PUSH_ACK`. A hard crash after the ACK
  cannot lose the entry.
- With `message_flush_interval > 0`, log writes are batched. A hard
  crash before the next periodic flush silently drops the most recent
  PUSHes that the producer received ACKs for. On restart,
  `log.last_seq()` simply reflects the last fsynced entry; the lost
  entries do not surface — same as `BROADCAST`.

The cursor is unaffected in either case: only entries that were never
POPed (and therefore never contributed to a cursor advance) can be
lost. See [Cursor Durability → Producer-side ACK-loss
ambiguity](#cursor-durability) and the [out-of-scope server-side PUSH
idempotency item](#out-of-scope-v1).

If recovery fails, **operator recovery** is one of:

1. Restore the sidecar from a backup.
2. Explicitly accept loss: write a fresh sidecar with
   `next_seq = log.last_seq() + 1` (treats whatever is in the log as
   already-consumed).
3. `DELETE` the channel.

`length` reported by `GET_CHAN_LEN` is computed from the cursor and the log
(the log is consulted only for `last_seq` and entry retrieval):

```text
length = if next_seq > log.last_seq() { 0 } else { log.last_seq() - next_seq + 1 }
```

The guard handles the empty-queue invariant `next_seq == log.last_seq() + 1`,
where `log.last_seq() - next_seq + 1` would underflow as a `u64`.

### Cursor Durability

`POP` performs an **immediate fsync** on the cursor sidecar before sending
`POP_ACK`. `PUSH` log writes follow the existing pub/sub durability rules
driven by `message_flush_interval` — FIFO does **not** diverge from
`BROADCAST` here. The cursor sidecar is only ever written by `POP` (and
once at type transition).

**`PUSH` path:**

1. Append the entry to the message log buffer.
2. If `message_flush_interval == 0`: synchronously flush+fsync the log
   (per the existing `BROADCAST` semantics — see
   [`docs/PROTOCOL.md`](../PROTOCOL.md) and `broadcast_payload` in
   `crates/server/src/channel/manager.rs`). Otherwise the entry stays in
   the log buffer until the next periodic flush.
3. Send `PUSH_ACK`.

Therefore `PUSH_ACK` durability **mirrors `BROADCAST` exactly**:

- **`message_flush_interval == 0` (the auto-create default):** the entry
  is fsynced before `PUSH_ACK` is sent. A hard crash after `PUSH_ACK`
  cannot lose the entry. (`PUSH_ACK` still does not survive an in-flight
  network drop — see ACK-loss ambiguity below.)
- **`message_flush_interval > 0`:** the entry sits in the log buffer
  until the next periodic flush (or until a concurrent `POP` forces one,
  see the `POP` path below). A hard crash before the next flush silently
  drops the entry — same as `BROADCAST`.

The choice between the two is the operator's, configured via
`SET_CHAN_CONFIG`; FIFO does not impose its own value.

**Producer-side ACK-loss ambiguity.** Whether or not the log fsync ran,
the network may still drop `PUSH_ACK` (or the server may crash between
the fsync and the socket write). The producer therefore cannot
distinguish a successful-but-unacknowledged `PUSH` from a failed one
just from the absence of `PUSH_ACK`. Retrying on missing `PUSH_ACK` may
therefore add a duplicate entry to the queue. v1 has no server-side
dedup (no client-supplied idempotency ID); producers needing exactly-one
logical delivery must dedup at the application layer (embedded payload
ID consumed downstream). A future at-least-once / lease-based mode would
address this server-side; out of scope for v1 (see
[Out of Scope](#out-of-scope-v1)).

**`POP` path:**

1. Read the entry at `next_seq` from the message log.
2. Ensure the entry is durable: if `next_seq > log.last_durable_seq()`,
   trigger a synchronous log flush+fsync. If the background flusher
   already covered the entry, this is a cheap no-op. **This step is
   required:** without it, `POP` could read a buffered (not-yet-fsynced)
   entry, advance the cursor durably, and crash before the log was
   flushed — leaving `cursor.next_seq > log.last_seq() + 1` on disk,
   which the recovery model treats as corruption.
3. Update `cursor.bin` with `next_seq = previous_next_seq + 1`.
4. `fsync` `cursor.bin`.
5. Send `POP_ACK`.

A crash between (4) and (5) loses that element on the consumer side
(cursor advanced; client never received bytes). This is the at-most-once
consumer-side loss model: **no duplication, possible loss**. The
no-duplication contract is the load-bearing one — at no point can two
clients (or one client across retries) receive the same physical queue
element.

**Cursor write protocol** (atomic, mirrors the channel-store write — see
[CHANNEL_STORE.md](CHANNEL_STORE.md)):

1. Write to `cursor.bin.tmp`.
2. `fsync` the tmp file.
3. `rename` over `cursor.bin`.
4. `fsync` the parent directory.

**Throughput cost.** `PUSH` matches `BROADCAST` semantics across both
flush modes — synchronous fsync per `PUSH` when
`message_flush_interval == 0`, batched flushing on the configured
interval otherwise. `POP` costs **one** cursor fsync in the common
case; in the worst case (POP racing ahead of the background flusher),
it adds a single log flush+fsync. Per-channel `POP` throughput is
bounded by `1 / fsync_latency`. Workloads needing higher rates should
shard logically across multiple FIFO channels, or wait for a future
batched-cursor / lease-based mode (out of scope for v1).

### Segment Eviction

FIFO eviction has two ends:

- **Tail eviction (existing pub/sub behavior) is disabled.** `PUSH` is
  rejected with `QUEUE_FULL` whenever the **logical queue depth** would
  exceed `max_persist_messages` after appending. Logical depth comes from
  the cursor's `next_seq` and the log's `last_seq()` — **not** from the
  physical number of entries on disk. Popped elements remain on disk until
  their segment is head-evicted, so a check against physical retention
  would spuriously reject `PUSH`es after consumers have drained part of a
  segment.

  Concretely, before append, compute logical depth with a guard against
  `u64` underflow on an empty queue:

  ```text
  depth = if next_seq > log.last_seq() { 0 } else { log.last_seq() - next_seq + 1 }
  ```

  If `depth >= max_persist_messages`, return `QUEUE_FULL`; otherwise append.

- **Head eviction (new).** When the cursor advances past the end of the
  oldest segment, that segment becomes unreachable and is deleted from
  disk. This reclaims space as consumers drain the queue.

**Tail-segment retention invariant.** Head-eviction must **never delete
the segment containing `log.last_seq()`** (the most recently appended
entry's segment), even when all of its entries have been logically popped
(cursor past their seq). Without this, a fully-drained queue could lose
its seq counter, breaking length math, the `QUEUE_FULL` check, and the
recovery procedure (which all consult `log.last_seq()`). After a
subsequent `PUSH` rolls into a new segment, the previous tail segment
becomes a regular candidate for head-eviction, so the maximum number of
"stranded" segments holding only popped entries is **one**.

Both behaviors are implemented inside the message log, gated on the
channel's type (or on a flag passed at log creation time).

### Channel Metadata

`PersistedChannel` (defined in `crates/server/src/channel/store.rs`; used
by `file_store.rs`) gains one new field:

- `channel_type: ChannelType` — enum `{ PubSub, Fifo }`.

This is a positional postcard schema change. Because the project is
**pre-production**, no migration path is required: existing on-disk
`metadata.bin` files written before this change become unreadable and the
expected operator action is to wipe `<channels_root>` and let the server
start fresh. (If/when the project ships, a versioned envelope or per-version
decoder will need to be introduced — out of scope here.)

The owner is already persisted in `PersistedChannel`. No change there.

The cursor is **not** part of the postcard-encoded metadata file — it is its
own sidecar (`cursor.bin`, see above) so that the high-frequency cursor fsync
does not require rewriting the larger metadata blob.

## Behavior Overrides vs. Pub/Sub

The following pub/sub paths must branch on channel type:

| Path                                  | Pub/Sub                          | FIFO                                |
| ------------------------------------- | -------------------------------- | ----------------------------------- |
| `LEAVE` handler (manager.rs)          | Removes member; clears owner if owner leaves; auto-deletes channel if last member | Returns `OWNER_CANNOT_LEAVE` if owner; otherwise removes member. Auto-delete-on-empty is implicitly unreachable since the owner is always a member. |
| `remove_member` (manager.rs)          | Clears `owner` if owner is the leaver | Never reached for owner — LEAVE rejects upstream |
| Empty-channel auto-delete sites (manager.rs) | Removes channel | Unreachable for FIFO (precondition `member_count == 0` cannot be met). Defensive type-gate is optional. |
| Message log eviction policy           | Tail-evict at cap                | Reject PUSH at cap (`QUEUE_FULL`)    |
| `BROADCAST` handler                   | Append + fan out                 | `WRONG_TYPE`                         |
| `HISTORY` / `CHAN_SEQ` handlers       | Read from log                    | `WRONG_TYPE`                         |

Pub/sub paths that **do not** need to branch:

- JOIN handler (existing semantics work as-is).
- ACL evaluation (read ACL gates POP, exactly as it gates MESSAGE delivery).
- Channel store save / load (just adds a `channel_type` field).
- DELETE (already owner-only; just needs to clean up the cursor sidecar in
  addition to existing cleanup).

## Operational Notes

### Polling Cost

With empty-`POP` returning `QUEUE_EMPTY` immediately and no push notification on
`PUSH`, idle consumers have to poll. For an idle FIFO channel with `C`
consumers polling at rate `R`, the server processes `C × R` `POP` round trips
per second that all return `QUEUE_EMPTY`.

This is acceptable for v1 because:

- The actor is single-threaded per channel and `POP` on an empty queue is a
  cheap branch (no log read, no fsync).
- Consumers can implement application-side exponential backoff.

A future wake-on-push extension is sketched in [Out of Scope (v1)](#out-of-scope-v1).

### Per-POP fsync

Throughput is bounded by `1 / fsync_latency` per channel. On typical SSDs that
is on the order of single-digit thousands of POPs/sec/channel. Workloads
needing higher rates should:

- Use multiple FIFO channels (sharded by partition key) to parallelize across
  shards.
- Or wait for a batched-cursor mode (future work).

### Metrics

New metrics under the `narwhal` channel prefix:

| Metric                        | Type      | Notes                                  |
| ----------------------------- | --------- | -------------------------------------- |
| `fifo_pushes{result}`         | Counter   | One label per `PUSH` outcome — see below. |
| `fifo_pops{result}`           | Counter   | One label per `POP` outcome — see below.  |
| `fifo_queue_depth`            | Gauge     | Per-channel? Or aggregate? (see implementation note) |
| `fifo_cursor_fsync_seconds`   | Histogram | Wall time of cursor fsync.             |

**`fifo_pushes{result}` labels** (must cover every documented `PUSH`
outcome — see [PUSH errors](#push--push_ack)):

- `success`
- `channel_not_found`
- `user_not_in_channel`
- `forbidden` (caller is not the owner)
- `wrong_type` (channel is pub/sub)
- `queue_full`
- `policy_violation` (payload exceeds `max_payload_size`)
- `cursor_recovery_required`
- `not_implemented` (non-local domain)

**`fifo_pops{result}` labels** (must cover every documented `POP`
outcome — see [POP errors](#pop--pop_ack)):

- `success`
- `channel_not_found`
- `user_not_in_channel`
- `not_allowed` (read ACL denies the caller)
- `wrong_type` (channel is pub/sub)
- `queue_empty`
- `cursor_recovery_required`
- `not_implemented` (non-local domain)

Implementations must emit exactly one of these labels per call. If a
new outcome is added in the future and the implementation has not yet
been updated, it should be bucketed into a single `other` label rather
than dropped — and the doc updated alongside.

Per-channel gauges blow up cardinality. The `fifo_queue_depth` gauge
should be aggregate (sum across channels) or omitted in favor of an
operator query against `GET_CHAN_LEN`. Decide during implementation.

## Out of Scope (v1)

The following are deliberately deferred:

- **Wake-on-push (long-poll).** Server holds a waiter queue per channel; `PUSH`
  hands the element directly to the head waiter without traversing the log
  (or just wakes them to re-issue `POP`). Eliminates polling cost. Additive
  feature; no protocol break.
- **At-least-once with explicit ACK.** `POP` returns a lease; client must
  `ACK <lease>`; on lease expiry the element is redelivered. Requires
  per-element lease state, visibility timeout configuration, and a redelivery
  scheduler. Significantly more machinery.
- **Server-side PUSH idempotency.** A producer-supplied dedup ID on `PUSH`,
  with the server tracking recent IDs and short-circuiting duplicates, would
  let producers safely retry on missing `PUSH_ACK` without risking duplicate
  queue entries (see [Cursor Durability → Producer-side ACK-loss
  ambiguity](#cursor-durability)). v1 punts: producers handle duplicates at
  the application layer if needed.
- **Batched POP (`POP n=K`).** Single-`POP` return value lets us avoid framing
  decisions for v1.
- **Type-conversion FIFO → pub/sub.** Would require defining what happens to
  the head cursor and the queue tail. Not worth solving until there's a real
  use case.
- **Shared ownership / owner transfer.** Single immutable owner is the simpler
  contract.

## Implementation Checklist

A non-binding outline of the work units. Order is suggestive, not prescriptive.

1. **Protocol crate (`narwhal-protocol`):**
   - Add `type: Option<StringAtom>` to `SetChannelConfigurationParameters`
     (mutating request, partial-update).
   - Add `type: StringAtom` to `ChannelConfigurationParameters` (read response
     to `GET_CHAN_CONFIG`).
   - Add `Push`, `PushAck`, `Pop`, `PopAck`, `GetChannelLen`, `ChannelLen`
     message variants. `Push` and `PopAck` carry `length` like `Broadcast` /
     `Message` and a binary payload.
   - Add `EventKind::ChannelReconfigured`.
   - Add error reasons: `QUEUE_EMPTY`, `QUEUE_FULL`, `OWNER_CANNOT_LEAVE`,
     `WRONG_TYPE`, `CURSOR_RECOVERY_REQUIRED`.
   - Update serialize/deserialize/validate/test fixtures.
2. **Channel manager (`crates/server/src/channel/manager.rs`):**
   - Add `Channel.channel_type: ChannelType` and `Channel.cursor` (head seq).
   - Add `Command::Push`, `Command::Pop`, `Command::GetChannelLen`.
   - Implement transition validation in `set_channel_configuration`,
     including **seq-counter alignment**: when transitioning to
     `type=fifo`, set `channel.seq = log.last_seq() + 1` before writing
     the cursor sidecar. Necessary because non-persistent `BROADCAST`s
     advance `channel.seq` without touching the log (see
     [Becoming a FIFO Channel](#becoming-a-fifo-channel)).
   - Enforce the **crash-safe transition order** (see
     [Becoming a FIFO Channel](#becoming-a-fifo-channel)): `cursor.bin`
     written + fsynced first, then `metadata.bin` updated + fsynced,
     then `SET_CHAN_CONFIG_ACK` sent. A crash before metadata is
     written must leave the channel as pub/sub (a stranded
     `cursor.bin` is acceptable — pub/sub ignores it).
   - Implement type-aware branches in `LEAVE` (reject for owner with
     `OWNER_CANNOT_LEAVE`), `BROADCAST`, `HISTORY` / `CHAN_SEQ` (all return
     `WRONG_TYPE`).
   - Emit `CHANNEL_RECONFIGURED` from `set_channel_configuration` after a
     successful change.
3. **Message log (`crates/server/src/...message_log...`):**
   - Add a constructor flag (or per-call mode) to disable rolling tail
     eviction and enable head-driven segment delete, with the
     [tail-segment retention invariant](#segment-eviction): head-eviction
     never deletes the segment containing `last_seq()`.
   - Expose `first_seq()`, `last_seq()`, and `last_durable_seq()` (the
     highest seq fsynced to disk) for the `POP` path's
     ensure-durability check.
   - Expose a synchronous flush that fsyncs the log buffer up to a
     specified seq, used by `POP` when the entry it is reading is not
     yet durable.
   - `PUSH` durability follows existing pub/sub batching driven by
     `message_flush_interval` — no FIFO-specific change.
4. **Cursor sidecar:**
   - 16-byte layout: `next_seq` (u64-LE) + `crc32` (u32-LE) + 4-byte
     zero pad. CRC32 is over `next_seq` only.
   - Atomic write protocol (tmp → fsync → rename → fsync parent dir).
   - One-time write at type transition (`SET_CHAN_CONFIG type=fifo`) to
     initialize `next_seq = log.last_seq() + 1`.
   - **`POP` path** (only writer in steady state):
     1. Read entry at `next_seq` from the log.
     2. If `next_seq > log.last_durable_seq()`, force a log flush+fsync
        up to `next_seq`.
     3. Advance `next_seq`, atomic-write + fsync the cursor sidecar.
     4. Send `POP_ACK`.
   - **`PUSH` path** does **not** touch the cursor sidecar; it just
     appends to the log buffer and sends `PUSH_ACK` (durability via
     batched flush, like pub/sub `BROADCAST`).
   - Recovery: validate `next_seq >= 1`, `next_seq <= log.last_seq() + 1`,
     and `log.first_seq() <= next_seq` when the queue is non-empty; on
     any failure surface `CURSOR_RECOVERY_REQUIRED`.
5. **Channel store (`store.rs` for the schema; `file_store.rs` for
   on-disk I/O):**
   - Add `channel_type: ChannelType` (enum `{ PubSub, Fifo }`) to
     `PersistedChannel` (defined in `store.rs`). Existing on-disk
     `metadata.bin` files written before this change will fail to
     decode — acceptable because the project is pre-production;
     operators wipe `<channels_root>` before upgrading. (A versioned
     envelope can be introduced later when production durability is
     required; out of scope here.)
   - On `DELETE`, also remove `cursor.bin`.
6. **C2S dispatch (`c2s/conn.rs`):**
   - Wire new messages to channel-manager commands.
7. **Metrics:** register the new counters/histogram (see [Metrics](#metrics)).
8. **Tests:**
   - Unit tests for transition validation (rejects when `persist=false`,
     rejects FIFO → pub/sub, etc.).
   - **Transient-broadcast-then-FIFO test (seq alignment):** auto-create
     a channel via `JOIN` (`persist=false` by default), issue several
     non-persistent `BROADCAST`s so `channel.seq` advances without log
     writes, then `SET_CHAN_CONFIG type=fifo persist=true` and verify
     (a) `GET_CHAN_LEN` returns 0, (b) the first FIFO `PUSH` is assigned
     `seq = log.last_seq() + 1` (= 1 in the all-non-persistent case),
     and (c) a subsequent `POP` reads that entry cleanly. Without seq
     alignment the first PUSH would land at a phantom seq beyond the
     cursor and `POP` would never see it.
   - Integration tests for PUSH/POP semantics: ordering, competing consumers,
     QUEUE_EMPTY, QUEUE_FULL, owner-only PUSH, JOIN-required POP, read-ACL
     denial returns `NOT_ALLOWED` on POP.
   - `QUEUE_FULL`-via-logical-depth test: PUSH up to the cap, POP some, confirm
     PUSH succeeds again before any segment has been head-evicted (ensures
     the capacity check is logical, not physical).
   - **Transition + restart test:** broadcast some messages on a pub/sub
     channel, transition to FIFO, restart **before** any FIFO `PUSH`, then
     verify (a) `GET_CHAN_LEN` returns 0, (b) the cursor is still considered
     valid (no `CURSOR_RECOVERY_REQUIRED`), and (c) the first FIFO `PUSH`
     after restart is assigned `prev_last_seq + 1` (seq monotonicity).
   - Restart recovery test: PUSH N elements, restart, POP N elements, verify
     order and at-most-once.
   - **Drain + restart + PUSH test (tail-segment retention):** PUSH N
     elements (let the background flusher run), POP all N, restart, then
     `PUSH` one more element and verify it is assigned `seq = N + 1`. This
     exercises that the segment containing `log.last_seq()` is retained
     across drain — without the invariant, head-eviction would remove the
     last segment and the seq counter would reset on restart.
   - **Force-flush-on-POP test:** with `message_flush_interval` set to a
     long value (e.g. 60s), issue a `PUSH` followed immediately by a
     `POP` and confirm `POP_ACK` arrives well before the flush interval
     elapses (i.e. `POP` triggered a synchronous log flush rather than
     waiting for the periodic flush).
   - **Durable-PUSH test (`message_flush_interval=0`):** with
     `message_flush_interval` set to 0 (the auto-create default), `PUSH`
     an element, kill the process, restart, and confirm the element is
     present and `POP`-able. This locks in the "0 means immediate
     flush" contract that FIFO inherits from `BROADCAST`.
   - **Note on testing the non-durable case (`message_flush_interval > 0`).**
     The contract is "no durability guarantee", **not** "guaranteed
     loss": `write_all_at` lands in the OS page cache, which a process
     `kill` does not discard. Reliably observing the loss requires an
     OS-level crash (VM reset / power cut), which is not in the unit/
     integration test budget. Keep this case to a contract assertion
     only — do not write a test that expects the entry to be absent.
   - Crash-during-POP test (kill between cursor fsync and `POP_ACK` socket
     write): verify the next consumer does **not** see the lost element
     (no duplication is the load-bearing contract).
   - Cursor-corruption test: corrupt or delete `cursor.bin` between runs,
     verify the channel refuses `PUSH`/`POP`/`GET_CHAN_LEN` with
     `CURSOR_RECOVERY_REQUIRED` until an operator-supplied recovery action.
9. **Docs:**
   - Update `docs/PROTOCOL.md` to document the new messages, errors, and the
     `type` field. Add a 1.5 changelog entry.
   - Mark this doc's status as **Implemented** once 1–8 land.
