# Changelog

All notable changes to Narwhal will be documented in this file.

## Unreleased

## 0.6.1 (2026-05-01)

* [CHANGE]: Replace the multi-value `operations` string list on `S2M_CONNECT_ACK` with a single `operation_mask` u64 bitmask. Bit positions are now the authoritative wire contract, frozen in `PROTOCOL.md`'s new `Operation Bit Assignments` section. Breaking wire change with no compatibility shim. [#283](https://github.com/lonewolf-io/narwhal/pull/283)
* [ENHANCEMENT]: Surface sealed message-log segments whose recovery validation scan terminates before EOF (CRC mismatch, unreadable header, oversized declared lengths, truncated tail, or I/O error) via a new `message_log_sealed_segment_truncations` counter and a `tracing::warn!`, so silent data loss past the validated region shows up as telemetry instead of being discovered later via missing history. [#269](https://github.com/lonewolf-io/narwhal/pull/269)
* [BUGFIX]: Stop unconditionally forwarding broadcast payloads to the attached modulator. The C2S dispatcher now caches the modulator's declared `Operations` once at startup and gates the forward call on `Operation::ForwardBroadcastPayload`. [#275](https://github.com/lonewolf-io/narwhal/pull/275)
* [BUGFIX]: Route per-channel periodic flushes through the channel actor's mailbox instead of letting a sibling task call `FileMessageLog::flush` directly, fixing a `RefCell already borrowed` panic that fired when a persistent channel with `message_flush_interval > 0` saw a flush coincide with an in-flight append on the same shard. [#277](https://github.com/lonewolf-io/narwhal/pull/277)
* [BUGFIX]: Reject `seq=0` and non-monotonic `seq` in `FileMessageLog::append` to prevent caller-contract violations from silently corrupting the log's sentinel-based seq tracking and the sparse index's monotonicity invariant. [#268](https://github.com/lonewolf-io/narwhal/pull/268)
* [BUGFIX]: Make `bytes_since_index` recovery match the runtime invariant (`file_size - last_index_offset`), eliminating an off-by-one drift that delayed the next index entry by up to one entry's worth of bytes after every restart. [#267](https://github.com/lonewolf-io/narwhal/pull/267)
* [BUGFIX]: Make sealed-index rebuild atomic via write-temp-rename so a mid-rebuild crash never leaves a partial `.idx` on disk. [#263](https://github.com/lonewolf-io/narwhal/pull/263)
* [BUGFIX]: Tighten the sealed-index last-offset validation to require room for a full entry header, and document the empty-active-segment branch of recovery. [#262](https://github.com/lonewolf-io/narwhal/pull/262)
* [BUGFIX]: Read path no longer falls back to the active segment's index when a sealed segment has no mmap; recovery propagates fatal mmap/open errors so the affected channel refuses to come online instead of silently dropping index updates. [#261](https://github.com/lonewolf-io/narwhal/pull/261)
* [BUGFIX]: Validate sealed message-log index files at recovery time and rebuild them when visibly corrupt, preventing silent gaps in `HISTORY` reads. [#255](https://github.com/lonewolf-io/narwhal/pull/255)
* [BUGFIX]: Don't promote a sealed segment to "active" during message-log recovery when the on-disk active segment validates to zero entries. [#254](https://github.com/lonewolf-io/narwhal/pull/254)
* [BUGFIX]: Reject oversized `from`/payload in message-log append to prevent silent segment-tail truncation on recovery. [#250](https://github.com/lonewolf-io/narwhal/pull/250)
* [BUGFIX]: Avoid blocking shard runtimes on message-log index flush by replacing synchronous `mmap.flush()` with async `.idx` file `sync_all()`. [#249](https://github.com/lonewolf-io/narwhal/pull/249)

## 0.6.0 (2026-04-20)

* [CHANGE]: Migrate async runtime from monoio to compio (io_uring). [#212](https://github.com/lonewolf-io/narwhal/pull/212)
* [ENHANCEMENT]: Add channel persistence support. [#184](https://github.com/lonewolf-io/narwhal/pull/184), [#185](https://github.com/lonewolf-io/narwhal/pull/185), [#191](https://github.com/lonewolf-io/narwhal/pull/191), [#192](https://github.com/lonewolf-io/narwhal/pull/192), [#193](https://github.com/lonewolf-io/narwhal/pull/193), [#198](https://github.com/lonewolf-io/narwhal/pull/198), [#199](https://github.com/lonewolf-io/narwhal/pull/199), [#201](https://github.com/lonewolf-io/narwhal/pull/201), [#204](https://github.com/lonewolf-io/narwhal/pull/204), [#205](https://github.com/lonewolf-io/narwhal/pull/205), [#208](https://github.com/lonewolf-io/narwhal/pull/208), [#217](https://github.com/lonewolf-io/narwhal/pull/217), [#221](https://github.com/lonewolf-io/narwhal/pull/221), [#222](https://github.com/lonewolf-io/narwhal/pull/222), [#227](https://github.com/lonewolf-io/narwhal/pull/227)
* [ENHANCEMENT]: Abstract runtime behind `Runtime` trait for future multi-runtime support. [#210](https://github.com/lonewolf-io/narwhal/pull/210)
* [ENHANCEMENT]: Add Prometheus metrics support. [#175](https://github.com/lonewolf-io/narwhal/pull/175), [#234](https://github.com/lonewolf-io/narwhal/pull/234)
* [ENHANCEMENT]: Extract thread and runtime management into a shared `CoreDispatcher`. [#172](https://github.com/lonewolf-io/narwhal/pull/172)
* [ENHANCEMENT]: Allow `/` character in channel handler names for hierarchical naming. [#220](https://github.com/lonewolf-io/narwhal/pull/220)

## 0.5.0 (2026-03-02)

* [CHANGE]: Migrate primary async runtime from tokio to monoio (io_uring). [#150](https://github.com/lonewolf-io/narwhal/pull/150), [#165](https://github.com/lonewolf-io/narwhal/pull/165)
* [CHANGE]: Add `seq` and `timestamp` fields to `MESSAGE` and `BROADCAST_ACK`. Each delivered message now carries a per-channel monotonic sequence number and a server-assigned UTC millisecond timestamp. [#166](https://github.com/lonewolf-io/narwhal/pull/166)
* [ENHANCEMENT]: Add `DELETE` command for channel deletion. [#160](https://github.com/lonewolf-io/narwhal/pull/160), [#161](https://github.com/lonewolf-io/narwhal/pull/161)

## 0.4.0 (2026-01-27)

* [CHANGE]: Support alphanumeric channel identifiers. [#105](https://github.com/lonewolf-io/narwhal/pull/105)
* [CHANGE]: Add `ResourceConflict` error for concurrent modification scenarios. [#109](https://github.com/lonewolf-io/narwhal/pull/109)
* [CHANGE]: Refactor ACL commands to support granular type-based operations. [#115](https://github.com/lonewolf-io/narwhal/pull/115)
* [CHANGE]: Add dedicated ACK messages for `SET_CHAN_ACL` and `SET_CHAN_CONFIG` operations. [#116](https://github.com/lonewolf-io/narwhal/pull/116)
* [CHANGE]: Migrate to runtime-agnostic primitives to enable future support for alternative async runtimes. [#140](https://github.com/lonewolf-io/narwhal/pull/140), [#141](https://github.com/lonewolf-io/narwhal/pull/141)
* [ENHANCEMENT]: Add kernel TLS (kTLS) support for TLS encryption offloading on Linux. [#136](https://github.com/lonewolf-io/narwhal/pull/136)
* [ENHANCEMENT]: Distribute connections across CPU-pinned worker threads for improved performance. [#127](https://github.com/lonewolf-io/narwhal/pull/127)
* [ENHANCEMENT]: Use LocalSet for single-threaded connection worker tasks to reduce task scheduler overhead. [#128](https://github.com/lonewolf-io/narwhal/pull/128)
* [ENHANCEMENT]: Add pagination support to `CHANNELS`/`CHANNELS_ACK` messages. [#110](https://github.com/lonewolf-io/narwhal/pull/110), [#112](https://github.com/lonewolf-io/narwhal/pull/112)
* [ENHANCEMENT]: Add pagination support to `MEMBERS`/`MEMBERS_ACK` messages. [#113](https://github.com/lonewolf-io/narwhal/pull/113)
* [ENHANCEMENT]: Add pagination support to `GET_CHAN_ACL`/`CHAN_ACL` messages. [#117](https://github.com/lonewolf-io/narwhal/pull/117)
* [ENHANCEMENT]: Add `RESPONSE_TOO_LARGE` error for graceful handling of oversized server responses. [#118](https://github.com/lonewolf-io/narwhal/pull/118)
* [ENHANCEMENT]: Reduce lock contention in channel join and configuration operations. [#108](https://github.com/lonewolf-io/narwhal/pull/108)

## 0.3.0 (2025-12-12) 🎄

* [CHANGE]: Expand correlation IDs from `u16` to `u32` across protocol. [#60](https://github.com/lonewolf-io/narwhal/pull/60)
* [ENHANCEMENT]: Cache allowed broadcast targets to reduce lock contention during message broadcasting. [#68](https://github.com/lonewolf-io/narwhal/pull/68)
* [ENHANCEMENT]: Replace manual sharding with DashMap in c2s router. [#69](https://github.com/lonewolf-io/narwhal/pull/69)
* [ENHANCEMENT]: Move high-frequency operation logs from info to trace level. [#72](https://github.com/lonewolf-io/narwhal/pull/72)
* [ENHANCEMENT]: Pin cancellation token futures in select loops to reduce lock contention. [#75](https://github.com/lonewolf-io/narwhal/pull/75)
* [ENHANCEMENT]: Migrate buffer pool to lock-free async implementation. [#78](https://github.com/lonewolf-io/narwhal/pull/78)
* [ENHANCEMENT]: Simplify keep-alive with activity-based ping loop to eliminate race conditions. [#82](https://github.com/lonewolf-io/narwhal/pull/82)
* [ENHANCEMENT]: Refactor write batch handling for efficient pool buffer release. [#85](https://github.com/lonewolf-io/narwhal/pull/85)
* [ENHANCEMENT]: Improve bucketed pool acquire to block on smallest suitable bucket when buffers unavailable. [#86](https://github.com/lonewolf-io/narwhal/pull/86)
* [BUGFIX]: Ensure the connection is always gracefully shut down. [#65](https://github.com/lonewolf-io/narwhal/pull/65)
* [BUGFIX]: Only leave channels when user's last connection closes. [#70](https://github.com/lonewolf-io/narwhal/pull/70)

## 0.2.0 (2025-07-26)

* [CHANGE]: Optimize writes with vectored I/O implementation. [#39](https://github.com/lonewolf-io/narwhal/pull/39)
* [ENHANCEMENT]: Dynamic fd limits based on config. [#37](https://github.com/lonewolf-io/narwhal/pull/37)
* [ENHANCEMENT]: Add flush batching support for improved connection throughput. [#45](https://github.com/lonewolf-io/narwhal/pull/45)
* [BUGFIX]: Correct rate limit checking in connection handling. [#35](https://github.com/lonewolf-io/narwhal/pull/35)
