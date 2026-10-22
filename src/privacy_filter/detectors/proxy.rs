//! Proxy and VPN node credentials (mihomo/Clash, Xray/V2Ray, sing-box and
//! share links): `uuid`, Reality `short-id`, `auth-str`, server-side VLESS
//! `decryption`, and the credential part of `vless://`, `vmess://`, `ss://`
//! and similar links. In output that looks like a proxy config, also Xray's
//! `"id": "<uuid>"` and subscription tokens in URL paths. Public keys
//! (`public-key`, client `encryption`) stay readable.

use std::sync::OnceLock;

use regex::bytes::Regex;

use super::super::{Category, Finding};
use super::aggressive::url_tokens;
use super::keyed::{KeyedRule, Target};
use super::{is_given_value, regex};

/// Key names as the configs spell them; case matters, so `UUID=` in fstab
/// and blkid output stays.
const PROXY_KEY: &str = "uuid|short-id|short_id|shortId|shortid|short-ids|short_ids|shortIds|auth-str|auth_str|authStr|decryption";

pub(super) fn find(bytes: &[u8], findings: &mut Vec<Finding>) {
    static RULE: OnceLock<KeyedRule> = OnceLock::new();
    let rule = RULE.get_or_init(|| KeyedRule::new(PROXY_KEY, false, ":"));
    rule.find(
        bytes,
        &Target {
            category: Category::ProxyCredential,
            label: key_label,
            skip_key: |_| false,
            accept: is_proxy_value,
        },
        findings,
    );
    share_links(bytes, findings);
    if is_proxy_config(bytes) {
        xray_ids(bytes, findings);
        url_tokens(
            bytes,
            Category::ProxyCredential,
            "subscription-token",
            findings,
        );
    }
}

fn key_label(key: &[u8]) -> &'static str {
    match key {
        b"uuid" => "uuid",
        key if key.starts_with(b"short") => "short-id",
        _ => "proxy-credential",
    }
}

/// Any given value (`"decryption": "none"` is not one); Reality short IDs
/// may be as short as two hex digits.
fn is_proxy_value(value: &[u8]) -> bool {
    value.len() >= 2 && is_given_value(value)
}

/// Markers of a proxy client or server config in the same output.
fn is_proxy_config(bytes: &[u8]) -> bool {
    static MARKER: OnceLock<Regex> = OnceLock::new();
    regex(
        &MARKER,
        r#"(?im)^[ \t]*(?:proxies|proxy-providers|proxy-groups)[ \t]*:|"(?:outbounds|inbounds)"[ \t]*:|\b(?:vless|vmess|trojan|hysteria2?|tuic|shadowsocks)\b"#,
    )
    .is_match(bytes)
}

/// Xray and V2Ray users: `"id": "<uuid>"`.
fn xray_ids(bytes: &[u8], findings: &mut Vec<Finding>) {
    static ID: OnceLock<Regex> = OnceLock::new();
    let id = regex(
        &ID,
        r#"(?:^|[^A-Za-z0-9_.-])["']?id["']?[ \t]*:[ \t]*["']?([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})\b"#,
    );
    for captures in id.captures_iter(bytes) {
        if let Some(value) = captures.get(1) {
            findings.push(Finding::new(
                value.start(),
                value.end(),
                Category::ProxyCredential,
                "uuid",
            ));
        }
    }
}

/// `vless://uuid@host…`, `trojan://password@…`, `ss://base64@…`,
/// `vmess://base64`: the user info, a link that is one base64 blob, and the
/// short ID or password in its query. The host, port and name stay.
fn share_links(bytes: &[u8], findings: &mut Vec<Finding>) {
    static LINK: OnceLock<Regex> = OnceLock::new();
    static QUERY: OnceLock<Regex> = OnceLock::new();
    let link = regex(
        &LINK,
        r#"(?i)\b(?:vless|vmess|trojan|ssr?|hysteria2?|hy2|tuic|anytls|socks5?|snell|juicity)://([^\s'"<>]+)"#,
    );
    let query = regex(
        &QUERY,
        r"(?i)[?&](?:sid|password|obfs-password|auth|uuid|psk)=([^&#\s]+)",
    );
    for captures in link.captures_iter(bytes) {
        let Some(body) = captures.get(1) else {
            continue;
        };
        let start = body.start();
        let text = body.as_bytes();
        // Up to the query or name; base64 may contain `/`, so not the path.
        let head_end = text
            .iter()
            .position(|byte| matches!(byte, b'?' | b'#'))
            .unwrap_or(text.len());
        let head = &text[..head_end];
        if let Some(at) = head.iter().position(|byte| *byte == b'@') {
            if at > 0 {
                findings.push(Finding::new(
                    start,
                    start + at,
                    Category::ProxyCredential,
                    "share-link",
                ));
            }
        } else if head.len() >= 16
            && head.iter().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'-')
            })
        {
            findings.push(Finding::new(
                start,
                start + head_end,
                Category::ProxyCredential,
                "share-link",
            ));
        }
        for captures in query.captures_iter(&text[head_end..]) {
            if let Some(value) = captures.get(1) {
                let offset = start + head_end;
                findings.push(Finding::new(
                    offset + value.start(),
                    offset + value.end(),
                    Category::ProxyCredential,
                    "share-link",
                ));
            }
        }
    }
}
