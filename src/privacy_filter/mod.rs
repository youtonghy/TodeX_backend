//! Redacts secrets and personal numbers from text an agent is about to read.
//!
//! Credential rules come from gitleaks (its default `gitleaks.toml`, MIT,
//! embedded as is); keys, card, IBAN and ID numbers use checksum-validated
//! detectors in the style of nao1215/sensitive and Presidio: a pattern alone
//! is not enough, the value must also pass its checksum, or for weaker
//! patterns appear next to a context word. This is a best-effort filter for
//! what agents see, not a security boundary.

mod detectors;
mod gitleaks;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a redaction hid; also the setting key of its switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Category {
    PrivateKey,
    PublicKey,
    Credential,
    BankCard,
    IdCard,
    ProxyCredential,
    UrlToken,
    HighEntropy,
    Uuid,
    Email,
    Phone,
    PublicIp,
}

/// Declares [`Categories`], its defaults and the lenient stored form from one
/// list: `field, "jsonName", Category, default`. The aggressive ones
/// default to off.
macro_rules! categories {
    ($($field:ident, $name:literal, $category:ident, $default:literal;)*) => {
        /// Which categories are redacted.
        #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub(crate) struct Categories {
            $(pub $field: bool,)*
        }

        impl Categories {
            /// What a new install, and any field a stored or sent setting
            /// lacks, starts with.
            pub(crate) const DEFAULT: Self = Self { $($field: $default,)* };

            /// Every category, aggressive ones included.
            #[cfg(test)]
            pub(crate) const ALL: Self = Self { $($field: true,)* };

            #[cfg(test)]
            pub(crate) fn only(category: Category) -> Self {
                Self { $($field: category == Category::$category,)* }
            }

            pub(crate) fn contains(&self, category: Category) -> bool {
                match category {
                    $(Category::$category => self.$field,)*
                }
            }

            fn any(&self) -> bool {
                false $(|| self.$field)*
            }

            /// `base` overlaid with the booleans in a stored `categories`
            /// object; unknown keys and other values are ignored.
            fn overlay(base: Self, stored: &serde_json::Map<String, serde_json::Value>) -> Self {
                Self {
                    $($field: stored.get($name).and_then(serde_json::Value::as_bool).unwrap_or(base.$field),)*
                }
            }
        }

        /// A `categories` object where every field may be missing.
        #[derive(Clone, Copy, Debug, Default, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub(crate) struct CategoriesPatch {
            $(#[serde(default)] $field: Option<bool>,)*
        }

        impl CategoriesPatch {
            fn apply(self, base: Categories) -> Categories {
                Categories { $($field: self.$field.unwrap_or(base.$field),)* }
            }
        }
    };
}

categories! {
    private_key, "privateKey", PrivateKey, true;
    public_key, "publicKey", PublicKey, true;
    credential, "credential", Credential, true;
    bank_card, "bankCard", BankCard, true;
    id_card, "idCard", IdCard, true;
    proxy_credential, "proxyCredential", ProxyCredential, true;
    url_token, "urlToken", UrlToken, false;
    high_entropy, "highEntropy", HighEntropy, false;
    uuid, "uuid", Uuid, false;
    email, "email", Email, false;
    phone, "phone", Phone, false;
    public_ip, "publicIp", PublicIp, false;
}

/// At most this many custom key names, each 1–64 of `[A-Za-z0-9_.-]`.
pub(crate) const CUSTOM_KEYS_LIMIT: usize = 32;
const CUSTOM_KEY_LENGTH: usize = 64;

fn valid_custom_key(key: &str) -> bool {
    (1..=CUSTOM_KEY_LENGTH).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// The agent output filter setting (`/v2/ssh/agent-output-filter`). On
/// unless the user turns it off.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentOutputFilter {
    pub enabled: bool,
    pub categories: Categories,
    /// Extra key names whose values are redacted like passwords.
    #[serde(default)]
    pub custom_keys: Vec<String>,
}

impl Default for AgentOutputFilter {
    fn default() -> Self {
        Self {
            enabled: true,
            categories: Categories::DEFAULT,
            custom_keys: Vec::new(),
        }
    }
}

/// A `PUT` body: `enabled` and `categories` are required, but a category or
/// `customKeys` it leaves out keeps its saved value, so a client that
/// predates a switch does not reset it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentOutputFilterUpdate {
    enabled: bool,
    categories: CategoriesPatch,
    #[serde(default)]
    custom_keys: Option<Vec<String>>,
}

impl AgentOutputFilterUpdate {
    /// The setting after this update, or why the custom keys are invalid.
    pub(crate) fn apply(self, current: &AgentOutputFilter) -> Result<AgentOutputFilter, String> {
        let custom_keys = match self.custom_keys {
            None => current.custom_keys.clone(),
            Some(keys) => {
                if keys.len() > CUSTOM_KEYS_LIMIT {
                    return Err(format!("at most {CUSTOM_KEYS_LIMIT} custom keys"));
                }
                if let Some(key) = keys.iter().find(|key| !valid_custom_key(key)) {
                    return Err(format!(
                        "invalid custom key {key:?}: use 1–{CUSTOM_KEY_LENGTH} letters, digits, `_`, `.` or `-`"
                    ));
                }
                dedup_keys(keys)
            }
        };
        Ok(AgentOutputFilter {
            enabled: self.enabled,
            categories: self.categories.apply(current.categories),
            custom_keys,
        })
    }
}

/// Drops repeats (ignoring case), keeping the first spelling and the order.
fn dedup_keys(keys: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    keys.into_iter()
        .filter(|key| seen.insert(key.to_ascii_lowercase()))
        .collect()
}

impl AgentOutputFilter {
    /// The filter to apply; none while it is off or has nothing to do.
    pub(crate) fn active(&self) -> Option<&Self> {
        (self.enabled && (self.categories.any() || !self.custom_keys.is_empty())).then_some(self)
    }

    /// Redacts `text` with these categories and custom keys.
    pub(crate) fn redact(&self, text: &str) -> Redacted {
        redact_with(text, &self.categories, &self.custom_keys)
    }

    /// Reads a stored setting leniently: missing fields take their default
    /// and anything unreadable falls back to the default, so a store written
    /// by another version never turns the filter off.
    pub(crate) fn from_stored(value: serde_json::Value) -> Self {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Stored {
            enabled: bool,
            categories: serde_json::Map<String, serde_json::Value>,
            custom_keys: Vec<serde_json::Value>,
        }
        impl Default for Stored {
            fn default() -> Self {
                Self {
                    enabled: true,
                    categories: serde_json::Map::new(),
                    custom_keys: Vec::new(),
                }
            }
        }
        match serde_json::from_value::<Stored>(value) {
            Ok(stored) => {
                let custom_keys = stored
                    .custom_keys
                    .into_iter()
                    .filter_map(|key| key.as_str().map(str::to_owned))
                    .filter(|key| valid_custom_key(key))
                    .take(CUSTOM_KEYS_LIMIT)
                    .collect();
                Self {
                    enabled: stored.enabled,
                    categories: Categories::overlay(Categories::DEFAULT, &stored.categories),
                    custom_keys: dedup_keys(custom_keys),
                }
            }
            Err(error) => {
                tracing::warn!(%error, "ignoring an unreadable agent output filter setting");
                Self::default()
            }
        }
    }
}

/// Redacted text and how many values of each category it hid.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Redacted {
    pub text: String,
    pub counts: BTreeMap<Category, usize>,
}

/// One value to hide: the byte range and what replaces it.
#[derive(Debug)]
struct Finding {
    start: usize,
    end: usize,
    category: Category,
    replacement: String,
}

impl Finding {
    fn new(start: usize, end: usize, category: Category, label: &str) -> Self {
        Self {
            start,
            end,
            category,
            replacement: format!("[REDACTED:{label}]"),
        }
    }
}

/// Replaces every value of the enabled `categories` in `text`.
pub(crate) fn redact(text: &str, categories: &Categories) -> Redacted {
    redact_with(text, categories, &[])
}

/// [`redact`], also hiding the values of `custom_keys` (counted as
/// credentials) whatever the categories say.
pub(crate) fn redact_with(text: &str, categories: &Categories, custom_keys: &[String]) -> Redacted {
    if text.is_empty() || (!categories.any() && custom_keys.is_empty()) {
        return Redacted {
            text: text.to_owned(),
            ..Redacted::default()
        };
    }
    // Findings collected first win a tie over the same range: a gitleaks
    // rule names a token more precisely than an assignment does, and an ID
    // number that also passes the card checksum is reported as an ID.
    let mut findings = Vec::new();
    if categories.credential || categories.private_key {
        gitleaks::find(text, categories, &mut findings);
    }
    detectors::find(text, categories, custom_keys, &mut findings);
    apply(text, findings)
}

/// Merges overlapping findings (the earliest, then longest, names the
/// merged range) and rewrites the text.
fn apply(text: &str, mut findings: Vec<Finding>) -> Redacted {
    for finding in &mut findings {
        finding.start = floor_char_boundary(text, finding.start);
        finding.end = ceil_char_boundary(text, finding.end);
    }
    findings.retain(|finding| finding.start < finding.end);
    // Stable: equal ranges keep detector order.
    findings.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut merged: Vec<Finding> = Vec::with_capacity(findings.len());
    for finding in findings {
        match merged.last_mut() {
            Some(last) if finding.start < last.end => last.end = last.end.max(finding.end),
            _ => merged.push(finding),
        }
    }
    let mut redacted = Redacted::default();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for finding in merged {
        output.push_str(&text[copied..finding.start]);
        output.push_str(&finding.replacement);
        copied = finding.end;
        *redacted.counts.entry(finding.category).or_default() += 1;
    }
    output.push_str(&text[copied..]);
    redacted.text = output;
    redacted
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Shannon entropy in bits per byte, as gitleaks computes it.
fn shannon_entropy(value: &[u8]) -> f64 {
    if value.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for byte in value {
        counts[usize::from(*byte)] += 1;
    }
    let length = value.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let frequency = *count as f64 / length;
            -frequency * frequency.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests;
