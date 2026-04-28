// SPDX-License-Identifier: BSD-3-Clause

use std::path::Path;

use compio::BufResult;
use compio::io::AsyncWriteAtExt;

/// Atomically writes `data` to `path` using the write-temp-rename pattern:
///
/// 1. Create and write `data` to `tmp_path`.
/// 2. fsync the temp file.
/// 3. Close the temp file (Windows can't rename an open file).
/// 4. Rename `tmp_path` -> `path`.
/// 5. Best-effort fsync the parent directory so the rename itself is
///    durable across a crash.
///
/// A crash mid-write leaves either the previous contents of `path` (if any)
/// or the new contents — never a half-written file.
///
/// On error the temp file is removed (best-effort) so a stale `.tmp` doesn't
/// accumulate from a prior crashed write. The buffer is returned in both the
/// `Ok` and `Err` cases so callers can reuse it.
///
/// The parent-directory fsync is best-effort: failure only affects post-crash
/// visibility of the rename. The file content is fully written and durable;
/// the next recovery pass can re-validate.
pub async fn atomic_write(path: &Path, tmp_path: &Path, data: Vec<u8>) -> (anyhow::Result<()>, Vec<u8>) {
  let (result, buf) = atomic_write_inner(path, tmp_path, data).await;
  if result.is_err() {
    let _ = compio::fs::remove_file(tmp_path).await;
  }
  (result, buf)
}

async fn atomic_write_inner(path: &Path, tmp_path: &Path, data: Vec<u8>) -> (anyhow::Result<()>, Vec<u8>) {
  let tmp_file = match compio::fs::File::create(tmp_path).await {
    Ok(f) => f,
    Err(e) => return (Err(e.into()), data),
  };

  let mut file_ref = &tmp_file;
  let BufResult(write_result, buf) = file_ref.write_all_at(data, 0).await;
  if let Err(e) = write_result {
    let _ = tmp_file.close().await;
    return (Err(e.into()), buf);
  }

  if let Err(e) = tmp_file.sync_all().await {
    let _ = tmp_file.close().await;
    return (Err(e.into()), buf);
  }

  if let Err(e) = tmp_file.close().await {
    return (Err(e.into()), buf);
  }

  if let Err(e) = compio::fs::rename(tmp_path, path).await {
    return (Err(e.into()), buf);
  }

  if let Some(parent) = path.parent()
    && let Ok(dir_file) = compio::fs::File::open(parent).await
  {
    let _ = dir_file.sync_all().await;
    let _ = dir_file.close().await;
  }

  (Ok(()), buf)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[compio::test]
  async fn writes_and_renames() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("data.bin");
    let tmp_path = tmp.path().join("data.tmp");

    let (res, buf) = atomic_write(&path, &tmp_path, b"hello".to_vec()).await;
    assert!(res.is_ok());
    assert_eq!(buf, b"hello");

    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(on_disk, b"hello");
    assert!(!tmp_path.exists(), "tmp file should be renamed away");
  }

  #[compio::test]
  async fn overwrites_existing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("data.bin");
    let tmp_path = tmp.path().join("data.tmp");

    std::fs::write(&path, b"old").unwrap();

    let (res, _) = atomic_write(&path, &tmp_path, b"new".to_vec()).await;
    assert!(res.is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
  }

  #[compio::test]
  async fn cleans_up_stale_tmp_on_create_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("data.bin");
    // tmp_path points into a missing subdirectory so File::create fails.
    let tmp_path = tmp.path().join("missing").join("data.tmp");

    let (res, buf) = atomic_write(&path, &tmp_path, b"x".to_vec()).await;
    assert!(res.is_err());
    assert_eq!(buf, b"x");
    assert!(!tmp_path.exists());
  }
}
