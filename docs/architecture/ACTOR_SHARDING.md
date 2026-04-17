# Actor-Based Sharding — Architecture Specification

> **Status:** Implemented.
> **Last updated:** 2026-04-17

## Table of Contents

- [Overview](#overview)
- [Goals and Non-Goals](#goals-and-non-goals)
- [CoreDispatcher](#coredispatcher)
  - [Thread Model](#thread-model)
  - [Bootstrap](#bootstrap)
  - [Dispatch](#dispatch)
  - [Shutdown](#shutdown)
- [Shard Actor Pattern](#shard-actor-pattern)
  - [Key → Shard Assignment](#key--shard-assignment)
  - [Mailboxes and Commands](#mailboxes-and-commands)
  - [Request / Reply](#request--reply)
  - [Shard Actor Loop](#shard-actor-loop)
  - [Lifecycle: new → bootstrap → shutdown](#lifecycle-new--bootstrap--shutdown)
  - [Clone and Sharing](#clone-and-sharing)
- [Concrete Subsystems](#concrete-subsystems)
  - [c2s::Router](#c2srouter)
  - [ChannelManager](#channelmanager)
  - [Membership](#membership)
- [System-Wide Shutdown Order](#system-wide-shutdown-order)
- [Concurrency Model](#concurrency-model)
- [Constants](#constants)
- [Dependencies](#dependencies)
- [Testing](#testing)
- [File Locations](#file-locations)

---

## Overview

Narwhal runs N OS threads, each pinned to a CPU core and hosting a
[compio](https://github.com/compio-rs/compio) runtime (accessed through the
`narwhal_common::runtime` shim). A small set of stateful
subsystems — the C2S connection router, the channel manager, and the
per-user membership tracker — shard their state across those threads. Each
shard is an **actor**: it owns a partition of the subsystem's data, receives
`Command` messages through a bounded mailbox, and processes them one at a
time on its home thread.

The pattern is deliberately uniform. All three subsystems are built from the
same four pieces:

```
┌────────────────────────────────────────────────────────────────┐
│                        CoreDispatcher                          │
│   (N threads, each running a compio runtime pinned to a core)  │
│                                                                │
│   ┌──────────────┐   ┌──────────────┐       ┌──────────────┐   │
│   │  Worker 0    │   │  Worker 1    │  ...  │  Worker N-1  │   │
│   │              │   │              │       │              │   │
│   │ ┌──────────┐ │   │ ┌──────────┐ │       │ ┌──────────┐ │   │
│   │ │ RouterSh │ │   │ │ RouterSh │ │       │ │ RouterSh │ │   │
│   │ ├──────────┤ │   │ ├──────────┤ │       │ ├──────────┤ │   │
│   │ │ ChannelSh│ │   │ │ ChannelSh│ │       │ │ ChannelSh│ │   │
│   │ ├──────────┤ │   │ ├──────────┤ │       │ ├──────────┤ │   │
│   │ │ MemberSh │ │   │ │ MemberSh │ │       │ │ MemberSh │ │   │
│   │ └──────────┘ │   │ └──────────┘ │       │ └──────────┘ │   │
│   └──────────────┘   └──────────────┘       └──────────────┘   │
│         ▲                   ▲                      ▲           │
│         │                   │                      │           │
│         └─── mailbox[0] ────┴─── mailbox[1] ───────┘           │
│                  (async_channel::bounded)                      │
└────────────────────────────────────────────────────────────────┘
                             ▲
                             │ send(Command { .., reply_tx })
                             │ reply_tx.recv().await
                             │
                      ┌─────────────┐
                      │   Caller    │   (any task, any thread)
                      │ (dispatcher,│
                      │  listener,  │
                      │  handler…)  │
                      └─────────────┘
```

Each subsystem chooses a **sharding key** (username, channel handler,
username), hashes it, and routes the command to the owning shard. The shard
holds an ordinary `HashMap<Key, State>` and mutates it with plain `&mut self`
— no locks, no atomics on the state itself, because every command for a given
key is serialized through a single thread.

## Goals and Non-Goals

**Goals:**
- CPU-local state with no synchronization on the hot path. Per-shard state is
  plain `HashMap`; commands for the same key are serialized by construction.
- Scale horizontally with cores by partitioning state, not by contending on
  shared locks.
- A single repeatable pattern that can host unrelated subsystems side-by-side
  on the same worker pool.
- Clean lifecycle: every subsystem can be constructed eagerly and bootstrapped
  later, and can be shut down deterministically.

**Non-Goals:**
- Cross-shard transactions. A command that needs state from two shards must
  perform two round-trips (see [Membership](#membership) for the canonical
  pattern).
- Work-stealing. Each shard is pinned; an overloaded shard cannot migrate
  work to an idle one.
- Dynamic resharding. Shard count is fixed for the lifetime of the process.
- A generic reusable framework. The pattern is copy-pasted across three
  subsystems intentionally (see [Shard Actor Pattern](#shard-actor-pattern)).

## CoreDispatcher

`CoreDispatcher` is the shared worker pool that every sharded subsystem
builds on top of. It knows nothing about actors, commands, or mailboxes — it
just accepts closures and runs them on a specific worker.

### Thread Model

| Property | Value |
|----------|-------|
| Thread count | `num_workers()` — `NARWHAL_NUM_WORKERS` env var, or `available_parallelism()` |
| Thread name | `core-worker-{id}` |
| Runtime per thread | compio (via `narwhal_common::runtime::try_block_on`) |
| CPU affinity | `core_affinity::set_for_current(core_ids[id % cores.len()])`, best-effort |
| Signal mask | `SIGINT`, `SIGTERM`, `SIGQUIT`, `SIGHUP`, `SIGUSR1`, `SIGUSR2`, `SIGPIPE` blocked on worker threads (handled only by the main thread) |

Blocking the signals before spawning means every worker inherits the blocked
mask, so only the main thread receives Ctrl-C and friends. The original mask
is restored on the main thread after all workers are spawned.

### Bootstrap

```rust
impl CoreDispatcher {
    pub fn new(worker_count: usize) -> Self;
    pub async fn bootstrap(&mut self) -> anyhow::Result<()>;
    pub fn shard_count(&self) -> usize;
}
```

`bootstrap()` does the heavy lifting:

```
bootstrap():
│
├─ Block inheritable signals on the current (caller) thread.
│
├─ For each worker 0..N:
│   ├─ (task_tx, task_rx)       = async_channel::unbounded::<SpawnFn>()
│   ├─ (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1)
│   ├─ (ready_tx, ready_rx)     = async_channel::bounded::<()>(1)
│   │
│   └─ std::thread::Builder::new().name("core-worker-{id}").spawn(|| {
│         set_for_current(core_id);          // best-effort CPU pinning
│         runtime::try_block_on(async {
│             runtime::spawn_detached(async {
│                 while let Ok(f) = task_rx.recv().await { f(); }
│             });
│             ready_tx.send(()).await;        // signal readiness
│             shutdown_rx.recv().await;       // keep runtime alive
│         });
│     });
│
├─ Restore the original signal mask.
│
└─ Await every ready_rx — returns only when every worker's runtime is up.
```

Each worker has two roles in its single compio runtime:

1. A **detached drainer task** that loops over the worker's `task_rx`,
   receiving `SpawnFn = Box<dyn FnOnce() + Send + 'static>` closures and
   running them. Each closure typically calls `runtime::spawn_detached(f())`
   on its own future, so individual dispatches become independent tasks on
   that worker's runtime.
2. The **top-level future** that waits on `shutdown_rx`. This is what keeps
   `try_block_on` from returning; receiving a shutdown signal (or channel
   closure) is the cue to tear down the worker.

The task channel is `async_channel::unbounded`. Bounded mailboxes live one
level up (per subsystem); the dispatcher's job is just "run this closure on
that worker, eventually."

### Dispatch

```rust
pub async fn dispatch_at_shard<F, Fut>(&self, shard: usize, f: F) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static;
```

The implementation is a one-liner over the worker's task channel:

```rust
let task: SpawnFn = Box::new(move || {
    runtime::spawn_detached(f());
});
self.senders[shard].send(task).await
    .map_err(|_| anyhow!("shard {} channel closed", shard))
```

Two things to note:

- The closure `f` is `Send + 'static` but the **future it returns** is only
  `'static`. That means `f` runs on the worker thread, constructs the
  future there, and hands it to `spawn_detached`. The future itself never
  crosses a thread boundary, so it does not need to be `Send` — compio's
  per-worker runtime is single-threaded and its tasks are spawned locally.
- `dispatch_at_shard` itself does not await the future's completion. It only
  awaits enqueueing the closure on the worker's drainer task. Subsystems that
  need completion semantics layer their own reply channels on top.

### Shutdown

```rust
pub async fn shutdown(&mut self) -> anyhow::Result<()>;
```

Two-phase:

```
1. For each worker's task sender: sender.close()
   — the drainer task sees Err on recv(), exits.
2. For each worker: shutdown_tx.send(()).await
   — the top-level future wakes, try_block_on returns, thread exits.
3. join() every worker thread. Panics are logged, not propagated.
```

Task channel closure and shutdown signal are both necessary: closing the task
channel only terminates the drainer task, not the runtime itself. The
`shutdown_rx.recv()` await is what actually lets the runtime unwind.

## Shard Actor Pattern

Every sharded subsystem in narwhal is a variation on the following shape.
Rather than abstract it into a framework, the same ~50 lines are written
out three times (router, channel manager, membership) — each with its own
`Command` enum, shard struct, and sharding key. The pattern is simple enough
that explicit code reads better than a generic trait tower.

### Key → Shard Assignment

```rust
fn shard_for(key: &StringAtom, shard_count: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % shard_count
}
```

| Property | Value |
|----------|-------|
| Hasher | `std::hash::DefaultHasher` (SipHash-1-3 today) |
| Seed | Fresh per call — **stateless**, so every caller agrees on the shard for a given key |
| Distribution | Uniform in expectation; no rebalancing |
| Sharing | Defined independently in each subsystem; not a shared utility |

Using `DefaultHasher` (rather than a faster non-cryptographic hash) gives
resistance to accidentally-adversarial input — e.g. a client spamming
similar usernames — at the cost of a few nanoseconds per dispatch.

### Mailboxes and Commands

```rust
enum Command {
    DoThing { key: StringAtom, args: .., reply_tx: Sender<T> },
    ..
}

#[derive(Clone)]
pub struct Subsystem {
    mailboxes: Arc<[Sender<Command>]>,
    mailbox_capacity: usize,
    // plus any shared state (atomics, Arc<DashMap>, ..)
}
```

| Property | Value |
|----------|-------|
| Channel type | `async_channel::bounded` (MPSC in practice, MPMC by type) |
| Capacity | `DEFAULT_MAILBOX_CAPACITY = 16384` per shard |
| Storage | `Arc<[Sender<Command>]>` — fixed-size after bootstrap, cheap to clone |
| Backpressure | `mailboxes[shard].send(cmd).await` parks the caller when the shard is full |

The `Arc<[Sender<Command>]>` is rebuilt from a `Vec` at the end of
`bootstrap()` so it can be cheaply cloned into every `Subsystem` handle and
into every caller that holds one.

A bounded mailbox is **load-bearing for correctness**, not just memory
hygiene: it converts "too much work for one shard" from an unbounded memory
leak into explicit backpressure on the caller.

### Request / Reply

Commands that need a response carry a one-shot reply channel:

```rust
// caller:
let (reply_tx, reply_rx) = async_channel::bounded(1);
mailboxes[shard].send(Command::DoThing { .., reply_tx }).await?;
let result = reply_rx.recv().await?;

// shard actor:
let result = self.do_thing(..);
let _ = reply_tx.send(result).await;
```

| Property | Value |
|----------|-------|
| Channel type | `async_channel::bounded(1)` |
| Lifetime | One message, then dropped on both ends |
| Error handling | Caller treats `Err` (sender dropped without replying) as subsystem-closed |

`async_channel::bounded(1)` is used rather than `oneshot` because the rest of
the code already depends on `async_channel` and the one-message restriction
is enforced by convention — every call site sends at most once. Fire-and-
forget commands (e.g. `Router::RouteTo`) simply omit `reply_tx`.

### Shard Actor Loop

```rust
struct Shard {
    state: HashMap<StringAtom, ..>,
    mailbox: Receiver<Command>,
    // shared state (atomics, membership handle, etc.)
}

impl Shard {
    async fn run(mut self) {
        while let Ok(cmd) = self.mailbox.recv().await {
            self.handle(cmd).await;
        }
        // (optional) shutdown hooks
    }
}
```

Exit condition: `mailbox.recv()` returns `Err` iff all senders are closed
*and* the queue is empty. Closing the `Arc<[Sender<_>]>`'s members (done by
the subsystem's `shutdown()`) causes every shard's loop to exit naturally
once drained.

`handle(cmd)` is `&mut self`, single-threaded, and typically **awaits**
while processing (e.g. a channel shard calls `self.store.save_channel()`
and `self.message_log.append()`, both async). That serialization is the
whole point: it is what makes per-shard state lock-free.

### Lifecycle: new → bootstrap → shutdown

Every subsystem follows the same three-step lifecycle:

```rust
impl Subsystem {
    // Cheap, synchronous. Leaves `mailboxes` empty.
    pub fn new(..) -> Self;

    // Spawns one shard actor per worker on the dispatcher.
    // Fills in `mailboxes` after all shards are enqueued.
    pub async fn bootstrap(&mut self, core_dispatcher: &CoreDispatcher)
        -> anyhow::Result<()>;

    // Closes every sender; shard loops exit once drained.
    pub fn shutdown(&self);
}
```

| Phase | What it does | Why split |
|-------|--------------|-----------|
| `new` | Allocates the struct; `mailboxes = Arc::from([])` | Lets callers hold references before the thread pool exists |
| `bootstrap` | Opens mailboxes; dispatches one shard actor per worker | Requires the `CoreDispatcher` to be booted |
| `shutdown` | `tx.close()` on every mailbox sender | Drains in-flight work without forcing a deadline |

`shutdown()` is **synchronous and fire-and-forget**. It does not wait for
shards to finish processing in-flight commands; that's `core_dispatcher.
shutdown().await` (which joins the worker threads after their runtimes
unwind). Calling a subsystem's `shutdown()` before the dispatcher's is what
guarantees shards see "mailbox closed" and exit their loops cleanly, rather
than getting cancelled mid-command when the runtime is torn down.

An `assert_bootstrapped` debug check in most subsystems catches the
"used before bootstrap" footgun early:

```rust
fn assert_bootstrapped(&self) {
    debug_assert!(!self.mailboxes.is_empty(),
        "Subsystem::bootstrap() must be called before use");
}
```

### Clone and Sharing

Every subsystem is designed to be cheaply `Clone`-able so it can be handed
to every connection handler, dispatcher factory, and background task:

| Field | Clone cost |
|-------|------------|
| `mailboxes: Arc<[Sender<Command>]>` | `Arc::clone` — one atomic increment, all clones share senders |
| `Arc<AtomicUsize>` counters | `Arc::clone` |
| `mailbox_capacity: usize` | `Copy` |
| Other `Arc<_>` handles | `Arc::clone` |

No subsystem wraps itself in an outer `Arc`. Cloning is the idiom, and
because every internal field is already shared-by-Arc, it's always O(1).

## Concrete Subsystems

Three subsystems use this pattern today. They differ in sharding key, state
shape, and command set, but not in structure.

### c2s::Router

**Purpose:** Route messages and track client→connection bindings for C2S
traffic.

| Aspect | Value |
|--------|-------|
| Sharding key | `username` (`StringAtom`) |
| Shard state | `HashMap<StringAtom, Vec<Entry>>` where `Entry = { handler: usize, transmitter: Arc<dyn Transmitter> }` |
| Commands | `RouteTo`, `Register`, `Unregister`, `HasConnection` |
| Extra shared state | `Arc<AtomicUsize>` total-connection counter (accessed from all shards) |
| Fire-and-forget ops | `RouteTo` |
| Reply-bearing ops | `Register` (→ `bool`), `Unregister` (→ `bool`), `HasConnection` (→ `bool`) |

`Register`'s `bool` reply carries the "exclusive login succeeded" semantics:
if `exclusive: true` and the user already has connections, the shard
returns `false` without mutating state.

`Unregister`'s `bool` reply means "this was the last connection for the
user" — useful to the caller for cleanup sequencing. The shard's
per-username `Vec<Entry>` can hold multiple connections (e.g. multi-device
sessions); only emptying it counts.

### ChannelManager

**Purpose:** Own the set of live channels, route join/leave/config/broadcast
commands, drive message-log appends.

| Aspect | Value |
|--------|-------|
| Sharding key | `channel handler` (`StringAtom`) |
| Shard state | `HashMap<StringAtom, Channel<MLF::Log>>` — the full per-channel in-memory state (owner, ACL, members, message log, flush task) |
| Commands | `JoinChannel`, `LeaveChannel`, `LeaveChannels`, `DeleteChannel`, `BroadcastPayload`, config/ACL ops, `History`, `ChannelSeq` |
| Extra shared state | `Arc<AtomicUsize>` total-channels counter; `Membership` handle (separately sharded) |
| Factories | `ChannelStore` (metadata persistence) + `MessageLogFactory` (log persistence), both `Clone`-d into each shard |

Every reply-bearing command returns `anyhow::Result<T>` so business-logic
errors (`ChannelNotFound`, `Forbidden`, `PolicyViolation`, …) flow back to
the caller on the reply channel. Shard actors never panic on bad input —
they serialize the error into `reply_tx` and keep looping.

`ChannelShard::restore_and_run(hashes)` wraps the standard loop with a
pre-loop restore phase that re-hydrates persisted channels from disk (see
[CHANNEL_STORE.md](CHANNEL_STORE.md)) and a post-loop shutdown phase that
cancels periodic flush tasks and performs a final flush on each persistent
channel before dropping.

### Membership

**Purpose:** Track which channels a given user is subscribed to across
**all** channel shards, to enforce `max_channels_per_client` and to drive
disconnect cleanup.

| Aspect | Value |
|--------|-------|
| Sharding key | `username` (`StringAtom`) |
| Shard state | `HashMap<StringAtom, HashSet<StringAtom>>` — user → set of channel handlers |
| Commands | `ReserveSlot` (→ `bool`), `ReleaseSlot`, `GetChannels` (→ `Arc<[StringAtom]>`) |
| Callers | Channel shards (from `join_channel` / `do_leave`) and connection cleanup paths |

`Membership` exists because `ChannelManager` is sharded by **channel**, not
by user. A client that subscribes to channels across multiple shards leaves
no single `ChannelShard` with a complete view of its subscriptions — so the
per-client subscription count can't be enforced inside a channel shard, and
"find every channel a user is in" would require an all-shard fan-out.

`Membership` solves both with a **second, orthogonally-sharded actor layer**.
A join flows through two actors:

```
Caller (connection dispatcher)
  │
  ├─ ChannelManager::join_channel(nid, handler, ..)
  │      │
  │      ▼
  │  ChannelShard [shard_by_channel(handler)]
  │      │
  │      ├─ Validate ACL, members, channel limits.
  │      │
  │      ├─ Membership::reserve_slot(username, handler, max_per_client)
  │      │      │
  │      │      ▼
  │      │  MembershipShard [shard_by_user(username)]
  │      │      │
  │      │      ├─ If set.len() >= max_per_client → reply false.
  │      │      └─ Else insert handler, reply true.
  │      │      ◄─────────────────────────────────── reply
  │      │
  │      ├─ Persist projected membership to ChannelStore.
  │      ├─ Mutate in-memory channel state.
  │      └─ Send JOIN_ACK to client.
```

If Membership rejects the reservation, the channel shard returns
`PolicyViolation` without mutating state. If a later step fails (persist,
notify), the shard calls `Membership::release_slot` to undo the reservation.

`Membership::get_channels(username)` is how connection teardown drives
channel cleanup: on disconnect, the C2S dispatcher gets the user's channels,
then sends a `LeaveChannels` command per owning shard to clean them up in
parallel.

This pattern generalizes: whenever a cross-shard question is `O(1)` per
user but `O(shards)` for the naïve fan-out, a second actor keyed on the
complementary dimension makes the question single-hop again.

## System-Wide Shutdown Order

Shutdown must run in this order (from
`crates/server/src/lib.rs:177-188`):

```
1. c2s_ln.shutdown().await              — stop accepting new connections
2. modulator_service.shutdown().await   — drain modulator traffic
3. route_m2s_payload_handle.close/await — stop m2s private-payload routing task
4. channel_mng.shutdown()               — close ChannelManager + Membership mailboxes
5. c2s_router.shutdown()                — close Router mailboxes
6. core_dispatcher.shutdown().await     — close task channels, signal workers, join threads
```

Rationale:
- Shutting down the listener first stops the producer side: no new commands
  are enqueued after step 1.
- `channel_mng.shutdown()` internally calls `membership.shutdown()`, so both
  layers drain together.
- Subsystem shutdowns must happen **before** the dispatcher's, so their
  shard loops observe "mailbox closed" and exit through the normal `Ok`
  path. If the dispatcher went first, shard futures would be cancelled mid-
  command, skipping shutdown hooks (most notably, `ChannelShard`'s final
  `flush()` on every persistent channel).
- `core_dispatcher.shutdown()` then closes the underlying task channels and
  signals each worker, letting `try_block_on` return and every thread `join`.

## Concurrency Model

- Each shard actor runs on exactly one worker thread. All state it owns is
  accessed only from that thread — `HashMap<Key, State>` with `&mut self`.
- Shared state across shards is `Arc`-wrapped and carefully chosen:
  `AtomicUsize` for counters, another sharded actor (`Membership`) for
  user-indexed queries. There is no shared `Mutex`/`RwLock` on the hot path.
- Callers are producer-only with respect to a shard: they `send(cmd).await`
  on a `Sender<Command>` and `recv().await` on a one-shot reply. Neither end
  holds a lock across the await.
- `async_channel` is MPMC, but each mailbox has exactly one consumer (its
  shard actor). The multi-sender side is what every caller uses.
- `dispatch_at_shard` ensures the shard actor is spawned on the correct
  worker. Its type signature — `F: FnOnce() -> Fut + Send + 'static` with
  `Fut: Future + 'static` (no `Send` bound on the future) — means the
  closure is shipped to the worker, but the future it returns is
  constructed and spawned there. The actor's state never crosses a thread
  boundary.

## Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `DEFAULT_MAILBOX_CAPACITY` | 16384 | Per-shard bounded mailbox capacity (every subsystem) |
| `NARWHAL_NUM_WORKERS` | env var | Overrides `available_parallelism()` for the dispatcher's worker count |
| Reply channel capacity | 1 | `async_channel::bounded(1)` for every request/reply |
| Task channel per worker | unbounded | `async_channel::unbounded::<SpawnFn>()` inside `CoreDispatcher` |
| Shutdown channel per worker | 1 | `async_channel::bounded::<()>(1)` |

`DEFAULT_MAILBOX_CAPACITY` is duplicated in each subsystem (router, channel
manager, membership) — a deliberate choice so each subsystem can be tuned
independently if traffic patterns diverge.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `async_channel` | MPMC channels for mailboxes, reply channels, task channels, shutdown signals |
| `core_affinity` | Best-effort CPU-core pinning for worker threads |
| `libc` | `pthread_sigmask` + signal constants to block signals on worker threads |
| `compio` | io_uring async runtime per worker (accessed via `narwhal_common::runtime`) |
| `std::hash::DefaultHasher` | Shard-key hashing (SipHash-1-3 today) |

## Testing

- `c2s::Router` unit tests (`crates/server/src/c2s/router.rs`) spawn a
  single `RouterShard` directly on the current runtime, bypassing
  `CoreDispatcher`. This keeps tests deterministic and single-threaded:
  the same structural pattern, no thread pool required.
- `ChannelManager` and `Membership` are exercised through integration tests
  that stand up a real `CoreDispatcher` (see `crates/server/tests/` and
  `crates/test-util/src/c2s_suite.rs`).
- `shard_for` is covered with a determinism test (`shard_assignment_is_
  deterministic`) to guarantee that rehashing the same key gives the same
  shard across calls.

## File Locations

| File | Purpose |
|------|---------|
| `crates/common/src/core_dispatcher.rs` | `CoreDispatcher` — thread pool + compio runtimes |
| `crates/common/src/runtime.rs` | `try_block_on`, `spawn_detached` — compio runtime shims |
| `crates/server/src/c2s/router.rs` | `Router` + `RouterShard` (sharded by username) |
| `crates/server/src/channel/manager.rs` | `ChannelManager` + `ChannelShard` (sharded by channel) |
| `crates/server/src/channel/membership.rs` | `Membership` + `MembershipShard` (sharded by username) |
| `crates/server/src/lib.rs` | `num_workers()`, bootstrap wiring, system-wide shutdown order |
