//! `$DATA_DIR/history/fingerprint.key`: 32 random bytes behind the
//! `requestFingerprint` / `textMac` envelope fields (spec §5.2). With history
//! encrypted the backend deduplicates by comparing these MACs instead of
//! plaintext, and the MACs reveal nothing without this key.

use std::{io, path::Path};

use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use zeroize::Zeroizing;

use super::{encode_id, invalid, read_private_file, HISTORY_DIR};
use crate::{error::AppError, secure_fs};

type Result<T> = std::result::Result<T, AppError>;

const FILE_NAME: &str = "fingerprint.key";
const KEY_LEN: usize = 32;

pub(crate) struct FingerprintKey {
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl FingerprintKey {
    /// Loads the key, creating it on first use. An existing file of the
    /// wrong size or with loose permissions is an error rather than being
    /// replaced: a new key would silently break comparisons with stored MACs.
    pub(crate) fn load_or_create(data_dir: &Path) -> Result<Self> {
        let directory = data_dir.join(HISTORY_DIR);
        let path = directory.join(FILE_NAME);
        if let Some(key) = Self::read(&path)? {
            return Ok(key);
        }
        secure_fs::ensure_owner_only_dir(&directory)?;
        let mut key = Zeroizing::new([0; KEY_LEN]);
        OsRng.fill_bytes(key.as_mut_slice());
        match secure_fs::create_owner_only(&path, key.as_slice()) {
            Ok(()) => {
                super::sync_directory(&directory)?;
                Ok(Self { key })
            }
            // Another handle created it first; use theirs.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Self::read(&path)?.ok_or_else(|| invalid("history fingerprint key disappeared"))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn read(path: &Path) -> Result<Option<Self>> {
        let Some(bytes) = read_private_file(path, KEY_LEN as u64, "history fingerprint key")?
        else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        let key: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| invalid("history fingerprint key has the wrong length"))?;
        Ok(Some(Self {
            key: Zeroizing::new(key),
        }))
    }

    /// `base64url(HMAC-SHA256(key, text))`.
    pub(crate) fn mac(&self, text: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.key.as_slice())
            .expect("HMAC accepts keys of any length");
        mac.update(text.as_bytes());
        encode_id(&mac.finalize().into_bytes())
    }
}

impl std::fmt::Debug for FingerprintKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FingerprintKey").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_keys::test_support::*;

    #[test]
    fn key_is_created_once_and_private() {
        let root = temp_dir("fingerprint");
        let first = FingerprintKey::load_or_create(&root).unwrap();
        let path = root.join(HISTORY_DIR).join(FILE_NAME);
        assert_eq!(std::fs::read(&path).unwrap().len(), KEY_LEN);
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&path), 0o600);
            assert_eq!(mode_of(&root.join(HISTORY_DIR)), 0o700);
        }
        let second = FingerprintKey::load_or_create(&root).unwrap();
        let mac = first.mac("hello");
        assert_eq!(mac, second.mac("hello"));
        assert_ne!(mac, first.mac("hello!"));
        // 32-byte MAC, base64url without padding.
        assert_eq!(mac.len(), 43);

        // RFC 4231 test case 1. HMAC zero-pads short keys to the block
        // size, so the 20-byte 0x0b key equals it padded to 32 bytes.
        let mut key = [0; KEY_LEN];
        key[..20].fill(0x0b);
        let fixed = FingerprintKey {
            key: Zeroizing::new(key),
        };
        let expected: Vec<u8> = (0..32)
            .map(|index| {
                u8::from_str_radix(
                    &"b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
                        [index * 2..index * 2 + 2],
                    16,
                )
                .unwrap()
            })
            .collect();
        assert_eq!(fixed.mac("Hi There"), encode_id(&expected));
        assert_eq!(format!("{fixed:?}"), "FingerprintKey { .. }");

        std::fs::write(&path, b"short").unwrap();
        assert!(FingerprintKey::load_or_create(&root).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
