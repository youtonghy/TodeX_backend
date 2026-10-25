//! Built-in detectors: PEM/OpenSSH keys, secret assignments and URL
//! credentials, and numbers that must pass a checksum (cards with Luhn and
//! issuer ranges, IBAN with MOD 97, national ID numbers with their check
//! digits). A pattern without a checksum only counts next to a context word.
//! ID numbers, phone numbers, proxy node credentials and the aggressive,
//! off-by-default categories live in [`ids`], [`phones`], [`proxy`] and
//! [`aggressive`].

mod aggressive;
mod ids;
mod keyed;
mod phones;
mod proxy;

use std::sync::OnceLock;

use base64::Engine as _;
use regex::bytes::{Regex, RegexBuilder};

use self::keyed::{KeyedRule, Target};
use super::{gitleaks::is_placeholder, Categories, Category, Finding};

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| build(pattern))
}

fn build(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .unicode(false)
        .build()
        .expect("built-in privacy pattern compiles")
}

pub(super) fn find(
    text: &str,
    categories: &Categories,
    custom_keys: &[String],
    findings: &mut Vec<Finding>,
) {
    let bytes = text.as_bytes();
    if categories.private_key || categories.public_key {
        armored_blocks(bytes, categories, findings);
    }
    if categories.public_key {
        openssh_public_keys(bytes, findings);
    }
    if categories.id_card {
        ids::find(bytes, findings);
    }
    if categories.bank_card {
        card_numbers(bytes, findings);
        ibans(bytes, findings);
    }
    if categories.credential {
        secret_assignments(bytes, findings);
        url_credentials(bytes, findings);
        url_query_secrets(bytes, findings);
        bearer_tokens(bytes, findings);
        basic_credentials(bytes, findings);
        prefixed_tokens(bytes, findings);
        password_hashes(bytes, findings);
    }
    if !custom_keys.is_empty() {
        custom_key_values(bytes, custom_keys, findings);
    }
    if categories.proxy_credential {
        proxy::find(bytes, findings);
    }
    aggressive::find(bytes, categories, findings);
}

// ---------------------------------------------------------------- keys

/// PEM, PGP and RFC 4716 blocks: the body between the BEGIN and END lines
/// is replaced, so the kind of block stays readable. A block without its
/// END line (cut by truncation) is redacted while lines still look like
/// armored content, so a lone BEGIN string in source code hides nothing.
fn armored_blocks(bytes: &[u8], categories: &Categories, findings: &mut Vec<Finding>) {
    static BEGIN: OnceLock<Regex> = OnceLock::new();
    let begin = regex(&BEGIN, r"-{4,5} ?BEGIN ([A-Z0-9][A-Z0-9 ]{0,60}?) ?-{4,5}");
    let mut from = 0;
    while let Some(captures) = begin.captures_at(bytes, from) {
        let (Some(header), Some(label)) = (captures.get(0), captures.get(1)) else {
            break;
        };
        from = header.end();
        let label = label.as_bytes();
        let category = if contains(label, b"PRIVATE KEY") {
            Category::PrivateKey
        } else if contains(label, b"PUBLIC KEY") || label == b"CERTIFICATE" {
            Category::PublicKey
        } else {
            continue;
        };
        if !categories.contains(category) {
            continue;
        }
        let body_start = header.end();
        let mut footer = b"END ".to_vec();
        footer.extend_from_slice(label);
        // Escaped newlines (`\n` in JSON) keep a whole block on one line.
        let line_end = line_end(bytes, body_start);
        let body_end = match find_from(&bytes[..line_end], &footer, body_start) {
            Some(end) => dashes_before(bytes, body_start, end),
            None => armored_body_end(bytes, body_start, &footer),
        };
        if body_end > body_start
            && bytes[body_start..body_end]
                .iter()
                .any(u8::is_ascii_alphanumeric)
        {
            let label = if category == Category::PrivateKey {
                "private_key"
            } else {
                "public_key"
            };
            findings.push(Finding::new(body_start, body_end, category, label));
            from = body_end;
        }
    }
}

/// Where a block body that continues past its BEGIN line ends: before the
/// END line, or after the last line that still looks like armored content.
fn armored_body_end(bytes: &[u8], body_start: usize, footer: &[u8]) -> usize {
    let mut end = body_start;
    let mut line_start = line_end(bytes, body_start);
    // Anything after BEGIN on its own line must be armor too.
    if !is_armor_line(&bytes[body_start..line_start]) {
        return body_start;
    }
    while line_start < bytes.len() {
        line_start += 1; // the newline
        let next = line_end(bytes, line_start);
        let line = &bytes[line_start..next];
        if let Some(at) = find_from(line, footer, 0) {
            return dashes_before(bytes, body_start, line_start + at);
        }
        if !is_armor_line(line) {
            break;
        }
        if line.iter().any(|byte| !byte.is_ascii_whitespace()) {
            end = next;
        }
        line_start = next;
    }
    end
}

/// Base64 body, `Key: value` headers (Proc-Type, Comment, …), PGP
/// checksums and blank lines, optionally indented or quoted.
fn is_armor_line(line: &[u8]) -> bool {
    let line = trim_ascii(line);
    let line = line
        .strip_suffix(b"\\n\",")
        .or_else(|| line.strip_suffix(b"\\n\""))
        .or_else(|| line.strip_suffix(b"\\n"))
        .or_else(|| line.strip_suffix(b"\""))
        .unwrap_or(line);
    let line = line.strip_prefix(b"\"").unwrap_or(line);
    if line.is_empty() {
        return true;
    }
    if line
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return true;
    }
    // A header: a token, a colon, then text.
    let key_len = line
        .iter()
        .position(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'-'))
        .unwrap_or(line.len());
    key_len > 0 && line.get(key_len) == Some(&b':')
}

/// Start of the dashes (and space) that open the END line at `end`.
fn dashes_before(bytes: &[u8], body_start: usize, mut end: usize) -> usize {
    while end > body_start && matches!(bytes[end - 1], b'-' | b' ') {
        end -= 1;
    }
    end
}

/// `ssh-ed25519 AAAA… comment`: the key data is replaced, the algorithm
/// and comment stay.
fn openssh_public_keys(bytes: &[u8], findings: &mut Vec<Finding>) {
    static KEY: OnceLock<Regex> = OnceLock::new();
    let key = regex(
        &KEY,
        r"(?:ssh-(?:rsa|dss|ed25519|ed448)|ecdsa-sha2-nistp(?:256|384|521)|sk-ssh-ed25519@openssh\.com|sk-ecdsa-sha2-nistp256@openssh\.com)(?:-cert-v01@openssh\.com)?[ \t]+(AAAA[A-Za-z0-9+/]{20,}={0,3})",
    );
    for captures in key.captures_iter(bytes) {
        let (Some(whole), Some(data)) = (captures.get(0), captures.get(1)) else {
            continue;
        };
        if whole.start() > 0 && is_word(bytes[whole.start() - 1]) {
            continue;
        }
        findings.push(Finding::new(
            data.start(),
            data.end(),
            Category::PublicKey,
            "public_key",
        ));
    }
}

// ---------------------------------------------------------- credentials

/// Key names that hold a secret: anything containing a secret word, plus a
/// few whole names (`auth`, mihomo's `authentication` list) that would match
/// too much as a part (`author`, `oauth_url`).
const SECRET_KEY: &str = r"[A-Za-z0-9_.-]*(?:passw(?:or)?d|passphrase|secret|token|api[_.-]?key|private[_.-]?key|access[_.-]?key|credential|authorization|auth[_.-]?(?:key|token)|session[_.-]?key|pre[_.-]?shared[_.-]?key|psk)[A-Za-z0-9_.-]*|auth|authentication|basic[_.-]?auth|htpasswd|密码|口令|密钥|私钥|令牌";

/// `password=…`, `"api_key": "…"`, `export DB_SECRET=…`, `密码：…`, and
/// lists under such a key (`authentication:` then `- user:pass`): the
/// values after a key that names a secret.
fn secret_assignments(bytes: &[u8], findings: &mut Vec<Finding>) {
    static RULE: OnceLock<KeyedRule> = OnceLock::new();
    let rule = RULE.get_or_init(|| KeyedRule::new(SECRET_KEY, true, "=>|:=|=|:|："));
    rule.find(
        bytes,
        &Target {
            category: Category::Credential,
            label: |_| "credential",
            skip_key: is_metadata_key,
            accept: is_credential_value,
        },
        findings,
    );
}

/// Keys that hold metadata about a secret, not the secret.
fn is_metadata_key(key: &[u8]) -> bool {
    static IGNORED_KEY: OnceLock<Regex> = OnceLock::new();
    let ignored_key = regex(
        &IGNORED_KEY,
        r"(?i)(?:^|[_.-])(?:types?|files?|paths?|dirs?|urls?|uris?|endpoints?|ids?|names?|length|len|size|count|expires?|expires[_.-]?(?:at|in)|expiry|ttl|limit|max|min|used|enabled|required|header|prefix|field|env|ref|hash[_.-]?alg|alg|algorithm|kind|format|policy|version|mode|store)$|^(?:max|min)[_.-]|csrf|^public[_.-]?|tokens$|tokenizer|tokenize",
    );
    key.is_ascii() && ignored_key.is_match(key)
}

/// A secret value, but not an armored key (left to the block detector) or
/// the name of an auth scheme (`auth: basic`, pg_hba's `md5`).
fn is_credential_value(value: &[u8]) -> bool {
    const SCHEMES: &[&str] = &[
        "basic",
        "bearer",
        "digest",
        "password",
        "token",
        "oauth",
        "oauth2",
        "jwt",
        "apikey",
        "api_key",
        "publickey",
        "plain",
        "md5",
        "scram-sha-256",
        "trust",
        "peer",
        "ident",
    ];
    let lowered = value.to_ascii_lowercase();
    is_secret_value(value)
        && !contains(value, b"-----BEGIN ")
        && !SCHEMES.iter().any(|scheme| lowered == scheme.as_bytes())
}

/// The values of the user's own key names, matched whole and in any case.
fn custom_key_values(bytes: &[u8], keys: &[String], findings: &mut Vec<Finding>) {
    let pattern = keys
        .iter()
        .map(|key| regex::escape(key))
        .collect::<Vec<_>>()
        .join("|");
    KeyedRule::new(&pattern, true, "=>|:=|=|:|：").find(
        bytes,
        &Target {
            category: Category::Credential,
            label: |_| "custom-key",
            skip_key: |_| false,
            // The user named the key: short PINs count too.
            accept: is_given_value,
        },
        findings,
    );
}

/// Not empty, not a placeholder and not a small number or flag.
fn is_secret_value(value: &[u8]) -> bool {
    value.len() >= 3
        && !(value.len() < 8 && value.iter().all(u8::is_ascii_digit))
        && is_given_value(value)
}

/// Not empty, a placeholder, a reference to a value kept elsewhere or a
/// word standing for no value.
fn is_given_value(value: &[u8]) -> bool {
    const NOT_SECRETS: &[&str] = &[
        "null",
        "none",
        "nil",
        "true",
        "false",
        "yes",
        "no",
        "on",
        "off",
        "undefined",
        "required",
        "optional",
        "string",
        "redacted",
        "hidden",
        "empty",
        "unset",
    ];
    if value.is_empty() {
        return false;
    }
    let lowered = value.to_ascii_lowercase();
    if NOT_SECRETS.iter().any(|word| lowered == word.as_bytes())
        || lowered.starts_with(b"[redacted")
        // `<your password>`, also when the value stops at its first space.
        || value.starts_with(b"<")
        // References to a secret kept elsewhere: `${VAR}`, `$(cmd)`, `{{ x }}`.
        || value.starts_with(b"${")
        || value.starts_with(b"$(")
        || value.starts_with(b"{{")
    {
        return false;
    }
    !is_placeholder(value)
}

/// `scheme://user:password@host`: the password.
fn url_credentials(bytes: &[u8], findings: &mut Vec<Finding>) {
    static URL: OnceLock<Regex> = OnceLock::new();
    let url = regex(
        &URL,
        r#"[A-Za-z][A-Za-z0-9+.-]{1,20}://[^/\s:@'"<>]{1,128}:([^/\s@'"<>]{1,256})@"#,
    );
    for captures in url.captures_iter(bytes) {
        let Some(password) = captures.get(1) else {
            continue;
        };
        if !is_secret_value(password.as_bytes()) {
            continue;
        }
        findings.push(Finding::new(
            password.start(),
            password.end(),
            Category::Credential,
            "url-password",
        ));
    }
}

/// `Bearer <token>` anywhere (headers in logs, `curl -v` output).
fn bearer_tokens(bytes: &[u8], findings: &mut Vec<Finding>) {
    static BEARER: OnceLock<Regex> = OnceLock::new();
    let bearer = regex(&BEARER, r"(?i)\bbearer[ \t]+([A-Za-z0-9._~+/=-]{16,})");
    for captures in bearer.captures_iter(bytes) {
        let Some(token) = captures.get(1) else {
            continue;
        };
        // A prose word (`Bearer authentication`) has no digits.
        if !token.as_bytes().iter().any(u8::is_ascii_digit) {
            continue;
        }
        findings.push(Finding::new(
            token.start(),
            token.end(),
            Category::Credential,
            "bearer-token",
        ));
    }
}

/// Secrets in a URL query (`?token=…`, `&sig=…`, presigned
/// `X-Amz-Signature`): the value up to the next `&`.
fn url_query_secrets(bytes: &[u8], findings: &mut Vec<Finding>) {
    static QUERY: OnceLock<Regex> = OnceLock::new();
    let query = regex(
        &QUERY,
        r#"(?i)[?&](?:access_token|refresh_token|id_token|token|api[_-]?key|key|sig|signature|auth|secret|password|passwd|x-amz-signature|x-amz-security-token|x-goog-signature)=([^&#\s"'<>]+)"#,
    );
    for captures in query.captures_iter(bytes) {
        let Some(value) = captures.get(1) else {
            continue;
        };
        if !is_secret_value(value.as_bytes()) {
            continue;
        }
        findings.push(Finding::new(
            value.start(),
            value.end(),
            Category::Credential,
            "url-secret",
        ));
    }
}

/// `Basic <base64 of user:password>` anywhere: only when it decodes to
/// printable text with a colon, so `Basic authentication` stays.
fn basic_credentials(bytes: &[u8], findings: &mut Vec<Finding>) {
    static BASIC: OnceLock<Regex> = OnceLock::new();
    let basic = regex(&BASIC, r"(?i)\bbasic[ \t]+([A-Za-z0-9+/]{6,}={0,2})");
    for captures in basic.captures_iter(bytes) {
        let Some(encoded) = captures.get(1) else {
            continue;
        };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes())
        else {
            continue;
        };
        let printable = decoded.iter().all(|byte| (0x20..0x7f).contains(byte));
        if !printable || !decoded.contains(&b':') || decoded.len() < 3 {
            continue;
        }
        findings.push(Finding::new(
            encoded.start(),
            encoded.end(),
            Category::Credential,
            "basic-auth",
        ));
    }
}

/// Well-known credential prefixes gitleaks has no rule for (or only one
/// that needs a key name): xAI keys and the generic `sk-` form.
fn prefixed_tokens(bytes: &[u8], findings: &mut Vec<Finding>) {
    static PREFIXED: OnceLock<Regex> = OnceLock::new();
    let prefixed = regex(&PREFIXED, r"\b(?:xai|sk)-[A-Za-z0-9_-]{20,}");
    for found in prefixed.find_iter(bytes) {
        let value = found.as_bytes();
        if !value.iter().any(u8::is_ascii_digit) {
            continue;
        }
        findings.push(Finding::new(
            found.start(),
            found.end(),
            Category::Credential,
            "api-key",
        ));
    }
}

/// crypt(3) hashes as in `/etc/shadow`: `$6$salt$hash`, `$2b$…`, `$y$…`.
fn password_hashes(bytes: &[u8], findings: &mut Vec<Finding>) {
    static HASH: OnceLock<Regex> = OnceLock::new();
    let hash = regex(
        &HASH,
        r"\$(?:1|2[abxy]?|5|6|7|y|gy|sha1|md5|argon2(?:id|i|d))\$[./A-Za-z0-9$=,+-]{16,}",
    );
    for found in hash.find_iter(bytes) {
        if found.start() > 0 && is_word(bytes[found.start() - 1]) {
            continue;
        }
        findings.push(Finding::new(
            found.start(),
            found.end(),
            Category::Credential,
            "password-hash",
        ));
    }
}

// -------------------------------------------------------------- numbers

/// Card numbers: 13–19 digits, contiguous or in groups of four (Amex
/// 4-6-5), that pass Luhn and fall in a card issuer's range. UnionPay
/// numbers (62…) that fail Luhn, which some do, count next to a card word.
fn card_numbers(bytes: &[u8], findings: &mut Vec<Finding>) {
    static CARD: OnceLock<Regex> = OnceLock::new();
    let card = regex(
        &CARD,
        r"[0-9]{4}[ -][0-9]{6}[ -][0-9]{4,5}|[0-9]{4}(?:[ -][0-9]{4}){2,3}(?:[ -][0-9]{1,4})?|[0-9]{13,19}",
    );
    let mut from = 0;
    while let Some(found) = card.find_at(bytes, from) {
        from = found.start() + 1;
        let (start, end) = (found.start(), found.end());
        if !standalone(bytes, start, end) || continues_number(bytes, start, end) {
            // Skip the rest of a longer number instead of matching inside it.
            from = end;
            continue;
        }
        let raw = found.as_bytes();
        let separators: Vec<u8> = raw
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_digit())
            .collect();
        if separators.windows(2).any(|pair| pair[0] != pair[1]) {
            continue;
        }
        let digits: Vec<u8> = raw.iter().copied().filter(u8::is_ascii_digit).collect();
        if !(13..=19).contains(&digits.len()) {
            continue;
        }
        let issuer = card_issuer_length_ok(&digits);
        let accepted = (issuer && luhn(&digits))
            || (digits.starts_with(b"62") && digits.len() >= 16 && near_card_word(bytes, start));
        if !accepted {
            continue;
        }
        let last4 = String::from_utf8_lossy(&digits[digits.len() - 4..]).into_owned();
        findings.push(Finding {
            start,
            end,
            category: Category::BankCard,
            replacement: format!("[REDACTED:bank_card ****{last4}]"),
        });
        from = end;
    }
}

fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            let value = u32::from(digit - b'0');
            if index % 2 == 1 {
                let doubled = value * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                value
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Whether a 13–19 digit number starts in a card issuer's range with one
/// of its lengths (Visa, Mastercard, Amex, Discover, UnionPay, JCB, Diners,
/// Maestro, Mir, RuPay).
fn card_issuer_length_ok(digits: &[u8]) -> bool {
    let prefix = |count: usize| -> u32 {
        digits[..count]
            .iter()
            .fold(0, |value, digit| value * 10 + u32::from(digit - b'0'))
    };
    let length = digits.len();
    let (p2, p3, p4) = (prefix(2), prefix(3), prefix(4));
    match digits[0] {
        b'4' => matches!(length, 13 | 16 | 19),
        // Mir, then Mastercard's 2-series.
        b'2' => {
            (2200..=2204).contains(&p4) && length >= 16
                || (2221..=2720).contains(&p4) && length == 16
        }
        b'3' => {
            (p2 == 34 || p2 == 37) && length == 15
                || (3528..=3589).contains(&p4) && length >= 16
                || ((300..=305).contains(&p3) || matches!(p2, 36 | 38 | 39)) && length >= 14
        }
        // Mastercard 51–55 and Maestro 50–59; Discover, UnionPay, RuPay
        // and Maestro across 6.
        b'5' | b'6' => true,
        _ => false,
    }
}

/// A card word in the 40 bytes before `start`.
fn near_card_word(bytes: &[u8], start: usize) -> bool {
    const WORDS: &[&str] = &[
        "card",
        "pan",
        "acct",
        "account",
        "卡号",
        "银行卡",
        "信用卡",
        "借记卡",
        "账号",
        "帐号",
    ];
    let window = bytes[start.saturating_sub(40)..start].to_ascii_lowercase();
    WORDS.iter().any(|word| contains(&window, word.as_bytes()))
}

/// IBANs with a known country length that pass the MOD 97 check.
fn ibans(bytes: &[u8], findings: &mut Vec<Finding>) {
    static IBAN: OnceLock<Regex> = OnceLock::new();
    let iban = regex(&IBAN, r"[A-Z]{2}[0-9]{2}[ ]?[A-Z0-9]{4}");
    let mut from = 0;
    while let Some(found) = iban.find_at(bytes, from) {
        from = found.start() + 1;
        let start = found.start();
        if start > 0 && is_word(bytes[start - 1]) {
            continue;
        }
        let Some(length) = iban_length(&bytes[start..start + 2]) else {
            continue;
        };
        // Collect `length` characters, allowing single spaces between them.
        let mut compact = Vec::with_capacity(length);
        let mut end = start;
        while end < bytes.len() && compact.len() < length {
            let byte = bytes[end];
            if byte.is_ascii_uppercase() || byte.is_ascii_digit() {
                compact.push(byte);
            } else if !(byte == b' ' && end > start && bytes[end - 1] != b' ') {
                break;
            }
            end += 1;
        }
        if compact.len() != length || (end < bytes.len() && is_word(bytes[end])) {
            continue;
        }
        if !iban_checksum(&compact) {
            continue;
        }
        findings.push(Finding::new(start, end, Category::BankCard, "iban"));
        from = end;
    }
}

fn iban_checksum(iban: &[u8]) -> bool {
    let mut remainder: u32 = 0;
    for byte in iban[4..].iter().chain(&iban[..4]) {
        let value = if byte.is_ascii_digit() {
            u32::from(byte - b'0')
        } else {
            u32::from(byte - b'A') + 10
        };
        remainder = if value >= 10 {
            (remainder * 100 + value) % 97
        } else {
            (remainder * 10 + value) % 97
        };
    }
    remainder == 1
}

/// IBAN length by country (SWIFT IBAN registry).
fn iban_length(country: &[u8]) -> Option<usize> {
    const LENGTHS: &[(&[u8; 2], usize)] = &[
        (b"AD", 24),
        (b"AE", 23),
        (b"AL", 28),
        (b"AT", 20),
        (b"AZ", 28),
        (b"BA", 20),
        (b"BE", 16),
        (b"BG", 22),
        (b"BH", 22),
        (b"BI", 27),
        (b"BR", 29),
        (b"BY", 28),
        (b"CH", 21),
        (b"CR", 22),
        (b"CY", 28),
        (b"CZ", 24),
        (b"DE", 22),
        (b"DJ", 27),
        (b"DK", 18),
        (b"DO", 28),
        (b"EE", 20),
        (b"EG", 29),
        (b"ES", 24),
        (b"FI", 18),
        (b"FK", 18),
        (b"FO", 18),
        (b"FR", 27),
        (b"GB", 22),
        (b"GE", 22),
        (b"GI", 23),
        (b"GL", 18),
        (b"GR", 27),
        (b"GT", 28),
        (b"HR", 21),
        (b"HU", 28),
        (b"IE", 22),
        (b"IL", 23),
        (b"IQ", 23),
        (b"IS", 26),
        (b"IT", 27),
        (b"JO", 30),
        (b"KW", 30),
        (b"KZ", 20),
        (b"LB", 28),
        (b"LC", 32),
        (b"LI", 21),
        (b"LT", 20),
        (b"LU", 20),
        (b"LV", 21),
        (b"LY", 25),
        (b"MC", 27),
        (b"MD", 24),
        (b"ME", 22),
        (b"MK", 19),
        (b"MN", 20),
        (b"MR", 27),
        (b"MT", 31),
        (b"MU", 30),
        (b"NI", 28),
        (b"NL", 18),
        (b"NO", 15),
        (b"OM", 23),
        (b"PK", 24),
        (b"PL", 28),
        (b"PS", 29),
        (b"PT", 25),
        (b"QA", 29),
        (b"RO", 24),
        (b"RS", 22),
        (b"RU", 33),
        (b"SA", 24),
        (b"SC", 31),
        (b"SD", 18),
        (b"SE", 24),
        (b"SI", 19),
        (b"SK", 24),
        (b"SM", 27),
        (b"SO", 23),
        (b"ST", 25),
        (b"SV", 28),
        (b"TL", 23),
        (b"TN", 24),
        (b"TR", 26),
        (b"UA", 29),
        (b"VA", 22),
        (b"VG", 24),
        (b"XK", 20),
        (b"YE", 30),
    ];
    LENGTHS
        .iter()
        .find(|(code, _)| code.as_slice() == country)
        .map(|(_, length)| *length)
}

/// A number shape for [`find_numbers`]: what it is redacted as, the check
/// it must pass, and the words one of which must come shortly before it
/// (none: the shape and check are distinctive enough on their own).
struct NumberRule {
    label: &'static str,
    pattern: &'static str,
    valid: fn(&[u8]) -> bool,
    context: &'static [&'static str],
}

/// Standalone matches of `rules` that pass their check and context, as
/// `category`. `compiled` caches the rules' regexes in order.
fn find_numbers(
    bytes: &[u8],
    rules: &[NumberRule],
    compiled: &'static OnceLock<Vec<Regex>>,
    category: Category,
    findings: &mut Vec<Finding>,
) {
    let compiled = compiled.get_or_init(|| rules.iter().map(|rule| build(rule.pattern)).collect());
    for (rule, pattern) in rules.iter().zip(compiled) {
        let mut from = 0;
        while let Some(found) = pattern.find_at(bytes, from) {
            let (start, end) = (found.start(), found.end());
            from = start + 1;
            if !standalone(bytes, start, end)
                || continues_number(bytes, start, end)
                || !(rule.valid)(found.as_bytes())
                || !(rule.context.is_empty() || near_word(bytes, start, rule.context))
            {
                continue;
            }
            findings.push(Finding::new(start, end, category, rule.label));
            from = end;
        }
    }
}

/// One of `words` in the 40 bytes before `start`. ASCII words match in any
/// case and only as whole words (`sin`, not `using`); others as written.
fn near_word(bytes: &[u8], start: usize, words: &[&str]) -> bool {
    let window = bytes[start.saturating_sub(40)..start].to_ascii_lowercase();
    words.iter().any(|word| {
        let word = word.as_bytes();
        if !word.is_ascii() {
            return contains(&window, word);
        }
        let mut from = 0;
        while let Some(at) = find_from(&window, word, from) {
            let end = at + word.len();
            let before = at > 0 && window[at - 1].is_ascii_alphanumeric();
            let after = window.get(end).is_some_and(u8::is_ascii_alphanumeric);
            if !before && !after {
                return true;
            }
            from = at + 1;
        }
        false
    })
}

/// The digits of `raw` as numbers, separators dropped.
fn digit_values(raw: &[u8]) -> Vec<u32> {
    raw.iter()
        .filter(|byte| byte.is_ascii_digit())
        .map(|byte| u32::from(byte - b'0'))
        .collect()
}

// -------------------------------------------------------------- helpers

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Not part of a longer word or number: no letter, digit or `_` on either
/// side, and no decimal point leading into more digits.
fn standalone(bytes: &[u8], start: usize, end: usize) -> bool {
    if start > 0 {
        let before = bytes[start - 1];
        if is_word(before) || (before == b'.' && start > 1 && bytes[start - 2].is_ascii_digit()) {
            return false;
        }
    }
    if let Some(&after) = bytes.get(end) {
        if is_word(after) {
            return false;
        }
        if after == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
            return false;
        }
    }
    true
}

/// A digit group joined by a space or dash on either side: the match is a
/// slice of a longer run of numbers (columns, phone or serial numbers).
fn continues_number(bytes: &[u8], start: usize, end: usize) -> bool {
    let before =
        start >= 2 && matches!(bytes[start - 1], b' ' | b'-') && bytes[start - 2].is_ascii_digit();
    let after = matches!(bytes.get(end), Some(b' ' | b'-'))
        && bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
    before || after
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |index| from + index)
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| from + index)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find_from(haystack, needle, 0).is_some()
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}
