// SPDX-License-Identifier: BSD-3-Clause

pub mod backoff;
pub mod codec_compio;
pub mod codec_tokio;
pub mod io;
pub mod pool;
pub mod string_atom;

/// Creates a new runtime and blocks on the given future.
///
/// Useful for tests that need a runtime on a spawned thread.
pub fn runtime_block_on<F: std::future::Future>(future: F) -> F::Output {
  let rt = compio::runtime::Runtime::new().unwrap();
  rt.block_on(future)
}
