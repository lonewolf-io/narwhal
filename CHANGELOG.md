# Changelog

All notable changes to Narwhal will be documented in this file.

## Unreleased

* [ENHANCEMENT]: Extract thread and runtime management into a shared `CoreDispatcher`. [#172](https://github.com/narwhal-io/narwhal/pull/172)

## 0.5.0 (2026-03-02)

* [CHANGE]: Migrate primary async runtime from tokio to monoio (io_uring). [#150](https://github.com/narwhal-io/narwhal/pull/150), [#165](https://github.com/narwhal-io/narwhal/pull/165)
* [CHANGE]: Add `seq` and `timestamp` fields to `MESSAGE` and `BROADCAST_ACK`. Each delivered message now carries a per-channel monotonic sequence number and a server-assigned UTC millisecond timestamp. [#166](https://github.com/narwhal-io/narwhal/pull/166)
* [ENHANCEMENT]: Add `DELETE` command for channel deletion. [#160](https://github.com/narwhal-io/narwhal/pull/160),[#161](https://github.com/narwhal-io/narwhal/pull/161)

## 0.4.0 (2026-01-27)

* [CHANGE]: Support alphanumeric channel identifiers. [#105](https://github.com/narwhal-io/narwhal/pull/105)
* [CHANGE]: Add `ResourceConflict` error for concurrent modification scenarios. [#109](https://github.com/narwhal-io/narwhal/pull/109)
* [CHANGE]: Refactor ACL commands to support granular type-based operations. [#115](https://github.com/narwhal-io/narwhal/pull/115)
* [CHANGE]: Add dedicated ACK messages for `SET_CHAN_ACL` and `SET_CHAN_CONFIG` operations. [#116](https://github.com/narwhal-io/narwhal/pull/116)
* [CHANGE]: Migrate to runtime-agnostic primitives to enable future support for alternative async runtimes. [#140](https://github.com/narwhal-io/narwhal/pull/140), [#141](https://github.com/narwhal-io/narwhal/pull/141)
* [ENHANCEMENT]: Add kernel TLS (kTLS) support for TLS encryption offloading on Linux. [#136](https://github.com/narwhal-io/narwhal/pull/136)
* [ENHANCEMENT]: Distribute connections across CPU-pinned worker threads for improved performance. [#127](https://github.com/narwhal-io/narwhal/pull/127)
* [ENHANCEMENT]: Use LocalSet for single-threaded connection worker tasks to reduce task scheduler overhead. [#128](https://github.com/narwhal-io/narwhal/pull/128)
* [ENHANCEMENT]: Add pagination support to `CHANNELS`/`CHANNELS_ACK` messages. [#110](https://github.com/narwhal-io/narwhal/pull/110), [#112](https://github.com/narwhal-io/narwhal/pull/112)
* [ENHANCEMENT]: Add pagination support to `MEMBERS`/`MEMBERS_ACK` messages. [#113](https://github.com/narwhal-io/narwhal/pull/113)
* [ENHANCEMENT]: Add pagination support to `GET_CHAN_ACL`/`CHAN_ACL` messages. [#117](https://github.com/narwhal-io/narwhal/pull/117)
* [ENHANCEMENT]: Add `RESPONSE_TOO_LARGE` error for graceful handling of oversized server responses. [#118](https://github.com/narwhal-io/narwhal/pull/118)
* [ENHANCEMENT]: Reduce lock contention in channel join and configuration operations. [#108](https://github.com/narwhal-io/narwhal/pull/108)

## 0.3.0 (2025-12-12) 🎄

* [CHANGE]: Expand correlation IDs from `u16` to `u32` across protocol. [#60](https://github.com/narwhal-io/narwhal/pull/60)
* [ENHANCEMENT]: Cache allowed broadcast targets to reduce lock contention during message broadcasting. [#68](https://github.com/narwhal-io/narwhal/pull/68)
* [ENHANCEMENT]: Replace manual sharding with DashMap in c2s router. [#69](https://github.com/narwhal-io/narwhal/pull/69)
* [ENHANCEMENT]: Move high-frequency operation logs from info to trace level. [#72](https://github.com/narwhal-io/narwhal/pull/72)
* [ENHANCEMENT]: Pin cancellation token futures in select loops to reduce lock contention. [#75](https://github.com/narwhal-io/narwhal/pull/75)
* [ENHANCEMENT]: Migrate buffer pool to lock-free async implementation. [#78](https://github.com/narwhal-io/narwhal/pull/78)
* [ENHANCEMENT]: Simplify keep-alive with activity-based ping loop to eliminate race conditions. [#82](https://github.com/narwhal-io/narwhal/pull/82)
* [ENHANCEMENT]: Refactor write batch handling for efficient pool buffer release. [#85](https://github.com/narwhal-io/narwhal/pull/85)
* [ENHANCEMENT]: Improve bucketed pool acquire to block on smallest suitable bucket when buffers unavailable. [#86](https://github.com/narwhal-io/narwhal/pull/86)
* [BUGFIX]: Ensure the connection is always gracefully shut down. [#65](https://github.com/narwhal-io/narwhal/pull/65)
* [BUGFIX]: Only leave channels when user's last connection closes. [#70](https://github.com/narwhal-io/narwhal/pull/70)

## 0.2.0 (2025-07-26)

* [CHANGE]: Optimize writes with vectored I/O implementation. [#39](https://github.com/narwhal-io/narwhal/pull/39)
* [ENHANCEMENT]: Dynamic fd limits based on config. [#37](https://github.com/narwhal-io/narwhal/pull/37)
* [ENHANCEMENT]: Add flush batching support for improved connection throughput. [#45](https://github.com/narwhal-io/narwhal/pull/45)
* [BUGFIX]: Correct rate limit checking in connection handling. [#35](https://github.com/narwhal-io/narwhal/pull/35)
