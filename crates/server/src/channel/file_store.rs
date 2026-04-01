// SPDX-License-Identifier: BSD-3-Clause

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use narwhal_util::string_atom::StringAtom;

use super::store::{ChannelStore, PersistedChannel};

const METADATA_FILE: &str = "metadata.bin";
const METADATA_TMP_FILE: &str = "metadata.bin.tmp";
const MAX_METADATA_SIZE: usize = 64 * 1024 * 1024; // 64 MiB

/// Computes the SHA-256 hex-encoded hash of a channel handler, used as the directory name.
fn channel_hash(handler: &StringAtom) -> StringAtom {
  let digest = Sha256::digest(handler.as_ref().as_bytes());
  let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
  StringAtom::from(hex)
}

/// A file-based `ChannelStore` implementation.
#[derive(Clone)]
pub struct FileChannelStore {
  data_dir: Arc<PathBuf>,
}

// === impl FileChannelStore ===

impl FileChannelStore {
  /// Creates a new `FileChannelStore` rooted at the given data directory.
  /// Creates the directory if it does not exist.
  pub async fn new(data_dir: PathBuf) -> anyhow::Result<Self> {
    compio::fs::create_dir_all(&data_dir).await?;
    Ok(Self { data_dir: Arc::new(data_dir) })
  }

  fn channel_dir(&self, hash: &str) -> PathBuf {
    self.data_dir.join(hash)
  }

  /// Atomically writes `data` to `file_path` using write-tmp-rename-fsync.
  async fn atomic_write(&self, dir: &Path, data: &[u8]) -> anyhow::Result<()> {
    let tmp_path = dir.join(METADATA_TMP_FILE);
    let final_path = dir.join(METADATA_FILE);

    // Write to temp file.
    let buf = data.to_vec();
    let compio::BufResult(result, _) = compio::fs::write(&tmp_path, buf).await;
    result?;

    // Fsync the temp file.
    let file = compio::fs::File::open(&tmp_path).await?;
    file.sync_all().await?;
    file.close().await?;

    // Rename temp → final.
    compio::fs::rename(&tmp_path, &final_path).await?;

    // Fsync the parent directory.
    let dir_file = compio::fs::File::open(dir).await?;
    dir_file.sync_all().await?;
    dir_file.close().await?;

    Ok(())
  }
}

#[async_trait(?Send)]
impl ChannelStore for FileChannelStore {
  async fn save_channel(&self, channel: &PersistedChannel) -> anyhow::Result<StringAtom> {
    let hash = channel_hash(&channel.handler);
    let dir = self.channel_dir(hash.as_ref());

    compio::fs::create_dir_all(&dir).await?;

    let data = bincode::serde::encode_to_vec(channel, bincode::config::standard())?;
    self.atomic_write(&dir, &data).await?;
    Ok(hash)
  }

  async fn delete_channel(&self, hash: &StringAtom) -> anyhow::Result<()> {
    let dir = self.channel_dir(hash.as_ref());
    // Remove metadata files, then the directory. Treat NotFound as success.
    let metadata_path = dir.join(METADATA_FILE);
    if let Err(e) = compio::fs::remove_file(&metadata_path).await
      && e.kind() != std::io::ErrorKind::NotFound
    {
      return Err(e.into());
    }
    let tmp_path = dir.join(METADATA_TMP_FILE);
    if let Err(e) = compio::fs::remove_file(&tmp_path).await
      && e.kind() != std::io::ErrorKind::NotFound
    {
      return Err(e.into());
    }
    if let Err(e) = compio::fs::remove_dir(&dir).await
      && e.kind() != std::io::ErrorKind::NotFound
    {
      return Err(e.into());
    }

    Ok(())
  }

  async fn load_channel_hashes(&self) -> anyhow::Result<Arc<[StringAtom]>> {
    let data_dir = self.data_dir.as_ref().clone();

    let entries = match std::fs::read_dir(&data_dir) {
      Ok(entries) => entries,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Arc::from([])),
      Err(e) => return Err(e.into()),
    };

    let mut hashes = Vec::new();
    for entry in entries {
      let entry = entry?;
      if entry.file_type()?.is_dir() {
        let dir_path = entry.path();
        if !dir_path.join(METADATA_FILE).exists() {
          tracing::warn!(path = %dir_path.display(), "removing stale channel directory (missing metadata.bin)");
          if let Ok(inner) = std::fs::read_dir(&dir_path) {
            for file in inner.flatten() {
              let _ = compio::fs::remove_file(file.path()).await;
            }
          }
          if let Err(e) = compio::fs::remove_dir(&dir_path).await {
            tracing::warn!(path = %dir_path.display(), error = %e, "failed to remove stale channel directory");
          }
          continue;
        }
        if let Some(name) = entry.file_name().to_str() {
          hashes.push(StringAtom::from(name));
        }
      }
    }
    Ok(hashes.into())
  }

  async fn load_channel(&self, hash: &StringAtom) -> anyhow::Result<PersistedChannel> {
    let path = self.channel_dir(hash.as_ref()).join(METADATA_FILE);
    let meta = compio::fs::metadata(&path).await?;
    if meta.len() > MAX_METADATA_SIZE as u64 {
      return Err(anyhow::anyhow!("metadata file exceeds {} bytes", MAX_METADATA_SIZE));
    }
    let data = compio::fs::read(&path).await?;
    let (channel, _) = bincode::serde::decode_from_slice(&data, bincode::config::standard())?;
    Ok(channel)
  }
}

#[cfg(test)]
mod tests {
  use std::rc::Rc;

  use narwhal_protocol::Nid;
  use narwhal_util::string_atom::StringAtom;

  use super::*;
  use crate::channel::{ChannelAcl, ChannelConfig};

  fn test_channel(handler: &str) -> PersistedChannel {
    PersistedChannel {
      handler: StringAtom::from(handler),
      owner: Some(Nid::new_unchecked(StringAtom::from("alice"), StringAtom::from("localhost"))),
      config: ChannelConfig {
        persist: Some(true),
        max_clients: Some(50),
        max_payload_size: Some(1024),
        max_persist_messages: Some(100),
        message_flush_interval: Some(0),
      },
      acl: ChannelAcl::default(),
      members: Rc::from(vec![
        Nid::new_unchecked(StringAtom::from("alice"), StringAtom::from("localhost")),
        Nid::new_unchecked(StringAtom::from("bob"), StringAtom::from("localhost")),
      ]),
    }
  }

  #[compio::test]
  async fn test_save_and_load_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileChannelStore::new(tmp.path().to_path_buf()).await.unwrap();

    let channel = test_channel("mychannel");
    store.save_channel(&channel).await.unwrap();

    let hash = channel_hash(&StringAtom::from("mychannel"));
    let loaded = store.load_channel(&hash).await.unwrap();

    assert_eq!(loaded.handler.as_ref(), "mychannel");
    assert_eq!(loaded.owner, channel.owner);
    assert_eq!(loaded.config.persist, Some(true));
    assert_eq!(loaded.config.max_clients, Some(50));
    assert_eq!(loaded.members.len(), 2);
  }

  #[compio::test]
  async fn test_load_channel_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileChannelStore::new(tmp.path().to_path_buf()).await.unwrap();

    store.save_channel(&test_channel("channel1")).await.unwrap();
    store.save_channel(&test_channel("channel2")).await.unwrap();

    let hashes = store.load_channel_hashes().await.unwrap();
    assert_eq!(hashes.len(), 2);

    let hash1 = channel_hash(&StringAtom::from("channel1"));
    let hash2 = channel_hash(&StringAtom::from("channel2"));
    assert!(hashes.contains(&hash1));
    assert!(hashes.contains(&hash2));
  }

  #[compio::test]
  async fn test_delete_channel() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileChannelStore::new(tmp.path().to_path_buf()).await.unwrap();

    store.save_channel(&test_channel("mychannel")).await.unwrap();
    let hash = channel_hash(&StringAtom::from("mychannel"));

    store.delete_channel(&hash).await.unwrap();

    let hashes = store.load_channel_hashes().await.unwrap();
    assert_eq!(hashes.len(), 0);
  }

  #[compio::test]
  async fn test_delete_nonexistent_channel() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileChannelStore::new(tmp.path().to_path_buf()).await.unwrap();

    let hash = channel_hash(&StringAtom::from("nonexistent"));
    // Should not error.
    store.delete_channel(&hash).await.unwrap();
  }

  #[compio::test]
  async fn test_save_overwrites_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileChannelStore::new(tmp.path().to_path_buf()).await.unwrap();

    let mut channel = test_channel("mychannel");
    store.save_channel(&channel).await.unwrap();

    channel.config.max_clients = Some(200);
    store.save_channel(&channel).await.unwrap();

    let hash = channel_hash(&StringAtom::from("mychannel"));
    let loaded = store.load_channel(&hash).await.unwrap();
    assert_eq!(loaded.config.max_clients, Some(200));
  }

  #[compio::test]
  async fn test_load_corrupted_file_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileChannelStore::new(tmp.path().to_path_buf()).await.unwrap();

    let hash = channel_hash(&StringAtom::from("bad"));
    let dir = tmp.path().join(hash.as_ref());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(METADATA_FILE), b"not valid bincode").unwrap();

    let result = store.load_channel(&hash).await;
    assert!(result.is_err());
  }

  #[compio::test]
  async fn test_channel_hash_deterministic() {
    let h1 = channel_hash(&StringAtom::from("test"));
    let h2 = channel_hash(&StringAtom::from("test"));
    assert_eq!(h1, h2);

    let h3 = channel_hash(&StringAtom::from("other"));
    assert_ne!(h1, h3);
  }
}
