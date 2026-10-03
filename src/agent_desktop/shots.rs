//! Browser screenshots taken for agents, kept outside the conversation
//! journal (events carry only a `shotId`). Each conversation keeps its
//! newest [`MAX_SHOTS_PER_CONVERSATION`]; deleting the conversation deletes
//! them.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::{error::AppError, secure_fs};

const MAX_SHOTS_PER_CONVERSATION: usize = 200;
pub(crate) const MAX_SHOT_BYTES: usize = 4 * 1024 * 1024;
const EXTENSION: &str = "jpg";

#[derive(Clone)]
pub(crate) struct ShotStore {
    root: PathBuf,
}

/// Conversation ids are client-influenced; never use them as path segments.
fn conversation_dir(root: &Path, conversation_id: &str) -> PathBuf {
    let digest = Sha256::digest(conversation_id.as_bytes());
    root.join(
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
}

/// `shot_<16-digit micros>_<16 hex>`. The micros are strictly increasing
/// within the daemon, so names sort by creation order.
fn valid_shot_id(shot_id: &str) -> bool {
    let Some(rest) = shot_id.strip_prefix("shot_") else {
        return false;
    };
    let Some((micros, random)) = rest.split_once('_') else {
        return false;
    };
    micros.len() == 16
        && micros.bytes().all(|byte| byte.is_ascii_digit())
        && random.len() == 16
        && random.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn next_micros() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap_or(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(current) => last = current,
        }
    }
}

impl ShotStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Stores a JPEG and returns its id, dropping the conversation's oldest
    /// shots beyond the cap.
    pub(crate) async fn save(
        &self,
        conversation_id: &str,
        jpeg: Vec<u8>,
    ) -> Result<String, AppError> {
        if jpeg.len() > MAX_SHOT_BYTES {
            return Err(AppError::ResourceExhausted(format!(
                "screenshot exceeds {MAX_SHOT_BYTES} bytes"
            )));
        }
        let micros = next_micros();
        let mut random = [0_u8; 8];
        OsRng.fill_bytes(&mut random);
        let shot_id = format!(
            "shot_{micros:016}_{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let root = self.root.clone();
        let dir = conversation_dir(&self.root, conversation_id);
        let path = dir.join(format!("{shot_id}.{EXTENSION}"));
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            secure_fs::ensure_owner_only_dir(&root)?;
            secure_fs::ensure_owner_only_dir(&dir)?;
            secure_fs::write_owner_only_atomic(&path, &jpeg)?;
            let mut shots: Vec<PathBuf> = std::fs::read_dir(&dir)?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == EXTENSION))
                .collect();
            if shots.len() > MAX_SHOTS_PER_CONVERSATION {
                shots.sort();
                for old in &shots[..shots.len() - MAX_SHOTS_PER_CONVERSATION] {
                    if let Err(error) = std::fs::remove_file(old) {
                        tracing::warn!(path = %old.display(), %error, "failed to prune agent screenshot");
                    }
                }
            }
            Ok(())
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(shot_id)
    }

    pub(crate) async fn read(
        &self,
        conversation_id: &str,
        shot_id: &str,
    ) -> Result<Vec<u8>, AppError> {
        if !valid_shot_id(shot_id) {
            return Err(AppError::InvalidRequest("invalid screenshot id".to_owned()));
        }
        let path =
            conversation_dir(&self.root, conversation_id).join(format!("{shot_id}.{EXTENSION}"));
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(AppError::NotFound(format!("screenshot {shot_id}")))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn remove_conversation(&self, conversation_id: &str) {
        let dir = conversation_dir(&self.root, conversation_id);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "failed to remove agent screenshots")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shots_are_capped_scoped_and_removed() {
        let root = std::env::temp_dir().join(format!("todex-shots-{}", uuid::Uuid::new_v4()));
        let store = ShotStore::new(root.clone());
        let first = store.save("conv/../a", b"jpeg-1".to_vec()).await.unwrap();
        assert!(valid_shot_id(&first), "{first}");
        assert_eq!(store.read("conv/../a", &first).await.unwrap(), b"jpeg-1");
        assert!(matches!(
            store.read("other", &first).await,
            Err(AppError::NotFound(_))
        ));
        assert!(store.read("conv/../a", "../../etc/passwd").await.is_err());
        assert!(store.save("c", vec![0; MAX_SHOT_BYTES + 1]).await.is_err());

        for index in 0..MAX_SHOTS_PER_CONVERSATION {
            store.save("conv/../a", vec![index as u8]).await.unwrap();
        }
        let dir = conversation_dir(&root, "conv/../a");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            MAX_SHOTS_PER_CONVERSATION
        );
        assert!(
            store.read("conv/../a", &first).await.is_err(),
            "oldest pruned"
        );

        store.remove_conversation("conv/../a").await;
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
