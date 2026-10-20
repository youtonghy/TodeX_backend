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
}

/// Which categories are redacted.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Categories {
    pub private_key: bool,
    pub public_key: bool,
    pub credential: bool,
    pub bank_card: bool,
    pub id_card: bool,
}

impl Categories {
    pub(crate) const ALL: Self = Self {
        private_key: true,
        public_key: true,
        credential: true,
        bank_card: true,
        id_card: true,
    };

    pub(crate) fn contains(&self, category: Category) -> bool {
        match category {
            Category::PrivateKey => self.private_key,
            Category::PublicKey => self.public_key,
            Category::Credential => self.credential,
            Category::BankCard => self.bank_card,
            Category::IdCard => self.id_card,
        }
    }

    fn any(&self) -> bool {
        self.private_key || self.public_key || self.credential || self.bank_card || self.id_card
    }
}

/// The agent output filter setting (`/v2/ssh/agent-output-filter`). On
/// unless the user turns it off.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentOutputFilter {
    pub enabled: bool,
    pub categories: Categories,
}

impl Default for AgentOutputFilter {
    fn default() -> Self {
        Self {
            enabled: true,
            categories: Categories::ALL,
        }
    }
}

impl AgentOutputFilter {
    /// The categories to redact; none while the filter is off.
    pub(crate) fn active(&self) -> Option<&Categories> {
        (self.enabled && self.categories.any()).then_some(&self.categories)
    }

    /// Reads a stored setting leniently: missing fields stay on and anything
    /// unreadable falls back to the default, so a store written by another
    /// version never turns the filter off.
    pub(crate) fn from_stored(value: serde_json::Value) -> Self {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Stored {
            enabled: bool,
            categories: StoredCategories,
        }
        impl Default for Stored {
            fn default() -> Self {
                Self {
                    enabled: true,
                    categories: StoredCategories::default(),
                }
            }
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct StoredCategories {
            private_key: bool,
            public_key: bool,
            credential: bool,
            bank_card: bool,
            id_card: bool,
        }
        impl Default for StoredCategories {
            fn default() -> Self {
                Self {
                    private_key: true,
                    public_key: true,
                    credential: true,
                    bank_card: true,
                    id_card: true,
                }
            }
        }
        match serde_json::from_value::<Stored>(value) {
            Ok(stored) => Self {
                enabled: stored.enabled,
                categories: Categories {
                    private_key: stored.categories.private_key,
                    public_key: stored.categories.public_key,
                    credential: stored.categories.credential,
                    bank_card: stored.categories.bank_card,
                    id_card: stored.categories.id_card,
                },
            },
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
    if text.is_empty() || !categories.any() {
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
    detectors::find(text, categories, &mut findings);
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
