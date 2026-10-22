//! Values named by their key: `key=value`, `key: value`, `"key": "value"`,
//! flow lists (`key: [a, b]`) and YAML block lists under a key with no
//! value of its own (`authentication:` followed by `- user:pass` lines).

use regex::bytes::{Regex, RegexBuilder};

use super::super::{Category, Finding};
use super::trim_ascii;

/// The three forms of one key pattern.
pub(super) struct KeyedRule {
    scalar: Regex,
    flow: Regex,
    block: Regex,
}

/// What a match of a [`KeyedRule`] becomes.
pub(super) struct Target {
    pub category: Category,
    /// The placeholder label for a value under this key.
    pub label: fn(&[u8]) -> &'static str,
    /// Skips keys that only describe a value (`*_file`, `max_tokens`).
    pub skip_key: fn(&[u8]) -> bool,
    /// Whether a value is worth hiding.
    pub accept: fn(&[u8]) -> bool,
}

impl KeyedRule {
    /// `key`: a pattern matching the whole key name (the match is anchored
    /// by non-key characters on both sides); `separators`: the assignment
    /// operators accepted after it in scalar and flow form. Block lists
    /// always use YAML's colon.
    pub(super) fn new(key: &str, case_insensitive: bool, separators: &str) -> Self {
        let key = if case_insensitive {
            format!("(?i:{key})")
        } else {
            format!("(?:{key})")
        };
        let lead = r#"(?:^|[^A-Za-z0-9_.-])["']?"#;
        let scalar = format!(
            r#"{lead}({key})["']?[ \t]*(?:{separators})[ \t]*(?:(?i:bearer|basic|token)[ \t]+)?(?:"([^"\r\n]*)"|'([^'\r\n]*)'|([^\s"',;}}\]]+))"#
        );
        let flow = format!(r#"{lead}({key})["']?[ \t]*(?:{separators})[ \t]*\[([^\]\r\n]*)\]"#);
        let block = format!(
            r#"(?m)^([ \t]*)(?:-[ \t]+)?["']?({key})["']?[ \t]*(?::|：)[ \t]*(?:#[^\r\n]*)?\r?$"#
        );
        let build = |pattern: &str| {
            RegexBuilder::new(pattern)
                .unicode(false)
                .build()
                .expect("keyed privacy pattern compiles")
        };
        Self {
            scalar: build(&scalar),
            flow: build(&flow),
            block: build(&block),
        }
    }

    pub(super) fn find(&self, bytes: &[u8], target: &Target, findings: &mut Vec<Finding>) {
        let mut push = |start: usize, end: usize, key: &[u8]| {
            findings.push(Finding::new(
                start,
                end,
                target.category,
                (target.label)(key),
            ));
        };
        for captures in self.scalar.captures_iter(bytes) {
            let (Some(key), Some(value)) = (
                captures.get(1),
                captures
                    .get(2)
                    .or_else(|| captures.get(3))
                    .or_else(|| captures.get(4)),
            ) else {
                continue;
            };
            // A URL query parameter is left to the query detector, which
            // stops at the next `&`.
            if key.start() > 0 && matches!(bytes[key.start() - 1], b'?' | b'&') {
                continue;
            }
            if (target.skip_key)(key.as_bytes()) || !accepts(target, value.as_bytes()) {
                continue;
            }
            push(value.start(), value.end(), key.as_bytes());
        }
        for captures in self.flow.captures_iter(bytes) {
            let (Some(key), Some(items)) = (captures.get(1), captures.get(2)) else {
                continue;
            };
            if (target.skip_key)(key.as_bytes()) {
                continue;
            }
            let mut start = items.start();
            for item in items.as_bytes().split(|byte| *byte == b',') {
                let end = start + item.len();
                if let Some((from, to)) = unquoted(bytes, start, end) {
                    if accepts(target, &bytes[from..to]) {
                        push(from, to, key.as_bytes());
                    }
                }
                start = end + 1;
            }
        }
        for captures in self.block.captures_iter(bytes) {
            let (Some(indent), Some(key), Some(line)) =
                (captures.get(1), captures.get(2), captures.get(0))
            else {
                continue;
            };
            if (target.skip_key)(key.as_bytes()) {
                continue;
            }
            for (from, to) in block_items(bytes, line.end(), indent.len()) {
                if accepts(target, &bytes[from..to]) {
                    push(from, to, key.as_bytes());
                }
            }
        }
    }
}

fn accepts(target: &Target, value: &[u8]) -> bool {
    let value = trim_ascii(value);
    !value.starts_with(b"[") && !value.starts_with(b"{") && (target.accept)(value)
}

/// The `- item` values of the YAML list that starts after the key line
/// ending at `from`: items at least as indented as the key, skipping
/// comment lines, up to the first other line. A list of mappings
/// (`- name: x`) is not a list of secrets and yields nothing.
fn block_items(bytes: &[u8], from: usize, key_indent: usize) -> Vec<(usize, usize)> {
    let mut items = Vec::new();
    let mut line_start = from;
    while line_start < bytes.len() {
        line_start += 1; // the newline
        let line_end = super::line_end(bytes, line_start);
        let line = &bytes[line_start..line_end];
        let indent = line
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t'))
            .unwrap_or(line.len());
        let rest = &line[indent..];
        if rest.starts_with(b"#") {
            line_start = line_end;
            continue;
        }
        let is_item = rest.first() == Some(&b'-')
            && rest
                .get(1)
                .is_none_or(|byte| matches!(byte, b' ' | b'\t' | b'\r'));
        if !is_item || indent < key_indent {
            break;
        }
        let value_start = line_start + indent + 1;
        let Some((from, to)) = item_value(bytes, value_start, line_end) else {
            line_start = line_end;
            continue;
        };
        let value = &bytes[from..to];
        if value.ends_with(b":") || super::contains(value, b": ") {
            return Vec::new();
        }
        items.push((from, to));
        line_start = line_end;
    }
    items
}

/// One list item's value without quotes or a trailing `# comment`.
fn item_value(bytes: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    let (from, to) = trimmed(bytes, start, end)?;
    if matches!(bytes[from], b'"' | b'\'') {
        return unquoted(bytes, from, to);
    }
    let to = super::find_from(&bytes[..to], b" #", from).unwrap_or(to);
    trimmed(bytes, from, to)
}

/// `start..end` without surrounding whitespace and one pair of quotes.
fn unquoted(bytes: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    let (mut from, mut to) = trimmed(bytes, start, end)?;
    if to - from >= 2 && matches!(bytes[from], b'"' | b'\'') && bytes[to - 1] == bytes[from] {
        from += 1;
        to -= 1;
    }
    (from < to).then_some((from, to))
}

fn trimmed(bytes: &[u8], mut start: usize, mut end: usize) -> Option<(usize, usize)> {
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    (start < end).then_some((start, end))
}
