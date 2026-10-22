use super::*;

/// The default categories: what most users run with.
fn all(text: &str) -> Redacted {
    redact(text, &Categories::DEFAULT)
}

fn only(category: Category) -> Categories {
    Categories::only(category)
}

/// Text that must come through untouched by the default categories.
fn assert_clean(text: &str) {
    let redacted = all(text);
    assert_eq!(redacted.text, text, "unexpected redaction");
    assert!(redacted.counts.is_empty());
}

// Credential-shaped values are assembled at run time so the source does not
// trip secret scanners.
fn token(prefix: &str, body: &str, repeat: usize) -> String {
    format!("{prefix}{}", body.repeat(repeat))
}

#[test]
fn every_embedded_gitleaks_rule_loads() {
    let ids = gitleaks::rule_ids();
    // 222 rules, minus the path-only and path-gated ones and `private-key`.
    assert!(ids.len() > 200, "only {} rules loaded", ids.len());
    for expected in [
        "generic-api-key",
        "github-pat",
        "aws-access-token",
        "jwt",
        "kubernetes-secret-yaml",
    ] {
        assert!(ids.contains(&expected), "{expected} missing");
    }
    assert!(!ids.contains(&"private-key"));
    assert!(!ids.contains(&"hashicorp-tf-password"));
}

#[test]
fn translation_escapes_only_literal_braces() {
    assert_eq!(gitleaks::translated(r"a{2,5}b{3}c{1,}"), r"a{2,5}b{3}c{1,}");
    assert_eq!(
        gitleaks::translated(r"^\${(?:[A-Z_]+)}$"),
        r"^\$\{(?:[A-Z_]+)\}$"
    );
    assert_eq!(gitleaks::translated(r"[{}]x"), r"[{}]x");
    assert_eq!(gitleaks::translated(r"[[:alnum:]]{14}"), r"[[:alnum:]]{14}");
    assert_eq!(gitleaks::translated(r"\{\{[ \t]*}}"), r"\{\{[ \t]*\}\}");
}

#[test]
fn gitleaks_rules_redact_known_token_formats() {
    let github = token("ghp_", "a1B2c3D4e5", 4)[..40].to_owned();
    let aws = token("AKIA", "Q3VX7R2LM6TP5KD4", 1);
    let slack = format!(
        "xoxb-{}-{}-{}",
        "1234567890",
        "9876543210",
        token("", "aB3dE6gH9k", 3)[..24].to_owned()
    );
    let stripe = token("sk_live_", "4eC39HqLyjWDarjtT1zdp7dc", 1);
    let jwt = format!(
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.{}.{}",
        "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ",
        "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    );
    for (secret, rule) in [
        (&github, "github-pat"),
        (&aws, "aws-access-token"),
        (&slack, "slack-bot-token"),
        (&stripe, "stripe-access-token"),
        (&jwt, "jwt"),
    ] {
        let text = format!("found {secret} in the log\n");
        let redacted = redact(&text, &only(Category::Credential));
        assert!(
            !redacted.text.contains(secret.as_str()),
            "{rule}: {}",
            redacted.text
        );
        assert!(
            redacted.text.contains("[REDACTED:"),
            "{rule}: {}",
            redacted.text
        );
        assert_eq!(
            redacted.counts.get(&Category::Credential),
            Some(&1),
            "{rule}"
        );
    }
}

#[test]
fn generic_api_key_keeps_the_key_name_and_honours_allowlists() {
    let value = "a8F3kL0qZ7xW2mN9pR4t";
    let redacted = all(&format!("SERVICE_API_KEY={value}\n"));
    assert_eq!(
        redacted.text,
        "SERVICE_API_KEY=[REDACTED:generic-api-key]\n"
    );
    // Low entropy, no digit, or a placeholder: not a secret.
    assert_clean("api_version=2024-01-01\n");
    assert_clean("keyboard: us-international\n");
    assert_clean("token: ${GITHUB_TOKEN}\n");
    assert_clean("monkey=banana\n");
}

#[test]
fn curl_headers_and_url_passwords_are_redacted() {
    let bearer = token("", "Zx9Qw8Er7Ty6", 3);
    let redacted = all(&format!(
        "curl -H \"Authorization: Bearer {bearer}\" https://api.example.com\n"
    ));
    assert!(!redacted.text.contains(&bearer), "{}", redacted.text);

    let redacted = all("DATABASE_URL=postgres://app:S3cr3t-Pa55@db.internal:5432/app\n");
    assert!(!redacted.text.contains("S3cr3t-Pa55"), "{}", redacted.text);
    assert!(redacted.text.contains("postgres://app:"));
    assert!(redacted.text.contains("@db.internal:5432/app"));
}

#[test]
fn secret_assignments_cover_env_json_yaml_and_chinese_keys() {
    let redacted = all("export DB_PASSWORD=hunter2!x\n\"client_secret\": \"abc def ghi\",\npassword: 'p@ss w0rd'\n数据库密码：Xy7Kq9\n");
    assert_eq!(
        redacted.text,
        "export DB_PASSWORD=[REDACTED:credential]\n\"client_secret\": \"[REDACTED:credential]\",\npassword: '[REDACTED:credential]'\n数据库密码：[REDACTED:credential]\n"
    );
    assert_eq!(redacted.counts.get(&Category::Credential), Some(&4));
}

#[test]
fn secret_assignment_metadata_and_placeholders_stay() {
    assert_clean("max_tokens: 4096\n");
    assert_clean("token_type: Bearer\n");
    assert_clean("password_file=/run/secrets/db\n");
    assert_clean("secret_name: prod-db\n");
    assert_clean("PASSWORD=\n");
    assert_clean("password: ********\n");
    assert_clean("password: <your password>\n");
    assert_clean("Failed password for root from 10.0.0.1 port 22 ssh2\n");
    assert_clean("PasswordAuthentication no\n");
    assert_clean("Author: Jane Doe <jane@example.com>\n");
}

#[test]
fn shadow_hashes_are_redacted() {
    let redacted = all("root:$6$Zq0rS1t2$Kx9mVb3nQ8wLp5rT7yU1iO2aS4dF6gH8jK0lZ3xC5vB7nM9qW1eR3tY5uI7oP9aS:19700:0:99999:7:::\n");
    assert_eq!(
        redacted.text,
        "root:[REDACTED:password-hash]:19700:0:99999:7:::\n"
    );
}

#[test]
fn pem_private_keys_keep_their_frame_and_cover_truncation() {
    let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB\n";
    let text = format!("ok\n-----BEGIN PRIVATE KEY-----\n{body}-----END PRIVATE KEY-----\ndone\n");
    let redacted = all(&text);
    assert_eq!(
        redacted.text,
        "ok\n-----BEGIN PRIVATE KEY-----[REDACTED:private_key]-----END PRIVATE KEY-----\ndone\n"
    );
    assert_eq!(redacted.counts.get(&Category::PrivateKey), Some(&1));

    // Cut off by the output limit: the rest of the armor is hidden.
    let cut = format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{body}");
    let redacted = all(&cut);
    assert_eq!(
        redacted.text,
        "-----BEGIN OPENSSH PRIVATE KEY-----[REDACTED:private_key]\n"
    );

    // JSON-escaped, as in a service account file.
    let json = "{\"private_key\": \"-----BEGIN PRIVATE KEY-----\\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\\n-----END PRIVATE KEY-----\\n\"}";
    let redacted = all(json);
    assert!(!redacted.text.contains("MIIEvQ"), "{}", redacted.text);
    assert!(redacted.text.contains("-----BEGIN PRIVATE KEY-----"));

    // A BEGIN string in source code without armor after it hides nothing.
    assert_clean("const HEADER = \"-----BEGIN RSA PRIVATE KEY-----\";\nfn parse() {}\n");
}

#[test]
fn public_keys_keep_algorithm_and_comment() {
    let data = format!("AAAAC3NzaC1lZDI1NTE5AAAAI{}", "Hx7Kq2Lm9".repeat(4));
    let redacted = all(&format!("ssh-ed25519 {data} jh@laptop\n"));
    assert_eq!(
        redacted.text,
        "ssh-ed25519 [REDACTED:public_key] jh@laptop\n"
    );
    assert_eq!(redacted.counts.get(&Category::PublicKey), Some(&1));

    let cert = "-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIEAgAAuTANBgkqhkiG9w0BAQUFADBaMQsw\n-----END CERTIFICATE-----\n";
    let redacted = all(cert);
    assert_eq!(
        redacted.text,
        "-----BEGIN CERTIFICATE-----[REDACTED:public_key]-----END CERTIFICATE-----\n"
    );
    // Off: public keys pass through.
    let off = Categories {
        public_key: false,
        ..Categories::DEFAULT
    };
    assert_eq!(redact(cert, &off).text, cert);
}

#[test]
fn card_numbers_need_luhn_and_an_issuer() {
    let redacted = all("visa 4111 1111 1111 1111, mc 5555-5555-5555-4444, amex 378282246310005.\n");
    assert_eq!(
        redacted.text,
        "visa [REDACTED:bank_card ****1111], mc [REDACTED:bank_card ****4444], amex [REDACTED:bank_card ****0005].\n"
    );
    assert_eq!(redacted.counts.get(&Category::BankCard), Some(&3));
    // Luhn-invalid, no issuer, a millisecond timestamp, part of a longer
    // number, a float, or columns of numbers.
    assert_clean("4111111111111112\n");
    assert_clean("ts=1729425000000\n");
    assert_clean("id 41111111111111110000\n");
    assert_clean("3.4111111111111111\n");
    assert_clean("4111 1111 1111 1111 2222\n");
    assert_clean("sha 9a4111111111111111ff\n");
}

#[test]
fn unionpay_numbers_failing_luhn_need_a_card_word() {
    let number = "6222021234567890123";
    assert_clean(&format!("order {number}\n"));
    let redacted = all(&format!("银行卡号: {number}\n"));
    assert_eq!(redacted.text, "银行卡号: [REDACTED:bank_card ****0123]\n");
}

#[test]
fn ibans_need_the_country_length_and_mod_97() {
    let redacted = all("pay to DE89 3704 0044 0532 0130 00 today\nGB82WEST12345698765432\n");
    assert_eq!(
        redacted.text,
        "pay to [REDACTED:iban] today\n[REDACTED:iban]\n"
    );
    assert_clean("DE89 3704 0044 0532 0130 01\n");
}

#[test]
fn chinese_resident_ids_need_a_valid_date_and_check_digit() {
    let redacted = all("身份证 11010519491231002X 已登记\n");
    assert_eq!(redacted.text, "身份证 [REDACTED:cn_id] 已登记\n");
    assert_eq!(redacted.counts.get(&Category::IdCard), Some(&1));
    assert_clean("110105194912310021\n");
    assert_clean("110105194902300028\n");
}

#[test]
fn categories_switch_detectors_independently() {
    let text = "password=Zq8!x7Lk\n4111111111111111\n";
    let cards_only = redact(text, &only(Category::BankCard));
    assert_eq!(
        cards_only.text,
        "password=Zq8!x7Lk\n[REDACTED:bank_card ****1111]\n"
    );
    let off = AgentOutputFilter {
        enabled: false,
        ..AgentOutputFilter::default()
    };
    assert!(off.active().is_none());
    assert!(AgentOutputFilter::default().active().is_some());
}

#[test]
fn ordinary_output_passes_through() {
    assert_clean(
        "total 48\ndrwxr-xr-x  5 root root 4096 Oct 20 10:00 .\n-rw-r--r--  1 root root 1234 Oct 20 10:00 README.md\n",
    );
    assert_clean(
        "commit 3f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39\nAuthor: Dev <dev@example.com>\n",
    );
    assert_clean("Filesystem     1K-blocks    Used Available Use% Mounted on\n/dev/sda1      102400000 5242880  97157120   6% /\n");
    assert_clean("PID USER PR NI VIRT RES SHR S %CPU\n1234 root 20 0 1048576 524288 65536 S 1.0\n");
    assert_clean("uuid 123e4567-e89b-12d3-a456-426614174000\n");
    assert_clean("用户名：张三，手机号 13800138000\n");
}

#[test]
fn utf8_text_around_findings_survives() {
    let redacted = all("密钥：Zq8x7Lk9 ✓ 完成\n");
    assert_eq!(redacted.text, "密钥：[REDACTED:credential] ✓ 完成\n");
}

#[test]
fn stored_settings_load_leniently() {
    assert_eq!(
        AgentOutputFilter::from_stored(serde_json::json!({})),
        AgentOutputFilter::default()
    );
    let stored = AgentOutputFilter::from_stored(serde_json::json!({
        "enabled": true,
        "categories": { "publicKey": false, "future": true },
        "other": 1,
    }));
    assert!(!stored.categories.public_key);
    assert!(stored.categories.private_key);
    assert_eq!(
        AgentOutputFilter::from_stored(serde_json::json!("garbage")),
        AgentOutputFilter::default()
    );
}

#[test]
fn a_large_output_redacts_quickly() {
    let line = "2026-10-20T10:00:00Z INFO request id=8f14e45f method=GET path=/api/v1/items status=200 bytes=5120 api_key=abc\n";
    let text = line.repeat(256 * 1024 / line.len());
    let started = std::time::Instant::now();
    let _ = all(&text);
    // Warm (rules compiled); generous for debug builds and slow CI runners.
    let started_warm = std::time::Instant::now();
    let _ = all(&text);
    let warm = started_warm.elapsed();
    assert!(
        warm < std::time::Duration::from_secs(if cfg!(debug_assertions) { 5 } else { 1 }),
        "256 KiB took {warm:?} (first run {:?})",
        started.elapsed() - warm
    );
}

// ------------------------------------------------- leak report fixtures

/// An OpenClash `mihomo.yaml` with the shapes the leak report found
/// (all values fake).
const MIHOMO: &str = r#"mixed-port: 7890
bind-address: "*"
external-controller: 0.0.0.0:9090
authentication:
- Clash:a1b2c3d4
proxy-providers:
  机场订阅:
    type: http
    url: https://ping.ytoo.online/pingouo/8f3a9c2d1e4b5f6a7c8d9e0f1a2b3c4d
    interval: 86400
rule-providers:
  cn:
    url: https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/geo/geosite/cn.mrs
proxies:
- name: PP-LAX
  type: vless
  server: 140.235.228.222
  port: 443
  uuid: 0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d
  encryption: mlkem768x25519plus.native.0rtt.QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODkrLz0=
- name: AWS-SG
  type: vless
  server: 18.138.55.187
  uuid: 1b2c3d4e-5f6a-7b8c-9d0e-1f2a3b4c5d6e
  reality-opts:
    public-key: Zq3xL9vK2mN8pR4tW7yB1cE5fH6jA0sD3gU9iO2lQ4w
    short-id: 1a2b3c4d5e6f7a8b
- name: Home
  type: wireguard
  server: 192.168.88.1
  ip: 10.66.0.2
  public-key: Ab4Cd8Ef2Gh6Ij0Kl4Mn8Op2Qr6St0Uv4Wx8Yz2Ab4=
"#;

#[test]
fn the_leak_report_values_are_redacted_by_default() {
    let redacted = all(MIHOMO);
    for leaked in [
        "Clash:a1b2c3d4",
        "8f3a9c2d1e4b5f6a7c8d9e0f1a2b3c4d",
        "0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d",
        "1b2c3d4e-5f6a-7b8c-9d0e-1f2a3b4c5d6e",
        "1a2b3c4d5e6f7a8b",
    ] {
        assert!(
            !redacted.text.contains(leaked),
            "{leaked} leaked:\n{}",
            redacted.text
        );
    }
    assert!(redacted
        .text
        .contains("authentication:\n- [REDACTED:credential]\n"));
    assert!(redacted
        .text
        .contains("url: https://ping.ytoo.online/pingouo/[REDACTED:subscription-token]\n"));
    assert!(redacted.text.contains("  uuid: [REDACTED:uuid]\n"));
    assert!(redacted
        .text
        .contains("    short-id: [REDACTED:short-id]\n"));
    // What the report calls fine to show: public keys, addresses, public
    // rule-set URLs, ports and names. The long `encryption` value is
    // public key material, left to the high-entropy option.
    for kept in [
        "public-key: Zq3xL9vK2mN8pR4tW7yB1cE5fH6jA0sD3gU9iO2lQ4w",
        "public-key: Ab4Cd8Ef2Gh6Ij0Kl4Mn8Op2Qr6St0Uv4Wx8Yz2Ab4=",
        "server: 140.235.228.222",
        "ip: 10.66.0.2",
        "external-controller: 0.0.0.0:9090",
        "https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/geo/geosite/cn.mrs",
        "encryption: mlkem768x25519plus.native.0rtt.QUJD",
        "- name: PP-LAX",
    ] {
        assert!(
            redacted.text.contains(kept),
            "{kept} hidden:\n{}",
            redacted.text
        );
    }
    assert_eq!(redacted.counts.get(&Category::Credential), Some(&1));
    assert_eq!(redacted.counts.get(&Category::ProxyCredential), Some(&4));

    // Everything on: the long base64 and the public addresses go too.
    let strict = redact(MIHOMO, &Categories::ALL);
    assert!(strict
        .text
        .contains("encryption: mlkem768x25519plus.native.0rtt.[REDACTED:high-entropy]\n"));
    assert!(strict.text.contains("server: [REDACTED:public-ip]\n"));
    assert!(strict.text.contains("server: 192.168.88.1\n"));
}

#[test]
fn secret_lists_flow_lists_and_new_key_names() {
    let redacted = all(
        "authentication:\n  # users\n  - \"admin:Pa55w0rd\"\n  - guest:Zx81yy # tmp\nport: 80\n",
    );
    assert_eq!(
        redacted.text,
        "authentication:\n  # users\n  - \"[REDACTED:credential]\"\n  - [REDACTED:credential] # tmp\nport: 80\n"
    );
    let redacted = all("{\"authentication\": [\"a:Secr3tx\", \"b:Other9y\"], \"port\": 1}\n");
    assert_eq!(
        redacted.text,
        "{\"authentication\": [\"[REDACTED:credential]\", \"[REDACTED:credential]\"], \"port\": 1}\n"
    );
    let redacted =
        all("PresharedKey = Kq9xLm2Vb7Ns4Rt1Wy8Zc3Df6Gh0Jk5\npsk: hunter2x\nauth: s3cret-v4l\n");
    assert!(!redacted.text.contains("Kq9xLm2Vb7"), "{}", redacted.text);
    assert!(redacted
        .text
        .ends_with("psk: [REDACTED:credential]\nauth: [REDACTED:credential]\n"));
    // A list of mappings, metadata lists and auth scheme names stay.
    assert_clean("secrets:\n- name: db\n  mountPath: /run/secrets\n");
    assert_clean("skip-auth-prefixes:\n- 127.0.0.1/8\n");
    assert_clean("auth: basic\nauth: none\nauthor: Jane\n");
    assert_clean("token_types: [access, refresh]\n");
}

#[test]
fn url_query_secrets_and_basic_credentials() {
    let redacted =
        all("GET https://api.example.com/v1/items?page=2&access_token=Zq8x7Lk9Wm2&sort=asc\n");
    assert_eq!(
        redacted.text,
        "GET https://api.example.com/v1/items?page=2&access_token=[REDACTED:url-secret]&sort=asc\n"
    );
    // `Basic` with base64 of user:password, also outside a header.
    let redacted = all("sent Basic YWRtaW46czNjcmV0UGFzcw== to proxy\n");
    assert_eq!(redacted.text, "sent Basic [REDACTED:basic-auth] to proxy\n");
    assert_clean("uses Basic authentication over TLS\n");
    assert_clean("https://example.com/search?q=rust&page=2\n");
}

#[test]
fn proxy_share_links_and_xray_ids() {
    let uuid = "2c3d4e5f-6a7b-8c9d-0e1f-2a3b4c5d6e7f";
    let redacted = all(&format!(
        "vless://{uuid}@193.177.220.211:443?security=reality&pbk=Zq3xL9vK2mN8&sid=9f8e7d6c5b4a3210#GH-SYD\n"
    ));
    assert_eq!(
        redacted.text,
        "vless://[REDACTED:share-link]@193.177.220.211:443?security=reality&pbk=Zq3xL9vK2mN8&sid=[REDACTED:share-link]#GH-SYD\n"
    );
    let redacted = all("vmess://eyJhZGQiOiIxLjIuMy40IiwiaWQiOiJhYmMifQ==\nss://YWVzLTI1Ni1nY206cGFzcw@1.2.3.4:8388#node\n");
    assert_eq!(
        redacted.text,
        "vmess://[REDACTED:share-link]\nss://[REDACTED:share-link]@1.2.3.4:8388#node\n"
    );
    // Xray users, only in output that looks like a proxy config.
    let xray = format!("{{\"outbounds\": [{{\"protocol\": \"vless\", \"settings\": {{\"vnext\": [{{\"users\": [{{\"id\": \"{uuid}\"}}]}}]}}}}]}}\n");
    assert!(!all(&xray).text.contains(uuid), "{}", all(&xray).text);
    assert_clean(&format!("{{\"id\": \"{uuid}\", \"status\": \"done\"}}\n"));
    assert_clean("\"decryption\": \"none\",\n");
}

#[test]
fn proxy_rules_leave_system_uuids_alone() {
    assert_clean("UUID=7d1e2a5c-9b3f-4e8a-a1c2-5f6e7d8c9b0a / ext4 defaults 0 1\n");
    assert_clean("/dev/sda1: UUID=\"7d1e2a5c-9b3f-4e8a-a1c2-5f6e7d8c9b0a\" TYPE=\"ext4\"\n");
    assert_clean("connection.uuid:                        7d1e2a5c-9b3f-4e8a-a1c2-5f6e7d8c9b0a\n");
    assert_clean("UUID:            7d1e2a5c-9b3f-4e8a-a1c2-5f6e7d8c9b0a\n");
    // A token-like path outside a proxy config is only the aggressive
    // option's business.
    assert_clean(
        "https://github.com/rust-lang/rust/commit/3f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39\n",
    );
}

#[test]
fn aggressive_categories_each_switch_on_alone() {
    let cases: [(Category, &str, &str); 7] = [
        (
            Category::UrlToken,
            "see https://example.com/share/Ab3dE6gH9kLm2Np5Qr8tUv?x=1\n",
            "see https://example.com/share/[REDACTED:url-token]?x=1\n",
        ),
        (
            Category::HighEntropy,
            "blob 3f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39 end\n",
            "blob [REDACTED:high-entropy] end\n",
        ),
        (
            Category::Uuid,
            "request 123e4567-e89b-12d3-a456-426614174000 done\n",
            "request [REDACTED:uuid] done\n",
        ),
        (
            Category::Email,
            "Author: Jane Doe <jane.doe@example.com>\n",
            "Author: Jane Doe <[REDACTED:email]>\n",
        ),
        (
            Category::Phone,
            "手机号 13800138000，备用 +86 139-1234-5678\n",
            "手机号 [REDACTED:phone]，备用 [REDACTED:phone]\n",
        ),
        (
            Category::PublicIp,
            "from 8.8.8.8 and 2001:4860:4860::8888 via 10.0.0.1, 100.64.0.1, fe80::1\n",
            "from [REDACTED:public-ip] and [REDACTED:public-ip] via 10.0.0.1, 100.64.0.1, fe80::1\n",
        ),
        (
            Category::ProxyCredential,
            "uuid: 123e4567-e89b-12d3-a456-426614174000\n",
            "uuid: [REDACTED:uuid]\n",
        ),
    ];
    for (category, text, expected) in cases {
        assert_eq!(redact(text, &only(category)).text, expected, "{category:?}");
        if category != Category::ProxyCredential {
            assert_clean(text);
        }
    }
}

#[test]
fn aggressive_categories_still_skip_obvious_non_matches() {
    let strict = |text: &str| redact(text, &Categories::ALL).text;
    // Low-entropy runs, version numbers with more parts, times, MACs,
    // longer numbers and words.
    for text in [
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        "version 1.2.3.4.5\n",
        "at 10:00:00 mac aa:bb:cc:dd:ee:ff\n",
        "order 138001380001234\n",
        "/usr/lib/x86_64-linux-gnu/libstdc++.so.6\n",
    ] {
        assert_eq!(strict(text), text);
    }
}

#[test]
fn custom_keys_hide_their_values_in_any_case() {
    let keys = ["router_pin".to_owned(), "x-team-code".to_owned()];
    let redacted = redact_with(
        "ROUTER_PIN=8812\n\"x-team-code\": \"alpha\"\nrouter_pins: 3\n",
        &Categories::DEFAULT,
        &keys,
    );
    assert_eq!(
        redacted.text,
        "ROUTER_PIN=[REDACTED:custom-key]\n\"x-team-code\": \"[REDACTED:custom-key]\"\nrouter_pins: 3\n"
    );
    assert_eq!(redacted.counts.get(&Category::Credential), Some(&2));
    // Custom keys work with every category off.
    let filter = AgentOutputFilter {
        enabled: true,
        categories: Categories::only(Category::Credential),
        custom_keys: keys.to_vec(),
    };
    let none = AgentOutputFilter {
        categories: Categories {
            credential: false,
            ..filter.categories
        },
        ..filter.clone()
    };
    assert!(none.active().is_some());
    assert_eq!(
        none.redact("router_pin: 1234\n").text,
        "router_pin: [REDACTED:custom-key]\n"
    );
}

#[test]
fn updates_merge_missing_fields_and_validate_custom_keys() {
    let current = AgentOutputFilter {
        enabled: true,
        categories: Categories {
            uuid: true,
            ..Categories::DEFAULT
        },
        custom_keys: vec!["pin".to_owned()],
    };
    // A client that only knows the first five categories.
    let old_client: AgentOutputFilterUpdate = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "categories": {
            "privateKey": true, "publicKey": false, "credential": true,
            "bankCard": true, "idCard": true,
        },
    }))
    .unwrap();
    let next = old_client.apply(&current).unwrap();
    assert!(!next.categories.public_key);
    assert!(next.categories.uuid, "an old client reset a newer switch");
    assert_eq!(next.custom_keys, ["pin"]);

    let keys = |keys: serde_json::Value| {
        serde_json::from_value::<AgentOutputFilterUpdate>(serde_json::json!({
            "enabled": true, "categories": {}, "customKeys": keys,
        }))
        .unwrap()
        .apply(&current)
    };
    assert_eq!(
        keys(serde_json::json!(["a", "A", "b.c"]))
            .unwrap()
            .custom_keys,
        ["a", "b.c"]
    );
    assert!(keys(serde_json::json!(["has space"])).is_err());
    assert!(keys(serde_json::json!([""])).is_err());
    assert!(keys(serde_json::json!(vec!["k"; CUSTOM_KEYS_LIMIT + 1])).is_err());
    assert!(
        serde_json::from_value::<AgentOutputFilterUpdate>(serde_json::json!({
            "enabled": true, "categories": { "future": true },
        }))
        .is_err()
    );
}

#[test]
fn stored_settings_default_new_categories_by_kind() {
    // Saved before the new categories existed.
    let stored = AgentOutputFilter::from_stored(serde_json::json!({
        "enabled": true,
        "categories": {
            "privateKey": true, "publicKey": false, "credential": true,
            "bankCard": true, "idCard": true,
        },
    }));
    assert!(!stored.categories.public_key);
    assert!(stored.categories.proxy_credential);
    assert!(!stored.categories.url_token && !stored.categories.email);
    let stored = AgentOutputFilter::from_stored(serde_json::json!({
        "enabled": true,
        "categories": { "email": true, "uuid": "yes" },
        "customKeys": ["ok", 3, "bad key"],
    }));
    assert!(stored.categories.email);
    assert!(!stored.categories.uuid);
    assert_eq!(stored.custom_keys, ["ok"]);
}

#[test]
fn every_category_on_stays_fast() {
    let line = "2026-10-20T10:00:00Z INFO GET https://api.example.com/v1/items/Ab3dE6gH9kLm2Np5Qr8tUv from 8.8.8.8 uuid: 123e4567-e89b-12d3-a456-426614174000 user=jane@example.com\n";
    let text = format!("proxies:\n{}", line.repeat(256 * 1024 / line.len()));
    let filter = AgentOutputFilter {
        enabled: true,
        categories: Categories::ALL,
        custom_keys: vec!["pin".to_owned(), "code".to_owned()],
    };
    let _ = filter.redact(&text);
    let started = std::time::Instant::now();
    let _ = filter.redact(&text);
    let warm = started.elapsed();
    assert!(
        warm < std::time::Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 2 }),
        "256 KiB with every category took {warm:?}"
    );
}
