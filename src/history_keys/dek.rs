//! In-memory segment keys (DEKs) for encrypting new history (spec §3.2).
//!
//! Each conversation has at most one active DEK, created lazily on the first
//! encrypted write: generated, wrapped for every active recipient and
//! recorded in `keyring.json` (fsynced) before it is handed out. It rotates
//! when the recipient epoch changes, after 24 hours, when the active segment
//! is sealed ([`DekManager::rotate`]) and on restart (nothing is persisted).
//! Rotated keys stay in memory until their segment has been sealed and
//! repacked ([`DekManager::release_sealed`]); `SegmentKey` zeroizes on drop.

use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;

use super::{encode_id, keyring::KeyEntry, Clock, KeyringStore, RecipientRegistry};
use crate::{
    config::HistoryEncryption,
    error::AppError,
    history_crypto::{self, SegmentKey},
};

type Result<T> = std::result::Result<T, AppError>;

/// A DEK older than this is replaced on the next write.
pub(crate) const MAX_DEK_AGE: Duration = Duration::hours(24);

struct ActiveKey {
    kid: String,
    key: Arc<SegmentKey>,
    epoch: u64,
    created_at: DateTime<Utc>,
}

#[derive(Default)]
struct ConversationKeys {
    active: Option<ActiveKey>,
    /// Rotated keys whose segment is not sealed yet, by kid.
    retained: HashMap<String, Arc<SegmentKey>>,
    /// The most recently created key, while it is still in memory: what a
    /// running turn keeps encrypting under when no new key can be created
    /// (see [`DekManager::fallback_key`]).
    newest: Option<String>,
}

impl ConversationKeys {
    fn retire_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.retained.insert(active.kid, active.key);
        }
    }
}

#[derive(Clone)]
pub(crate) struct DekManager {
    recipients: RecipientRegistry,
    keyrings: KeyringStore,
    clock: Clock,
    conversations: Arc<DashMap<String, Arc<tokio::sync::Mutex<ConversationKeys>>>>,
}

impl DekManager {
    pub(crate) fn new(recipients: RecipientRegistry, keyrings: KeyringStore, clock: Clock) -> Self {
        Self {
            recipients,
            keyrings,
            clock,
            conversations: Arc::new(DashMap::new()),
        }
    }

    /// The key to encrypt the next record of `conversation_id` with, as
    /// `(kid, key)`; `None` while history encryption is off. A new key is
    /// durable in the keyring before this returns it. Fails when no
    /// recipient is active, since nobody could read the result.
    pub(crate) async fn current_key(
        &self,
        conversation_id: &str,
    ) -> Result<Option<(String, Arc<SegmentKey>)>> {
        // Validates the id before it becomes a map key.
        self.keyrings.path(conversation_id)?;
        let recipients = self.recipients.active_recipients()?;
        let slot = self.slot(conversation_id);
        let mut keys = slot.lock().await;
        if recipients.mode == HistoryEncryption::Off {
            keys.retire_active();
            return Ok(None);
        }
        let now = (self.clock)();
        if let Some(active) = &keys.active {
            let age = now.signed_duration_since(active.created_at);
            // A clock that moved backwards also rotates.
            if active.epoch == recipients.epoch && age >= Duration::zero() && age < MAX_DEK_AGE {
                return Ok(Some((active.kid.clone(), active.key.clone())));
            }
        }
        keys.retire_active();
        if recipients.keys.is_empty() {
            return Err(no_recipients());
        }
        let key = SegmentKey::generate();
        let kid = encode_id(&key.kid());
        let wraps = recipients
            .keys
            .iter()
            .map(|recipient| history_crypto::wrap(&key, recipient))
            .collect::<Result<Vec<_>>>()?;
        self.keyrings
            .add_key(
                conversation_id,
                KeyEntry {
                    kid: kid.clone(),
                    created_at: now,
                    epoch: recipients.epoch,
                    wraps,
                },
            )
            .await?;
        let key = Arc::new(key);
        keys.newest = Some(kid.clone());
        keys.active = Some(ActiveKey {
            kid: kid.clone(),
            key: key.clone(),
            epoch: recipients.epoch,
            created_at: now,
        });
        Ok(Some((kid, key)))
    }

    /// A one-off key for content that must never share a nonce with the
    /// event streams — an encrypted title (stream 2, counter 0) or a
    /// migrated segment's frames. Like [`Self::current_key`] it is wrapped
    /// for every active recipient and durable in the keyring before it is
    /// returned, but it is not kept: the caller drops (and so zeroizes) it.
    /// `None` while history encryption is off.
    pub(crate) async fn fresh_key(
        &self,
        conversation_id: &str,
    ) -> Result<Option<(String, SegmentKey)>> {
        self.keyrings.path(conversation_id)?;
        let recipients = self.recipients.active_recipients()?;
        if recipients.mode == HistoryEncryption::Off {
            return Ok(None);
        }
        if recipients.keys.is_empty() {
            return Err(no_recipients());
        }
        let key = SegmentKey::generate();
        let kid = encode_id(&key.kid());
        let wraps = recipients
            .keys
            .iter()
            .map(|recipient| history_crypto::wrap(&key, recipient))
            .collect::<Result<Vec<_>>>()?;
        self.keyrings
            .add_key(
                conversation_id,
                KeyEntry {
                    kid: kid.clone(),
                    created_at: (self.clock)(),
                    epoch: recipients.epoch,
                    wraps,
                },
            )
            .await?;
        Ok(Some((kid, key)))
    }

    /// The newest key of `conversation_id` still in memory, active or
    /// retired. A running turn whose [`Self::current_key`] fails (every
    /// recipient was revoked, the keyring cannot be written) keeps
    /// encrypting under it rather than writing plaintext; `None` once the
    /// daemon restarted or the key's segment was sealed.
    pub(crate) async fn fallback_key(
        &self,
        conversation_id: &str,
    ) -> Option<(String, Arc<SegmentKey>)> {
        let slot = self.existing_slot(conversation_id)?;
        let keys = slot.lock().await;
        let kid = keys.newest.as_ref()?;
        match &keys.active {
            Some(active) if &active.kid == kid => Some((kid.clone(), active.key.clone())),
            _ => keys.retained.get(kid).map(|key| (kid.clone(), key.clone())),
        }
    }

    /// Every key of `conversation_id` still in memory (active and retired),
    /// by kid: what a segment build may repack with.
    pub(crate) async fn keys_snapshot(
        &self,
        conversation_id: &str,
    ) -> HashMap<String, Arc<SegmentKey>> {
        let Some(slot) = self.existing_slot(conversation_id) else {
            return HashMap::new();
        };
        let keys = slot.lock().await;
        let mut snapshot = keys.retained.clone();
        if let Some(active) = &keys.active {
            snapshot.insert(active.kid.clone(), active.key.clone());
        }
        snapshot
    }

    /// Ends the active key (the active segment was sealed); the next write
    /// creates a new one. The old key stays available to [`Self::key_for`].
    pub(crate) async fn rotate(&self, conversation_id: &str) {
        if let Some(slot) = self.existing_slot(conversation_id) {
            slot.lock().await.retire_active();
        }
    }

    /// The active or retained key `kid` (segment builds take
    /// [`Self::keys_snapshot`]).
    #[cfg(test)]
    pub(crate) async fn key_for(
        &self,
        conversation_id: &str,
        kid: &str,
    ) -> Option<Arc<SegmentKey>> {
        let slot = self.existing_slot(conversation_id)?;
        let keys = slot.lock().await;
        match &keys.active {
            Some(active) if active.kid == kid => Some(active.key.clone()),
            _ => keys.retained.get(kid).cloned(),
        }
    }

    /// Drops keys whose segments are sealed (including the active one if
    /// listed). Each key is zeroized once its last user lets go.
    pub(crate) async fn release_sealed(&self, conversation_id: &str, kids: &[String]) {
        let Some(slot) = self.existing_slot(conversation_id) else {
            return;
        };
        let empty = {
            let mut keys = slot.lock().await;
            if keys
                .active
                .as_ref()
                .is_some_and(|active| kids.contains(&active.kid))
            {
                keys.active = None;
            }
            keys.retained.retain(|kid, _| !kids.contains(kid));
            if keys.newest.as_ref().is_some_and(|kid| kids.contains(kid)) {
                keys.newest = None;
            }
            keys.active.is_none() && keys.retained.is_empty()
        };
        if empty {
            self.conversations.remove_if(conversation_id, |_, slot| {
                slot.try_lock()
                    .is_ok_and(|keys| keys.active.is_none() && keys.retained.is_empty())
            });
        }
    }

    /// Forgets every key of a deleted conversation.
    pub(crate) fn forget(&self, conversation_id: &str) {
        self.conversations.remove(conversation_id);
    }

    fn slot(&self, conversation_id: &str) -> Arc<tokio::sync::Mutex<ConversationKeys>> {
        self.conversations
            .entry(conversation_id.to_owned())
            .or_default()
            .clone()
    }

    fn existing_slot(
        &self,
        conversation_id: &str,
    ) -> Option<Arc<tokio::sync::Mutex<ConversationKeys>>> {
        self.conversations
            .get(conversation_id)
            .map(|slot| slot.clone())
    }
}

/// `CONFLICT`: nobody could read what would be encrypted. New prompts are
/// refused with it; see `ConversationStore::ensure_history_writable`.
fn no_recipients() -> AppError {
    AppError::Conflict(
        "history encryption has no active recipients; register a device key \
         (history.recipient.register) or disable history encryption"
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_crypto::ContentStream;
    use crate::history_keys::{decode_id, test_support::*};
    use std::sync::Mutex;

    struct Fixture {
        root: std::path::PathBuf,
        conversation: String,
        recipients: RecipientRegistry,
        keyrings: KeyringStore,
        deks: DekManager,
        now: Arc<Mutex<DateTime<Utc>>>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = temp_dir("dek");
            let (conversation, _) = conversation_dir(&root);
            let now = Arc::new(Mutex::new(Utc::now()));
            let clock_now = now.clone();
            let clock: Clock = Arc::new(move || *clock_now.lock().unwrap());
            let recipients =
                RecipientRegistry::load(&root, HistoryEncryption::Off, None, clock.clone())
                    .unwrap();
            let keyrings = KeyringStore::new(&root);
            let deks = DekManager::new(recipients.clone(), keyrings.clone(), clock);
            Self {
                root,
                conversation,
                recipients,
                keyrings,
                deks,
                now,
            }
        }

        fn advance(&self, by: Duration) {
            *self.now.lock().unwrap() += by;
        }

        async fn current(&self) -> (String, Arc<SegmentKey>) {
            self.deks
                .current_key(&self.conversation)
                .await
                .unwrap()
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn no_key_while_encryption_is_off() {
        let fixture = Fixture::new();
        assert!(fixture
            .deks
            .current_key(&fixture.conversation)
            .await
            .unwrap()
            .is_none());
        assert!(fixture
            .keyrings
            .keys(&fixture.conversation)
            .await
            .unwrap()
            .is_empty());
        assert!(fixture.deks.current_key("not-a-uuid").await.is_err());
    }

    #[tokio::test]
    async fn new_key_is_persisted_and_unwrappable_before_use() {
        let fixture = Fixture::new();
        fixture
            .recipients
            .register_device("dev_a", &recipient(1))
            .unwrap();
        fixture.recipients.set_recovery(&recipient(9)).unwrap();
        fixture.recipients.set_mode(HistoryEncryption::E2e).unwrap();

        let (kid, key) = fixture.current().await;
        let keys = fixture.keyrings.keys(&fixture.conversation).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].kid, kid);
        assert_eq!(keys[0].epoch, 2);
        assert_eq!(keys[0].wraps.len(), 2);
        let sealed = history_crypto::seal(
            &key,
            &fixture.conversation,
            ContentStream::EventFull,
            5,
            b"x",
        )
        .unwrap();
        for byte in [1, 9] {
            let wrap = keys[0]
                .wraps
                .iter()
                .find(|wrap| wrap.rid == recipient(byte).rid())
                .unwrap();
            let unwrapped =
                history_crypto::unwrap_for_tests(seed(byte), wrap, decode_id(&kid, "kid").unwrap())
                    .unwrap();
            assert_eq!(
                history_crypto::open(
                    &unwrapped,
                    &fixture.conversation,
                    ContentStream::EventFull,
                    5,
                    &sealed
                )
                .unwrap(),
                b"x"
            );
        }
        // The same key is reused while nothing changes.
        assert_eq!(fixture.current().await.0, kid);
    }

    #[tokio::test]
    async fn rotation_rules() {
        let fixture = Fixture::new();
        fixture
            .recipients
            .register_device("dev_a", &recipient(1))
            .unwrap();
        fixture.recipients.set_mode(HistoryEncryption::E2e).unwrap();
        let (first, _) = fixture.current().await;

        // Age: still valid just before 24 h, rotated at 24 h.
        fixture.advance(MAX_DEK_AGE - Duration::seconds(1));
        assert_eq!(fixture.current().await.0, first);
        fixture.advance(Duration::seconds(1));
        let (second, _) = fixture.current().await;
        assert_ne!(second, first);

        // Epoch: a new recipient rotates and the new key is wrapped for it.
        fixture
            .recipients
            .register_device("dev_b", &recipient(2))
            .unwrap();
        let (third, _) = fixture.current().await;
        assert_ne!(third, second);
        let keys = fixture.keyrings.keys(&fixture.conversation).await.unwrap();
        assert_eq!(keys.last().unwrap().wraps.len(), 2);

        // Explicit rotation (segment sealed).
        fixture.deks.rotate(&fixture.conversation).await;
        let (fourth, _) = fixture.current().await;
        assert_ne!(fourth, third);

        // Clock moving backwards rotates too.
        fixture.advance(-Duration::hours(1));
        let (fifth, _) = fixture.current().await;
        assert_ne!(fifth, fourth);
        assert_eq!(
            fixture
                .keyrings
                .keys(&fixture.conversation)
                .await
                .unwrap()
                .len(),
            5
        );

        // Rotated keys stay available until their segment is sealed.
        for kid in [&first, &second, &third, &fourth, &fifth] {
            assert!(fixture
                .deks
                .key_for(&fixture.conversation, kid)
                .await
                .is_some());
        }
        fixture
            .deks
            .release_sealed(&fixture.conversation, &[first.clone(), second.clone()])
            .await;
        assert!(fixture
            .deks
            .key_for(&fixture.conversation, &first)
            .await
            .is_none());
        assert!(fixture
            .deks
            .key_for(&fixture.conversation, &third)
            .await
            .is_some());
        fixture
            .deks
            .release_sealed(
                &fixture.conversation,
                &[third.clone(), fourth.clone(), fifth.clone()],
            )
            .await;
        assert!(fixture
            .deks
            .key_for(&fixture.conversation, &fifth)
            .await
            .is_none());
        assert!(fixture.deks.conversations.is_empty());
    }

    #[tokio::test]
    async fn disabling_retires_the_key_and_no_recipients_fails() {
        let fixture = Fixture::new();
        let rid = fixture
            .recipients
            .register_device("dev_a", &recipient(1))
            .unwrap();
        fixture.recipients.set_mode(HistoryEncryption::E2e).unwrap();
        let (first, _) = fixture.current().await;
        fixture.recipients.set_mode(HistoryEncryption::Off).unwrap();
        assert!(fixture
            .deks
            .current_key(&fixture.conversation)
            .await
            .unwrap()
            .is_none());
        assert!(fixture
            .deks
            .key_for(&fixture.conversation, &first)
            .await
            .is_some());
        fixture.recipients.set_mode(HistoryEncryption::E2e).unwrap();
        assert_ne!(fixture.current().await.0, first);

        fixture.recipients.revoke(&rid).unwrap();
        assert_eq!(
            fixture
                .deks
                .current_key(&fixture.conversation)
                .await
                .unwrap_err()
                .code(),
            "CONFLICT"
        );
    }

    #[tokio::test]
    async fn fresh_fallback_and_snapshot_keys() {
        let fixture = Fixture::new();
        assert!(fixture
            .deks
            .fresh_key(&fixture.conversation)
            .await
            .unwrap()
            .is_none());
        let rid = fixture
            .recipients
            .register_device("dev_a", &recipient(1))
            .unwrap();
        fixture.recipients.set_mode(HistoryEncryption::E2e).unwrap();
        // A fresh key is recorded but never becomes the active one.
        let (fresh, _) = fixture
            .deks
            .fresh_key(&fixture.conversation)
            .await
            .unwrap()
            .unwrap();
        assert!(fixture
            .deks
            .keys_snapshot(&fixture.conversation)
            .await
            .is_empty());
        let (active, _) = fixture.current().await;
        assert_ne!(active, fresh);
        let kids: Vec<String> = fixture
            .keyrings
            .keys(&fixture.conversation)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.kid)
            .collect();
        assert_eq!(kids, vec![fresh.clone(), active.clone()]);
        fixture.deks.rotate(&fixture.conversation).await;
        let (second, _) = fixture.current().await;
        let snapshot = fixture.deks.keys_snapshot(&fixture.conversation).await;
        assert!(snapshot.contains_key(&active) && snapshot.contains_key(&second));
        // Without recipients the newest key in memory stays usable.
        fixture.recipients.revoke(&rid).unwrap();
        assert!(fixture
            .deks
            .current_key(&fixture.conversation)
            .await
            .is_err());
        assert!(fixture.deks.fresh_key(&fixture.conversation).await.is_err());
        assert_eq!(
            fixture
                .deks
                .fallback_key(&fixture.conversation)
                .await
                .unwrap()
                .0,
            second
        );
        fixture
            .deks
            .release_sealed(&fixture.conversation, std::slice::from_ref(&second))
            .await;
        assert!(fixture
            .deks
            .fallback_key(&fixture.conversation)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_failed_keyring_write_hands_out_no_key() {
        let fixture = Fixture::new();
        fixture
            .recipients
            .register_device("dev_a", &recipient(1))
            .unwrap();
        fixture.recipients.set_mode(HistoryEncryption::E2e).unwrap();
        // The conversation directory is gone: nothing may be encrypted.
        let missing = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            fixture.deks.current_key(&missing).await.unwrap_err().code(),
            "NOT_FOUND"
        );
        assert!(fixture.deks.key_for(&missing, "any").await.is_none());
    }
}
