//! Per-conversation `keyring.json`: every segment key id (`kid`) and its DEK
//! wrapped for each recipient. Keys are only ever added; granting a device
//! access to old history appends wraps to existing kids and never touches
//! segment files.
//!
//! Writers (the DEK manager creating a key, `history.grant.fulfill` adding
//! wraps) serialize on a per-conversation lock and replace the file
//! atomically with fsync, so a key is durable before any content uses it.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use super::{
    decode_id, encode_id, invalid, read_private_file, validate_conversation_id, write_private_file,
    MAX_BATCH,
};
use crate::{
    error::AppError,
    history_crypto::{WrappedKey, KID_LEN, RECIPIENT_ID_LEN},
};

type Result<T> = std::result::Result<T, AppError>;

pub(crate) const FILE_NAME: &str = "keyring.json";
const FILE_VERSION: u8 = 1;
/// One wrap is ~1.6 KB of JSON; this allows tens of thousands of key/recipient
/// pairs while bounding what a read may load.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Wraps per key: every recipient that ever had access, plus grants.
const MAX_WRAPS_PER_KEY: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct KeyEntry {
    /// 16 random bytes, base64url.
    pub kid: String,
    pub created_at: DateTime<Utc>,
    /// Recipient-set epoch the key was created under.
    pub epoch: u64,
    pub wraps: Vec<WrappedKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyringFile {
    version: u8,
    keys: Vec<KeyEntry>,
}

/// One page of `history.keys.list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyPage {
    /// `(conversationId, kid)` in conversation order, then key order.
    pub items: Vec<(String, String)>,
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub(crate) struct KeyringStore {
    root: PathBuf,
    locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl KeyringStore {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("conversations"),
            locks: Arc::new(DashMap::new()),
        }
    }

    /// Records a new key. The conversation directory must exist and `kid`
    /// must be new; returns once the keyring is fsynced.
    pub(crate) async fn add_key(&self, conversation_id: &str, entry: KeyEntry) -> Result<()> {
        let path = self.path(conversation_id)?;
        self.add_key_at(conversation_id, path, entry).await
    }

    /// [`Self::add_key`] into the keyring file at `path`: the conversation's
    /// own, or that of its draft directory before it is published.
    pub(crate) async fn add_key_at(
        &self,
        conversation_id: &str,
        path: PathBuf,
        entry: KeyEntry,
    ) -> Result<()> {
        decode_id::<KID_LEN>(&entry.kid, "history key id")?;
        let mut rids = HashSet::new();
        if entry.wraps.len() > MAX_WRAPS_PER_KEY
            || !entry.wraps.iter().all(|wrap| rids.insert(wrap.rid))
        {
            return Err(invalid("history key wraps must name distinct recipients"));
        }
        let lock = self.lock_for(conversation_id);
        let _guard = lock.lock().await;
        blocking(move || {
            let mut file = read_keyring(&path)?.unwrap_or_else(KeyringFile::new);
            if file.keys.iter().any(|key| key.kid == entry.kid) {
                return Err(AppError::Conflict(format!(
                    "history key {} already exists",
                    entry.kid
                )));
            }
            file.keys.push(entry);
            write_keyring(&path, &file)
        })
        .await
    }

    /// Appends wraps for `target` to existing keys. Every wrap must name
    /// `target`; every kid must exist (`NOT_FOUND` otherwise, before anything
    /// is written). A recipient that already has a wrap for a kid is skipped,
    /// so retried batches are harmless. Returns how many wraps were added.
    pub(crate) async fn append_wraps(
        &self,
        conversation_id: &str,
        target: &[u8; RECIPIENT_ID_LEN],
        wraps: Vec<(String, WrappedKey)>,
    ) -> Result<usize> {
        if wraps.iter().any(|(_, wrap)| wrap.rid != *target) {
            return Err(invalid(
                "every wrapped key must be for the target recipient",
            ));
        }
        let path = self.path(conversation_id)?;
        let lock = self.lock_for(conversation_id);
        let _guard = lock.lock().await;
        blocking(move || {
            let mut file = read_keyring(&path)?.unwrap_or_else(KeyringFile::new);
            if let Some((kid, _)) = wraps
                .iter()
                .find(|(kid, _)| !file.keys.iter().any(|key| key.kid == *kid))
            {
                return Err(AppError::NotFound(format!("history key {kid}")));
            }
            let mut added = 0;
            for (kid, wrap) in wraps {
                let key = file
                    .keys
                    .iter_mut()
                    .find(|key| key.kid == kid)
                    .expect("kids were checked above");
                if key.wraps.iter().any(|existing| existing.rid == wrap.rid) {
                    continue;
                }
                if key.wraps.len() >= MAX_WRAPS_PER_KEY {
                    return Err(AppError::ResourceExhausted(format!(
                        "history key {kid} has too many recipients"
                    )));
                }
                key.wraps.push(wrap);
                added += 1;
            }
            if added > 0 {
                write_keyring(&path, &file)?;
            }
            Ok(added)
        })
        .await
    }

    /// Every key of a conversation, oldest first (empty without a keyring).
    pub(crate) async fn keys(&self, conversation_id: &str) -> Result<Vec<KeyEntry>> {
        let path = self.path(conversation_id)?;
        blocking(move || Ok(read_keyring(&path)?.unwrap_or_default().keys)).await
    }

    /// The wraps for `rid` among `kids`; kids without one are omitted.
    pub(crate) async fn wraps_for(
        &self,
        conversation_id: &str,
        kids: &[String],
        rid: &[u8; RECIPIENT_ID_LEN],
    ) -> Result<BTreeMap<String, WrappedKey>> {
        let wanted = kids.iter().collect::<HashSet<_>>();
        Ok(self
            .keys(conversation_id)
            .await?
            .into_iter()
            .filter(|key| wanted.contains(&key.kid))
            .filter_map(|key| {
                let wrap = key.wraps.into_iter().find(|wrap| wrap.rid == *rid)?;
                Some((key.kid, wrap))
            })
            .collect())
    }

    /// Pages `(conversationId, kid)` over `conversation_ids` (any order;
    /// paged in sorted order). `cursor` is the opaque `nextCursor` of the
    /// previous page.
    pub(crate) async fn page(
        &self,
        conversation_ids: &[String],
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KeyPage> {
        if limit == 0 || limit > MAX_BATCH {
            return Err(invalid(&format!("limit must be between 1 and {MAX_BATCH}")));
        }
        let after = cursor.map(decode_cursor).transpose()?;
        let mut ids = conversation_ids.iter().collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        let mut items = Vec::new();
        for id in ids {
            let mut skip_through = None;
            if let Some((after_id, after_kid)) = &after {
                match id.cmp(after_id) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => skip_through = Some(after_kid),
                    std::cmp::Ordering::Greater => {}
                }
            }
            let keys = self.keys(id).await?;
            let start = match skip_through {
                // A kid that vanished (never expected: keys are append-only)
                // skips the rest of that conversation rather than repeating it.
                Some(kid) => keys
                    .iter()
                    .position(|key| key.kid == *kid)
                    .map_or(keys.len(), |index| index + 1),
                None => 0,
            };
            for key in &keys[start..] {
                if items.len() == limit {
                    let (last_id, last_kid): &(String, String) =
                        items.last().expect("limit is positive");
                    let next_cursor = Some(encode_cursor(last_id, last_kid));
                    return Ok(KeyPage { items, next_cursor });
                }
                items.push((id.clone(), key.kid.clone()));
            }
        }
        Ok(KeyPage {
            items,
            next_cursor: None,
        })
    }

    /// Copies every key of `conversation_id` into `directory/keyring.json`
    /// (a fork being assembled before it is published), so ciphertext the
    /// fork copied stays readable through the fork's own id. Returns whether
    /// the source had a keyring.
    pub(crate) async fn copy_into(&self, conversation_id: &str, directory: &Path) -> Result<bool> {
        let source = self.path(conversation_id)?;
        let target = directory.join(FILE_NAME);
        let lock = self.lock_for(conversation_id);
        let _guard = lock.lock().await;
        blocking(move || match read_keyring(&source)? {
            Some(file) if !file.keys.is_empty() => {
                write_keyring(&target, &file)?;
                Ok(true)
            }
            _ => Ok(false),
        })
        .await
    }

    pub(crate) fn path(&self, conversation_id: &str) -> Result<PathBuf> {
        validate_conversation_id(conversation_id)?;
        Ok(self.root.join(conversation_id).join(FILE_NAME))
    }

    fn lock_for(&self, conversation_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .entry(conversation_id.to_owned())
            .or_default()
            .clone()
    }
}

impl KeyringFile {
    fn new() -> Self {
        Self {
            version: FILE_VERSION,
            keys: Vec::new(),
        }
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| AppError::Anyhow(anyhow::anyhow!("keyring task failed: {error}")))?
}

fn read_keyring(path: &Path) -> Result<Option<KeyringFile>> {
    let Some(bytes) = read_private_file(path, MAX_FILE_BYTES, "history keyring")? else {
        return Ok(None);
    };
    let file: KeyringFile = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(&format!("history keyring file is unreadable: {error}")))?;
    let mut kids = HashSet::new();
    if file.version != FILE_VERSION
        || !file.keys.iter().all(|key| {
            decode_id::<KID_LEN>(&key.kid, "history key id").is_ok() && kids.insert(&key.kid)
        })
    {
        return Err(invalid("unsupported history keyring file"));
    }
    Ok(Some(file))
}

fn write_keyring(path: &Path, file: &KeyringFile) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| invalid("invalid keyring path"))?;
    if !directory.is_dir() {
        return Err(AppError::NotFound(format!(
            "conversation {}",
            directory
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        )));
    }
    let bytes = serde_json::to_vec(file)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(AppError::ResourceExhausted(
            "history keyring is full".to_owned(),
        ));
    }
    write_private_file(path, &bytes)
}

fn encode_cursor(conversation_id: &str, kid: &str) -> String {
    encode_id(format!("{conversation_id}/{kid}").as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(String, String)> {
    let bad = || invalid("invalid history keys cursor");
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, cursor)
        .map_err(|_| bad())?;
    let text = String::from_utf8(bytes).map_err(|_| bad())?;
    let (conversation_id, kid) = text.split_once('/').ok_or_else(bad)?;
    validate_conversation_id(conversation_id).map_err(|_| bad())?;
    decode_id::<KID_LEN>(kid, "history key id").map_err(|_| bad())?;
    Ok((conversation_id.to_owned(), kid.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_crypto::{self, SegmentKey};
    use crate::history_keys::test_support::*;

    fn entry(key: &SegmentKey, recipients: &[u8]) -> KeyEntry {
        KeyEntry {
            kid: encode_id(&key.kid()),
            created_at: Utc::now(),
            epoch: 1,
            wraps: recipients
                .iter()
                .map(|byte| history_crypto::wrap(key, &recipient(*byte)).unwrap())
                .collect(),
        }
    }

    #[tokio::test]
    async fn keys_are_added_wrapped_and_listed() {
        let root = temp_dir("keyring");
        let store = KeyringStore::new(&root);
        let (id, directory) = conversation_dir(&root);
        let key = SegmentKey::generate();
        let kid = encode_id(&key.kid());
        store.add_key(&id, entry(&key, &[1])).await.unwrap();
        assert_eq!(
            store
                .add_key(&id, entry(&key, &[1]))
                .await
                .unwrap_err()
                .code(),
            "CONFLICT"
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&directory.join(FILE_NAME)), 0o600);
        #[cfg(not(unix))]
        let _ = directory;

        // Grant: a second recipient's wrap for the same kid.
        let target = recipient(2).rid();
        let wrap = history_crypto::wrap(&key, &recipient(2)).unwrap();
        assert_eq!(
            store
                .append_wraps(&id, &target, vec![(kid.clone(), wrap.clone())])
                .await
                .unwrap(),
            1
        );
        // Duplicates are skipped, not repeated.
        assert_eq!(
            store
                .append_wraps(&id, &target, vec![(kid.clone(), wrap.clone())])
                .await
                .unwrap(),
            0
        );
        // A wrap for someone else, or for an unknown kid, is rejected.
        let other = history_crypto::wrap(&key, &recipient(3)).unwrap();
        assert_eq!(
            store
                .append_wraps(&id, &target, vec![(kid.clone(), other)])
                .await
                .unwrap_err()
                .code(),
            "INVALID_REQUEST"
        );
        let unknown = encode_id(&SegmentKey::generate().kid());
        assert_eq!(
            store
                .append_wraps(&id, &target, vec![(unknown.clone(), wrap)])
                .await
                .unwrap_err()
                .code(),
            "NOT_FOUND"
        );

        let wraps = store
            .wraps_for(&id, &[kid.clone(), unknown], &target)
            .await
            .unwrap();
        assert_eq!(wraps.len(), 1);
        let unwrapped = history_crypto::unwrap_for_tests(seed(2), &wraps[&kid], key.kid()).unwrap();
        let sealed = history_crypto::seal(
            &key,
            &id,
            history_crypto::ContentStream::EventFull,
            1,
            b"hi",
        )
        .unwrap();
        assert_eq!(
            history_crypto::open(
                &unwrapped,
                &id,
                history_crypto::ContentStream::EventFull,
                1,
                &sealed
            )
            .unwrap(),
            b"hi"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn keyring_needs_an_existing_conversation() {
        let root = temp_dir("keyring-missing");
        let store = KeyringStore::new(&root);
        let missing = uuid::Uuid::new_v4().to_string();
        let key = SegmentKey::generate();
        assert_eq!(
            store
                .add_key(&missing, entry(&key, &[1]))
                .await
                .unwrap_err()
                .code(),
            "NOT_FOUND"
        );
        assert!(store.keys(&missing).await.unwrap().is_empty());
        assert_eq!(
            store
                .add_key("../escape", entry(&key, &[1]))
                .await
                .unwrap_err()
                .code(),
            "INVALID_REQUEST"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pages_cross_conversations_with_a_cursor() {
        let root = temp_dir("keyring-page");
        let store = KeyringStore::new(&root);
        let mut ids = Vec::new();
        let mut expected = Vec::new();
        for _ in 0..3 {
            let (id, _) = conversation_dir(&root);
            for _ in 0..2 {
                let key = SegmentKey::generate();
                store.add_key(&id, entry(&key, &[1])).await.unwrap();
            }
            ids.push(id);
        }
        // An id without a keyring contributes nothing.
        let (empty, _) = conversation_dir(&root);
        ids.push(empty);
        let mut sorted = ids.clone();
        sorted.sort();
        for id in &sorted {
            for key in store.keys(id).await.unwrap() {
                expected.push((id.clone(), key.kid));
            }
        }

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.page(&ids, cursor.as_deref(), 4).await.unwrap();
            assert!(page.items.len() <= 4);
            seen.extend(page.items);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, expected);
        assert_eq!(
            store.page(&ids, Some("bogus"), 4).await.unwrap_err().code(),
            "INVALID_REQUEST"
        );
        assert!(store.page(&ids, None, 0).await.is_err());
        assert!(store.page(&ids, None, MAX_BATCH + 1).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
