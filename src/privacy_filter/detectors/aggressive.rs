//! The aggressive categories, all off by default: they hide values by shape
//! alone, so they also catch things that are not secrets (commit hashes in
//! links, disk UUIDs, maintainers' addresses, version numbers that look
//! like an IP). The user opts in to each.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

use regex::bytes::Regex;

use super::super::{shannon_entropy, Categories, Category, Finding};
use super::{is_word, regex, standalone};

pub(super) fn find(bytes: &[u8], categories: &Categories, findings: &mut Vec<Finding>) {
    if categories.url_token {
        url_tokens(bytes, Category::UrlToken, "url-token", findings);
    }
    if categories.high_entropy {
        high_entropy(bytes, findings);
    }
    if categories.uuid {
        uuids(bytes, findings);
    }
    if categories.email {
        emails(bytes, findings);
    }
    if categories.phone {
        phones(bytes, findings);
    }
    if categories.public_ip {
        public_ips(bytes, findings);
    }
}

/// Path segments and query values of http(s), ws(s) and ftp URLs that look
/// like a token: 16+ hex digits with letters and digits, or 20+ base64url
/// characters with upper and lower case and digits. The host stays.
pub(super) fn url_tokens(
    bytes: &[u8],
    category: Category,
    label: &str,
    findings: &mut Vec<Finding>,
) {
    static URL: OnceLock<Regex> = OnceLock::new();
    let url = regex(&URL, r#"(?i)\b(?:https?|wss?|ftp)://[^\s'"<>()\[\]{}`]+"#);
    for found in url.find_iter(bytes) {
        let text = found.as_bytes();
        let scheme_end = text
            .windows(3)
            .position(|window| window == b"://")
            .map_or(0, |at| at + 3);
        let Some(path_start) = text[scheme_end..]
            .iter()
            .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
            .map(|at| scheme_end + at)
        else {
            continue;
        };
        let mut segment_start = path_start;
        for at in path_start..=text.len() {
            let ends = text
                .get(at)
                .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')));
            if !ends {
                continue;
            }
            let segment = &text[segment_start..at];
            if looks_like_token(segment) {
                findings.push(Finding::new(
                    found.start() + segment_start,
                    found.start() + at,
                    category,
                    label,
                ));
            }
            segment_start = at + 1;
        }
    }
}

fn looks_like_token(segment: &[u8]) -> bool {
    let digit = segment.iter().any(u8::is_ascii_digit);
    let hex = segment.iter().all(u8::is_ascii_hexdigit);
    if hex {
        return segment.len() >= 16
            && digit
            && segment.iter().any(u8::is_ascii_alphabetic)
            && shannon_entropy(segment) >= 3.0;
    }
    segment.len() >= 20
        && digit
        && segment.iter().any(u8::is_ascii_uppercase)
        && segment.iter().any(u8::is_ascii_lowercase)
        && shannon_entropy(segment) >= 3.5
}

/// Standalone runs of 40+ hex digits, or of 64+ base64 characters with
/// upper and lower case and digits, that are random enough to be key
/// material (Xray's `mlkem768x25519plus.….<base64>`, raw tokens).
fn high_entropy(bytes: &[u8], findings: &mut Vec<Finding>) {
    static BLOB: OnceLock<Regex> = OnceLock::new();
    let blob = regex(&BLOB, r"[A-Za-z0-9+/_-]{40,}={0,2}");
    for found in blob.find_iter(bytes) {
        let (start, end) = (found.start(), found.end());
        if start > 0 && matches!(bytes[start - 1], b'=' | b'%' | b'\\') {
            continue;
        }
        let value = found.as_bytes();
        let body = value
            .strip_suffix(b"==")
            .or_else(|| value.strip_suffix(b"="))
            .unwrap_or(value);
        let accepted = if body.iter().all(u8::is_ascii_hexdigit) {
            shannon_entropy(body) >= 3.0
        } else {
            body.len() >= 64
                && body.iter().any(u8::is_ascii_digit)
                && body.iter().any(u8::is_ascii_uppercase)
                && body.iter().any(u8::is_ascii_lowercase)
                && shannon_entropy(body) >= 4.5
        };
        if accepted {
            findings.push(Finding::new(
                start,
                end,
                Category::HighEntropy,
                "high-entropy",
            ));
        }
    }
}

fn uuids(bytes: &[u8], findings: &mut Vec<Finding>) {
    static UUID: OnceLock<Regex> = OnceLock::new();
    let uuid = regex(
        &UUID,
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    );
    for found in uuid.find_iter(bytes) {
        let (start, end) = (found.start(), found.end());
        let before = start > 0 && (is_word(bytes[start - 1]) || bytes[start - 1] == b'-');
        let after = bytes
            .get(end)
            .is_some_and(|byte| is_word(*byte) || *byte == b'-');
        if !before && !after {
            findings.push(Finding::new(start, end, Category::Uuid, "uuid"));
        }
    }
}

fn emails(bytes: &[u8], findings: &mut Vec<Finding>) {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    let email = regex(
        &EMAIL,
        r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,24}\b",
    );
    for found in email.find_iter(bytes) {
        findings.push(Finding::new(
            found.start(),
            found.end(),
            Category::Email,
            "email",
        ));
    }
}

/// Mainland China mobile numbers, optionally with +86 and grouped 3-4-4.
fn phones(bytes: &[u8], findings: &mut Vec<Finding>) {
    static PHONE: OnceLock<Regex> = OnceLock::new();
    let phone = regex(
        &PHONE,
        r"(?:\+86[ -]?)?1[3-9][0-9](?:[0-9]{8}|[ -][0-9]{4}[ -][0-9]{4})",
    );
    for found in phone.find_iter(bytes) {
        let (start, end) = (found.start(), found.end());
        let raw = found.as_bytes();
        let separators: Vec<u8> = raw[raw.len() - 11..]
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_digit())
            .collect();
        if separators.windows(2).any(|pair| pair[0] != pair[1]) || !standalone(bytes, start, end) {
            continue;
        }
        findings.push(Finding::new(start, end, Category::Phone, "phone"));
    }
}

/// IPv4 and IPv6 addresses outside private, loopback, link-local, shared,
/// documentation, benchmarking, multicast and reserved ranges.
fn public_ips(bytes: &[u8], findings: &mut Vec<Finding>) {
    static V4: OnceLock<Regex> = OnceLock::new();
    static V6: OnceLock<Regex> = OnceLock::new();
    let v4 = regex(&V4, r"[0-9]{1,3}(?:\.[0-9]{1,3}){3}");
    for found in v4.find_iter(bytes) {
        let (start, end) = (found.start(), found.end());
        let before = start > 0 && (is_word(bytes[start - 1]) || bytes[start - 1] == b'.');
        let after = bytes.get(end).is_some_and(|byte| is_word(*byte))
            || (bytes.get(end) == Some(&b'.')
                && bytes.get(end + 1).is_some_and(u8::is_ascii_digit));
        if before || after {
            continue;
        }
        let parsed = std::str::from_utf8(found.as_bytes())
            .ok()
            .and_then(|text| text.parse::<Ipv4Addr>().ok());
        if parsed.is_some_and(public_v4) {
            findings.push(Finding::new(start, end, Category::PublicIp, "public-ip"));
        }
    }
    let v6 = regex(&V6, r"[0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7}");
    for found in v6.find_iter(bytes) {
        let (start, mut end) = (found.start(), found.end());
        if start > 0 && (is_word(bytes[start - 1]) || bytes[start - 1] == b':') {
            continue;
        }
        // `addr:` at the end of a sentence or before a port.
        if end - start > 2 && bytes[end - 1] == b':' && bytes[end - 2] != b':' {
            end -= 1;
        }
        if bytes
            .get(end)
            .is_some_and(|byte| is_word(*byte) || *byte == b'.')
        {
            continue;
        }
        let parsed = std::str::from_utf8(&bytes[start..end])
            .ok()
            .and_then(|text| text.parse::<Ipv6Addr>().ok());
        if parsed.is_some_and(public_v6) {
            findings.push(Finding::new(start, end, Category::PublicIp, "public-ip"));
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (18..20).contains(&b))
        || (a == 192 && b == 0 && c == 0))
}

/// Global unicast (2000::/3) outside the documentation prefix.
fn public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] & 0xe000 == 0x2000 && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
}
