//! The gitleaks rule set (`gitleaks.toml`, gitleaks v8.30.1, MIT — see
//! `LICENSE-gitleaks`) applied to command output.
//!
//! Matching follows gitleaks: a keyword pre-filter, the rule regex, the
//! secret (the configured group, else the first non-empty group, else the
//! whole match), the entropy threshold (generic rules also need a digit),
//! then the global and rule allowlists. Output has no file path, so
//! path-only rules and path-gated rules are skipped (except the ones in
//! [`PATH_RULES_APPLIED`]), and an allowlist that requires a path never
//! applies. Only the secret is replaced, so the key name stays readable.
//!
//! The patterns are Go RE2; they compile as byte regexes with Unicode off,
//! which matches Go's ASCII `\w`, `\s`, `\b`, after [`translate`] escapes
//! the braces Go reads literally.

use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;

use super::{shannon_entropy, Categories, Category, Finding};

const CONFIG: &str = include_str!("gitleaks.toml");

/// Rules the built-in detectors replace: the PEM detector keeps the
/// BEGIN/END lines readable and also covers a block cut off by truncation.
const SUPERSEDED_RULES: &[&str] = &["private-key"];

/// Path-gated rules whose pattern is specific enough for any output:
/// `kubectl get secret -o yaml` prints exactly what this one looks for.
const PATH_RULES_APPLIED: &[&str] = &["kubernetes-secret-yaml"];

/// Rules whose secret is a private key rather than a credential.
const PRIVATE_KEY_RULES: &[&str] = &["age-secret-key"];

#[derive(Deserialize)]
struct Config {
    allowlist: Option<RawAllowlist>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRule {
    id: String,
    regex: Option<String>,
    path: Option<String>,
    entropy: Option<f64>,
    #[serde(default)]
    secret_group: usize,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    allowlists: Vec<RawAllowlist>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAllowlist {
    condition: Option<String>,
    regex_target: Option<String>,
    #[serde(default)]
    regexes: Vec<String>,
    #[serde(default)]
    stopwords: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    commits: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Secret,
    Match,
    Line,
}

struct Allowlist {
    target: Target,
    regexes: Vec<Regex>,
    stopwords: Vec<String>,
    /// `condition = "AND"`: every configured check must pass.
    all: bool,
    /// Configures a path or commit check, which output can never pass.
    needs_path: bool,
}

impl Allowlist {
    fn compile(raw: RawAllowlist) -> Result<Self, String> {
        let target = match raw.regex_target.as_deref() {
            None | Some("secret") => Target::Secret,
            Some("match") => Target::Match,
            Some("line") => Target::Line,
            Some(other) => return Err(format!("unknown regexTarget {other}")),
        };
        Ok(Self {
            target,
            regexes: raw
                .regexes
                .iter()
                .map(|pattern| compile(pattern))
                .collect::<Result<_, _>>()?,
            stopwords: raw.stopwords,
            all: raw
                .condition
                .is_some_and(|condition| condition.eq_ignore_ascii_case("and")),
            needs_path: !raw.paths.is_empty() || !raw.commits.is_empty(),
        })
    }

    /// Whether this allowlist lets the finding through as a non-secret.
    fn allows(&self, secret: &[u8], matched: &[u8], line: &[u8]) -> bool {
        let target = match self.target {
            Target::Secret => secret,
            Target::Match => matched,
            Target::Line => line,
        };
        let regex = (!self.regexes.is_empty())
            .then(|| self.regexes.iter().any(|regex| regex.is_match(target)));
        let stopword = (!self.stopwords.is_empty()).then(|| {
            let lowered = secret.to_ascii_lowercase();
            self.stopwords
                .iter()
                .any(|word| contains(&lowered, word.as_bytes()))
        });
        let checks = [regex, stopword];
        if self.all {
            !self.needs_path && checks.iter().flatten().all(|passed| *passed)
        } else {
            checks.iter().flatten().any(|passed| *passed)
        }
    }
}

struct Rule {
    id: String,
    category: Category,
    regex: Regex,
    secret_group: usize,
    entropy: Option<f64>,
    /// Indexes into [`Rules::keywords`]; empty runs on every input.
    keywords: Vec<usize>,
    allowlists: Vec<Allowlist>,
}

struct Rules {
    global: Allowlist,
    rules: Vec<Rule>,
    keywords: AhoCorasick,
    keyword_count: usize,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    // The embedded file is checked by tests; a failure here is a build bug.
    RULES.get_or_init(|| load(CONFIG).expect("embedded gitleaks.toml loads"))
}

fn load(config: &str) -> Result<Rules, String> {
    let config: Config = toml::from_str(config).map_err(|error| error.to_string())?;
    let global = match config.allowlist {
        Some(raw) => Allowlist::compile(raw)?,
        None => Allowlist {
            target: Target::Secret,
            regexes: Vec::new(),
            stopwords: Vec::new(),
            all: false,
            needs_path: false,
        },
    };
    let mut keyword_list: Vec<String> = Vec::new();
    let mut rules = Vec::new();
    for raw in config.rules {
        let Some(pattern) = raw.regex else { continue };
        if SUPERSEDED_RULES.contains(&raw.id.as_str())
            || (raw.path.is_some() && !PATH_RULES_APPLIED.contains(&raw.id.as_str()))
        {
            continue;
        }
        let regex = compile(&pattern).map_err(|error| format!("{}: {error}", raw.id))?;
        let keywords = raw
            .keywords
            .iter()
            .map(|keyword| {
                let keyword = keyword.to_ascii_lowercase();
                match keyword_list.iter().position(|known| *known == keyword) {
                    Some(index) => index,
                    None => {
                        keyword_list.push(keyword);
                        keyword_list.len() - 1
                    }
                }
            })
            .collect();
        let allowlists = raw
            .allowlists
            .into_iter()
            .map(Allowlist::compile)
            .collect::<Result<_, _>>()
            .map_err(|error| format!("{}: {error}", raw.id))?;
        rules.push(Rule {
            category: if PRIVATE_KEY_RULES.contains(&raw.id.as_str()) {
                Category::PrivateKey
            } else {
                Category::Credential
            },
            id: raw.id,
            regex,
            secret_group: raw.secret_group,
            entropy: raw.entropy,
            keywords,
            allowlists,
        });
    }
    let keywords = AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::Standard)
        .build(&keyword_list)
        .map_err(|error| error.to_string())?;
    Ok(Rules {
        global,
        rules,
        keywords,
        keyword_count: keyword_list.len(),
    })
}

fn compile(pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(&translate(pattern))
        .unicode(false)
        .size_limit(64 << 20)
        .dfa_size_limit(16 << 20)
        .build()
        .map_err(|error| error.to_string())
}

/// Escapes the `{` and `}` that Go reads as literals (anything but a
/// `{n}`, `{n,}` or `{n,m}` repetition), which Rust rejects.
fn translate(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut output = String::with_capacity(pattern.len() + 8);
    let mut index = 0;
    let mut in_class = false;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\\' => {
                let next = pattern[index + 1..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
                output.push_str(&pattern[index..index + 1 + next]);
                index += 1 + next;
                continue;
            }
            b'[' if !in_class => {
                in_class = true;
                output.push('[');
                // A leading `]` (or `^]`) is a literal member.
                let mut lookahead = index + 1;
                if bytes.get(lookahead) == Some(&b'^') {
                    output.push('^');
                    lookahead += 1;
                }
                if bytes.get(lookahead) == Some(&b']') {
                    output.push_str("\\]");
                    lookahead += 1;
                }
                index = lookahead;
                continue;
            }
            b'[' if in_class && bytes.get(index + 1) == Some(&b':') => {
                // A POSIX class such as `[:alnum:]` inside a bracket.
                if let Some(end) = pattern[index..].find(":]") {
                    output.push_str(&pattern[index..index + end + 2]);
                    index += end + 2;
                    continue;
                }
            }
            b']' if in_class => in_class = false,
            b'{' if !in_class => {
                let rest = &pattern[index + 1..];
                let repetition = rest.find('}').is_some_and(|end| {
                    let inner = &rest[..end];
                    let mut parts = inner.splitn(2, ',');
                    let low = parts.next().unwrap_or("");
                    let high = parts.next();
                    !low.is_empty()
                        && low.bytes().all(|b| b.is_ascii_digit())
                        && high.is_none_or(|high| high.bytes().all(|b| b.is_ascii_digit()))
                });
                if repetition {
                    let end = index + 1 + rest.find('}').unwrap_or(0);
                    output.push_str(&pattern[index..=end]);
                    index = end + 1;
                } else {
                    output.push_str("\\{");
                    index += 1;
                }
                continue;
            }
            b'}' if !in_class => {
                output.push_str("\\}");
                index += 1;
                continue;
            }
            _ => {}
        }
        let width = pattern[index..].chars().next().map_or(1, char::len_utf8);
        output.push_str(&pattern[index..index + width]);
        index += width;
    }
    output
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// The line around `start..end` (all lines it spans).
fn line_of(text: &[u8], start: usize, end: usize) -> &[u8] {
    let line_start = text[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let line_end = text[end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |index| end + index);
    &text[line_start..line_end]
}

pub(super) fn find(text: &str, categories: &Categories, findings: &mut Vec<Finding>) {
    let rules = rules();
    let bytes = text.as_bytes();
    let mut seen = vec![false; rules.keyword_count];
    for found in rules.keywords.find_overlapping_iter(bytes) {
        seen[found.pattern().as_usize()] = true;
    }
    for rule in &rules.rules {
        if !categories.contains(rule.category)
            || !(rule.keywords.is_empty() || rule.keywords.iter().any(|index| seen[*index]))
        {
            continue;
        }
        for captures in rule.regex.captures_iter(bytes) {
            let Some(whole) = captures.get(0) else {
                continue;
            };
            let secret = if rule.secret_group > 0 {
                captures.get(rule.secret_group)
            } else {
                (1..captures.len())
                    .filter_map(|group| captures.get(group))
                    .find(|group| !group.is_empty())
            }
            .unwrap_or(whole);
            // gitleaks trims surrounding whitespace off the secret.
            let (start, end) = trim(bytes, secret.start(), secret.end());
            if start >= end {
                continue;
            }
            let value = &bytes[start..end];
            if let Some(threshold) = rule.entropy {
                if shannon_entropy(value) <= threshold {
                    continue;
                }
                if rule.id.starts_with("generic") && !value.iter().any(u8::is_ascii_digit) {
                    continue;
                }
            }
            let matched = whole.as_bytes();
            let line = line_of(bytes, whole.start(), whole.end());
            if rules.global.allows(value, matched, line)
                || rule
                    .allowlists
                    .iter()
                    .any(|allowlist| allowlist.allows(value, matched, line))
            {
                continue;
            }
            findings.push(Finding::new(start, end, rule.category, &rule.id));
        }
    }
}

fn trim(bytes: &[u8], mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    (start, end)
}

/// Whether gitleaks' global allowlist treats `value` as a placeholder
/// (`${VAR}`, `true`, `****`, a path, …); the built-in detectors use it too.
pub(super) fn is_placeholder(value: &[u8]) -> bool {
    rules().global.allows(value, value, value)
}

#[cfg(test)]
pub(super) fn rule_ids() -> Vec<&'static str> {
    rules().rules.iter().map(|rule| rule.id.as_str()).collect()
}

#[cfg(test)]
pub(super) fn translated(pattern: &str) -> String {
    translate(pattern)
}
