//! Pairing keys and transport encryption.
//!
//! - [`PairingKeys`]: the daemon's static X25519 / ML-KEM-768 keys that
//!   clients pin at pairing; [`PairingKeyStore`] is the shared, reloading
//!   handle the handshake and device pairing both read.
//! - The address-only pairing link and its QR renderings.
//! - Transport v2 (`docs/transport-v2.md`): [`handshake`] (key agreement and
//!   key schedule), [`channel`] (sealed records and WebSocket frames) and
//!   [`envelope`] (REST record streams and the inner request/response).
pub(crate) mod channel;
pub(crate) mod envelope;
pub(crate) mod handshake;
pub(crate) mod pairing_browser;
#[cfg(test)]
mod vector_tests;

pub(crate) use channel::TransportCryptoError;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ml_kem::{DecapsulationKey, Kem, KeyExport, MlKem768};
use qrcodegen::{QrCode, QrCodeEcc};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use crate::config::{Config, PairingEncryption};
use crate::error::AppError;
use crate::secure_fs::{self, FileStamp};

const PAIRING_VERSION: u8 = 1;
/// Version 2 links carry only the server address; the transport key is
/// delivered (and authenticated) by device pairing v3.
const PAIRING_LINK_VERSION: u8 = 2;
const PAIRING_KEYS_FILE: &str = "pairing_keys.json";

pub(crate) type MlKemSecret = DecapsulationKey<MlKem768>;

/// The FIPS 203 expanded decapsulation key (2400 bytes), the form
/// `pairing_keys.json` has always stored (pqcrypto-mlkem wrote it), so older
/// files load unchanged and older builds still read new ones. It holds no
/// seed, which is why existing keys cannot move to ml-kem's 64-byte seeds.
#[allow(deprecated)]
fn ml_kem_secret_to_expanded(secret: &MlKemSecret) -> zeroize::Zeroizing<Vec<u8>> {
    use ml_kem::ExpandedKeyEncoding;
    let mut expanded = secret.to_expanded_bytes();
    let bytes = zeroize::Zeroizing::new(expanded.to_vec());
    zeroize::Zeroize::zeroize(expanded.as_mut_slice());
    bytes
}

/// Parses an expanded key; `None` for a wrong length, or when its embedded
/// public key does not match the public key hash stored with it.
#[allow(deprecated)]
pub(crate) fn ml_kem_secret_from_expanded(bytes: &[u8]) -> Option<MlKemSecret> {
    use ml_kem::ExpandedKeyEncoding;
    let mut expanded = bytes.try_into().ok()?;
    let secret = MlKemSecret::from_expanded_bytes(&expanded).ok();
    zeroize::Zeroize::zeroize(expanded.as_mut_slice());
    secret
}

#[derive(Clone)]
pub struct PairingKeys {
    x25519_secret: Arc<X25519Secret>,
    x25519_public: [u8; 32],
    ml_kem_secret: Arc<MlKemSecret>,
    ml_kem_public: Vec<u8>,
}

impl PairingKeys {
    pub fn generate() -> Self {
        let x25519_secret = X25519Secret::random_from_rng(OsRng);
        let x25519_public = X25519PublicKey::from(&x25519_secret).to_bytes();
        let (ml_kem_secret, ml_kem_public) = MlKem768::generate_keypair();

        Self {
            x25519_secret: Arc::new(x25519_secret),
            x25519_public,
            ml_kem_secret: Arc::new(ml_kem_secret),
            ml_kem_public: ml_kem_public.to_bytes().to_vec(),
        }
    }

    pub async fn load_or_generate(data_dir: &Path) -> Result<Self, AppError> {
        let path = data_dir.join(PAIRING_KEYS_FILE);
        match tokio::fs::read_to_string(&path).await {
            Ok(raw) => Self::from_persisted_json(&raw, "pairing keys file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let keys = Self::generate();
                keys.persist(&path).await?;
                Ok(keys)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn reset(data_dir: &Path) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(data_dir).await?;
        let path = data_dir.join(PAIRING_KEYS_FILE);
        let keys = Self::generate();
        keys.persist(&path).await?;
        Ok(keys)
    }

    /// Raw static public key for the configured protocol: what the v2
    /// handshake uses and device pairing v3 delivers; empty for `none`.
    pub(crate) fn transport_public_key(&self, encryption: PairingEncryption) -> &[u8] {
        match encryption {
            PairingEncryption::None => &[],
            PairingEncryption::X25519 => self.static_public(EncryptionProtocol::X25519),
            PairingEncryption::MlKem768 => self.static_public(EncryptionProtocol::MlKem768),
        }
    }

    /// Writes the keys owner-only from the start (no window in which the
    /// secrets follow the umask), atomically and durably: the temp file and
    /// the directory entry are synced, and a failed write leaves no temp.
    async fn persist(&self, path: &Path) -> Result<(), AppError> {
        let persisted = PersistedPairingKeys {
            version: PAIRING_VERSION,
            x25519_secret: encode_b64(self.x25519_secret.to_bytes().as_slice()),
            ml_kem_public: encode_b64(&self.ml_kem_public),
            ml_kem_secret: encode_b64(&ml_kem_secret_to_expanded(&self.ml_kem_secret)),
        };
        let json = zeroize::Zeroizing::new(serde_json::to_vec_pretty(&persisted)?);
        secure_fs::write_owner_only_atomic_async(path, json, secure_fs::ParentDir::Create).await?;
        Ok(())
    }

    fn from_persisted_json(raw: &str, label: &str) -> Result<Self, AppError> {
        let persisted: PersistedPairingKeys = serde_json::from_str(raw)
            .map_err(|error| AppError::InvalidRequest(format!("invalid {label}: {error}")))?;
        if persisted.version != PAIRING_VERSION {
            return Err(AppError::InvalidRequest(format!(
                "{label} version {} is not supported",
                persisted.version
            )));
        }
        let x25519_secret = X25519Secret::from(decode_fixed_32(
            &persisted.x25519_secret,
            "x25519 pairing secret",
        )?);
        let x25519_public = X25519PublicKey::from(&x25519_secret).to_bytes();
        let ml_kem_secret_bytes = zeroize::Zeroizing::new(decode_b64(
            &persisted.ml_kem_secret,
            "ml-kem-768 pairing secret",
        )?);
        let ml_kem_secret = ml_kem_secret_from_expanded(&ml_kem_secret_bytes).ok_or_else(|| {
            AppError::InvalidRequest("ml-kem-768 pairing secret is invalid".to_owned())
        })?;
        let ml_kem_public = decode_b64(&persisted.ml_kem_public, "ml-kem-768 pairing public key")?;
        // Clients pin the stored public key; it must be the secret's own.
        if ml_kem_secret.encapsulation_key().to_bytes().as_slice() != ml_kem_public.as_slice() {
            return Err(AppError::InvalidRequest(
                "ml-kem-768 pairing public key is invalid".to_owned(),
            ));
        }

        Ok(Self {
            x25519_secret: Arc::new(x25519_secret),
            x25519_public,
            ml_kem_secret: Arc::new(ml_kem_secret),
            ml_kem_public,
        })
    }
}

struct KeyStoreState {
    path: PathBuf,
    keys: Arc<PairingKeys>,
    /// Version of `pairing_keys.json` the keys were read from.
    stamp: FileStamp,
}

/// The daemon's single source of pairing keys: the v2 handshake (WebSocket
/// and REST) and device pairing v3 both read [`PairingKeyStore::current`], so
/// a TUI reset of `pairing_keys.json` switches them together without a
/// restart (same mtime-reload model as `devices.json`).
#[derive(Clone)]
pub(crate) struct PairingKeyStore {
    inner: Arc<Mutex<KeyStoreState>>,
}

impl PairingKeyStore {
    pub(crate) async fn load(data_dir: &Path) -> Result<Self, AppError> {
        let keys = PairingKeys::load_or_generate(data_dir).await?;
        let path = data_dir.join(PAIRING_KEYS_FILE);
        let stamp = FileStamp::read(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(KeyStoreState {
                path,
                keys: Arc::new(keys),
                stamp,
            })),
        })
    }

    /// Keys for the next handshake or pairing request. Every call stats the
    /// file (one syscall, far cheaper than the key agreement it precedes, and
    /// what `devices.json` already does per authenticated request), so there
    /// is no window in which a stale key is served after a reset. The file is
    /// re-read only when it changed. A missing or invalid file fails closed:
    /// no handshake or pairing succeeds until it is valid again (a daemon
    /// restart regenerates a missing file).
    pub(crate) fn current(&self) -> Result<Arc<PairingKeys>, AppError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| AppError::InvalidRequest("pairing keys are unavailable".to_owned()))?;
        let stamp = FileStamp::read(&state.path)?;
        if stamp != state.stamp {
            // A replacement between the stat and the read only causes one
            // more (identical) reload on the next call.
            let raw = std::fs::read_to_string(&state.path)?;
            state.keys = Arc::new(PairingKeys::from_persisted_json(&raw, "pairing keys file")?);
            state.stamp = stamp;
            tracing::info!("pairing keys file changed; reloaded transport keys");
        }
        Ok(state.keys.clone())
    }
}

/// Short, human-comparable identity of a transport public key:
/// `upper-hex(SHA256(key))[0..16]` grouped `XXXX-XXXX-XXXX-XXXX`. `None` for
/// the empty key of `pairing_encryption = "none"`.
pub(crate) fn transport_fingerprint(public_key: &[u8]) -> Option<String> {
    if public_key.is_empty() {
        return None;
    }
    let hex = Sha256::digest(public_key)[..8]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    Some(format!(
        "{}-{}-{}-{}",
        &hex[..4],
        &hex[4..8],
        &hex[8..12],
        &hex[12..]
    ))
}

/// The address-only pairing link (version 2). It carries no key and no
/// credential: importing it fills the server address and starts device
/// verification, which delivers the transport key authenticated by the
/// verification code.
/// `server.advertise_url`, when set, replaces the guessed address.
pub(crate) fn pairing_link_json(config: &Config, port: u16) -> Result<String, AppError> {
    let server_url = match &config.deployment.advertise_url {
        Some(url) => url.clone(),
        None => format!("http://{}:{port}", pairing_advertise_host(&config.host)),
    };
    Ok(serde_json::to_string(&PairingLinkPayload {
        kind: "todex-pairing-link",
        version: PAIRING_LINK_VERSION,
        server_url,
    })?)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedPairingKeys {
    version: u8,
    x25519_secret: String,
    ml_kem_public: String,
    ml_kem_secret: String,
}

fn pairing_advertise_host(config_host: &str) -> String {
    let host = config_host.trim();
    match host.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => crate::listen_addrs::default_route_ipv4()
            .map_or_else(|| "127.0.0.1".to_owned(), |ip| ip.to_string()),
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(_) => host.to_owned(),
        Err(_) => host.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncryptionProtocol {
    X25519,
    MlKem768,
}

impl EncryptionProtocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "x25519" => Some(Self::X25519),
            "ml-kem-768" | "mlkem768" => Some(Self::MlKem768),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::X25519 => "x25519",
            Self::MlKem768 => "ml-kem-768",
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingLinkPayload {
    kind: &'static str,
    version: u8,
    server_url: String,
}

pub(crate) fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        if raw_key == key {
            Some(percent_decode(raw_value))
        } else {
            None
        }
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'%' if idx + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2]))
                {
                    output.push(high * 16 + low);
                    idx += 3;
                    continue;
                }
            }
            b'+' => {
                output.push(b' ');
                idx += 1;
                continue;
            }
            byte => {
                output.push(byte);
                idx += 1;
                continue;
            }
        }
        output.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&output).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn encode_b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn decode_b64(value: &str, label: &str) -> Result<Vec<u8>, AppError> {
    URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .map_err(|_| AppError::InvalidRequest(format!("invalid base64 for {label}")))
}

fn decode_fixed_32(value: &str, label: &str) -> Result<[u8; 32], AppError> {
    let bytes = decode_b64(value, label)?;
    bytes
        .try_into()
        .map_err(|_| AppError::InvalidRequest(format!("{label} must be 32 bytes")))
}

/// Render locally generated QR geometry only. Payload text never becomes HTML,
/// a script literal, a URL, or an attribute value.
pub(crate) fn render_pairing_qr_browser_html(payload: &str) -> Result<String, AppError> {
    use std::fmt::Write;
    let qr = QrCode::encode_text(payload, QrCodeEcc::Low)
        .map_err(|_| AppError::InvalidRequest("pairing payload is too large for QR".to_owned()))?;
    let modules = qr.size() + 8;
    let pixels = modules * 8;
    let mut path = String::new();
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                write!(&mut path, "M{} {}h1v1h-1z", x + 4, y + 4).expect("writing to String");
            }
        }
    }
    let script_hash = base64::engine::general_purpose::STANDARD
        .encode(Sha256::digest(PAIRING_BROWSER_SCRIPT.as_bytes()));
    Ok(format!(
        r##"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light"><meta name="referrer" content="no-referrer"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'sha256-{script_hash}'; base-uri 'none'; form-action 'none'; object-src 'none'; connect-src 'none'"><title>TodeX 配对</title><style>{style}</style></head><body><main><h1>TodeX 配对</h1><p>在客户端扫描此二维码填入服务器地址，然后在 TUI 中核对验证码。</p><div class="qr-window"><div class="qr-frame"><svg xmlns="http://www.w3.org/2000/svg" role="img" aria-label="配对二维码" width="{pixels}" height="{pixels}" viewBox="0 0 {modules} {modules}" shape-rendering="crispEdges"><rect width="{modules}" height="{modules}" fill="#ffffff"/><path fill="#000000" d="{path}"/></svg></div></div><p class="hint">二维码只包含服务器地址；已完整保留白色边缘。</p></main><script>{script}</script></body></html>"##,
        style = PAIRING_BROWSER_STYLE,
        script = PAIRING_BROWSER_SCRIPT,
    ))
}

const PAIRING_BROWSER_STYLE: &str = r#"
* { box-sizing: border-box; }
html { background: #f4f6f8; color: #172129; font-family: system-ui, sans-serif; color-scheme: light; }
body { margin: 0; padding: 24px; min-height: 100vh; display: grid; place-items: center; }
main { text-align: center; max-width: 100%; }
h1 { margin: 0 0 8px; font-size: 24px; }
p { margin: 8px 0 18px; }
.qr-window { max-width: 100%; overflow: auto; }
.qr-frame { width: fit-content; margin: auto; background: #fff; }
svg { display: block; }
.hint { font-size: 13px; color: #52606b; margin-bottom: 0; }
"#;

const PAIRING_BROWSER_SCRIPT: &str = r#"
'use strict';
const svg = document.querySelector('.qr-frame svg');
function fit() {
  const modules = svg.viewBox.baseVal.width;
  const availableWidth = Math.max(1, document.documentElement.clientWidth - 48);
  const availableHeight = Math.max(1, window.innerHeight - 180);
  const scale = Math.max(1, Math.min(8, Math.floor(availableWidth / modules), Math.floor(availableHeight / modules)));
  svg.setAttribute('width', String(modules * scale));
  svg.setAttribute('height', String(modules * scale));
}
window.addEventListener('resize', fit);
fit();
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QrRenderMode {
    HalfBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenderedQrText {
    pub(crate) text: String,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub(crate) fn render_qr_text_for_bounds(
    payload: &str,
    max_width: u16,
    max_height: u16,
) -> Result<RenderedQrText, AppError> {
    let qr = QrCode::encode_text(payload, QrCodeEcc::Low)
        .map_err(|_| AppError::InvalidRequest("pairing payload is too large for QR".to_owned()))?;
    // A QR symbol needs a four-module quiet zone on every side. If it does
    // not fit, report its real size so the TUI can ask for a larger terminal.
    let candidates = [(QrRenderMode::HalfBlock, 4)];
    let mut best = None;
    for (mode, border) in candidates {
        let rendered = render_qr_with_mode(&qr, mode, border);
        if rendered.width <= max_width && rendered.height <= max_height {
            return Ok(rendered);
        }
        best = match best {
            Some(current) if qr_area(&current) <= qr_area(&rendered) => Some(current),
            _ => Some(rendered),
        };
    }

    Ok(best.expect("QR render candidates must not be empty"))
}

#[cfg(test)]
fn render_qr_text(payload: &str) -> Result<String, AppError> {
    let qr = QrCode::encode_text(payload, QrCodeEcc::Low)
        .map_err(|_| AppError::InvalidRequest("pairing payload is too large for QR".to_owned()))?;
    Ok(render_qr_with_mode(&qr, QrRenderMode::HalfBlock, 4).text)
}

fn render_qr_with_mode(qr: &QrCode, mode: QrRenderMode, border: i32) -> RenderedQrText {
    let text = match mode {
        QrRenderMode::HalfBlock => render_qr_half_block(qr, border),
    };
    let width = text
        .lines()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let height = text.lines().count() as u16;

    RenderedQrText {
        text,
        width,
        height,
    }
}

fn render_qr_half_block(qr: &QrCode, border: i32) -> String {
    let size = qr.size();
    let min = -border;
    let max = size + border;
    let mut lines = Vec::new();
    let mut y = min;
    while y < max {
        let mut line = String::new();
        for x in min..max {
            let top = qr.get_module(x, y);
            let bottom = y + 1 < max && qr.get_module(x, y + 1);
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
        y += 2;
    }
    lines.join("\n")
}

fn qr_area(rendered: &RenderedQrText) -> u32 {
    rendered.width as u32 * rendered.height as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use std::path::PathBuf;

    #[test]
    fn browser_pairing_page_is_self_contained_and_escapes_payload_by_geometry() {
        let payload = "synthetic-</script><img src=malicious onerror=alert(1)>".to_owned();
        let html = render_pairing_qr_browser_html(&payload).unwrap();
        let size = QrCode::encode_text(&payload, QrCodeEcc::Low)
            .unwrap()
            .size()
            + 8;
        assert!(!html.contains(&payload));
        assert!(!html.contains("onerror="));
        assert!(html.contains(&format!("viewBox=\"0 0 {size} {size}\"")));
        assert!(html.contains(&format!("width=\"{}\" height=\"{}\"", size * 8, size * 8)));
        assert!(html.contains("shape-rendering=\"crispEdges\""));
        assert!(html.contains("connect-src 'none'"));
        assert!(html.contains("default-src 'none'"));
        assert_eq!(html.matches("<svg ").count(), 1);
        assert!(!html.contains("<button"));
        let hash = base64::engine::general_purpose::STANDARD
            .encode(Sha256::digest(PAIRING_BROWSER_SCRIPT.as_bytes()));
        assert!(html.contains(&format!("script-src 'sha256-{hash}'")));
    }

    #[test]
    fn pairing_link_carries_only_the_server_address() {
        let config = test_config();
        let link = pairing_link_json(&config, 7345).unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();
        // No key, protocol or credential: device pairing v3 delivers the
        // transport key authenticated by the verification code.
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "todex-pairing-link",
                "version": 2,
                "serverUrl": "http://127.0.0.1:7345",
            })
        );
        let qr = render_qr_text(&link).unwrap();
        let max_width = qr.lines().map(|line| line.chars().count()).max().unwrap();
        assert!(
            max_width <= 80,
            "pairing QR should fit common terminal widths, got {max_width}"
        );
    }

    #[test]
    fn transport_public_key_returns_only_the_selected_public_material() {
        let keys = PairingKeys::generate();

        assert!(keys
            .transport_public_key(PairingEncryption::None)
            .is_empty());
        assert_eq!(
            keys.transport_public_key(PairingEncryption::X25519),
            keys.x25519_public.as_slice()
        );
        assert_eq!(
            keys.transport_public_key(PairingEncryption::MlKem768),
            keys.ml_kem_public.as_slice()
        );
        assert_eq!(
            keys.transport_public_key(PairingEncryption::MlKem768).len(),
            1184
        );
        assert_ne!(
            keys.transport_public_key(PairingEncryption::X25519),
            keys.x25519_secret.to_bytes().as_slice()
        );
    }

    #[test]
    fn transport_fingerprint_is_grouped_upper_hex_of_the_key_hash() {
        assert_eq!(transport_fingerprint(&[]), None);
        // SHA256("abc") = ba7816bf8f01cfea...
        assert_eq!(
            transport_fingerprint(b"abc").as_deref(),
            Some("BA78-16BF-8F01-CFEA")
        );
    }

    #[test]
    fn pairing_qr_renderer_uses_block_cells_instead_of_braille_dots() {
        let payload = pairing_link_json(&test_config(), 7345).unwrap();
        let rendered = render_qr_text_for_bounds(&payload, 76, 10).unwrap();

        assert!(
            rendered.width <= 76,
            "pairing QR should fit terminal content width, got {}",
            rendered.width
        );
        assert!(
            rendered.height > 10,
            "block-cell pairing QR should report that this terminal height is too short"
        );
        assert!(
            !rendered
                .text
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "pairing QR must not use Braille dot cells"
        );
    }

    #[test]
    fn pairing_link_replaces_unspecified_bind_host_with_reachable_advertise_host() {
        let mut config = test_config();
        config.host = "0.0.0.0".to_owned();
        let link = pairing_link_json(&config, 7345).unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_ne!(value["serverUrl"], "http://0.0.0.0:7345");
        assert!(value.get("authToken").is_none());
        assert!(value.get("pairingUrl").is_none());
    }

    #[test]
    fn pairing_link_uses_the_advertised_url() {
        let mut config = test_config();
        config.host = "0.0.0.0".to_owned();
        config.deployment.advertise_url = Some("https://todex.example:8443".to_owned());
        let link = pairing_link_json(&config, 7345).unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();
        assert_eq!(value["serverUrl"], "https://todex.example:8443");
    }

    #[tokio::test]
    async fn pairing_keys_persist_across_restarts() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-persist");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let first = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        let second = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        for encryption in [PairingEncryption::X25519, PairingEncryption::MlKem768] {
            assert_eq!(
                second.transport_public_key(encryption),
                first.transport_public_key(encryption)
            );
        }

        // The reloaded secret agrees with the public key a client pinned.
        let server_public: [u8; 32] = first
            .transport_public_key(PairingEncryption::X25519)
            .try_into()
            .unwrap();
        let client = X25519Secret::random_from_rng(OsRng);
        let client_public = X25519PublicKey::from(&client).to_bytes();
        let server_shared = second
            .agree(EncryptionProtocol::X25519, &client_public)
            .unwrap();
        let client_shared = client.diffie_hellman(&X25519PublicKey::from(server_public));
        assert_eq!(server_shared.as_slice(), client_shared.as_bytes());

        use ml_kem::{Encapsulate, EncapsulationKey, TryKeyInit};
        let server_public = EncapsulationKey::<MlKem768>::new_from_slice(
            first.transport_public_key(PairingEncryption::MlKem768),
        )
        .unwrap();
        let (ciphertext, client_shared) = server_public.encapsulate();
        let server_shared = second
            .agree(EncryptionProtocol::MlKem768, &ciphertext)
            .unwrap();
        assert_eq!(server_shared.as_slice(), client_shared.as_slice());

        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    /// `pairing_keys.json` from pqcrypto-mlkem (PQClean) builds loads as is,
    /// decapsulates what PQClean encapsulated, and is written back unchanged.
    #[tokio::test]
    async fn pqclean_pairing_keys_load_and_round_trip_unchanged() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/pairing-keys-pqclean.json"
        ))
        .unwrap();
        let raw = fixture["file"].to_string();
        let keys = PairingKeys::from_persisted_json(&raw, "fixture").unwrap();
        let ciphertext = vector_tests::hex(&fixture["ciphertext"]);
        let shared = keys
            .agree(EncryptionProtocol::MlKem768, &ciphertext)
            .unwrap();
        assert_eq!(shared.as_slice(), vector_tests::hex(&fixture["shared"]));

        let data_dir = unique_tmp_dir("todex-pairing-keys-pqclean");
        let path = data_dir.join(PAIRING_KEYS_FILE);
        keys.persist(&path).await.unwrap();
        let written: PersistedPairingKeys =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let original: PersistedPairingKeys = serde_json::from_str(&raw).unwrap();
        assert_eq!(written.ml_kem_secret, original.ml_kem_secret);
        assert_eq!(written.ml_kem_public, original.ml_kem_public);
        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    #[test]
    fn pairing_keys_reject_a_mismatched_or_tampered_ml_kem_key() {
        let keys = PairingKeys::generate();
        let other = PairingKeys::generate();
        let persisted = |public: &[u8], secret: &[u8]| {
            serde_json::to_string(&PersistedPairingKeys {
                version: PAIRING_VERSION,
                x25519_secret: encode_b64(keys.x25519_secret.to_bytes().as_slice()),
                ml_kem_public: encode_b64(public),
                ml_kem_secret: encode_b64(secret),
            })
            .unwrap()
        };
        let secret = ml_kem_secret_to_expanded(&keys.ml_kem_secret);
        assert!(
            PairingKeys::from_persisted_json(&persisted(&keys.ml_kem_public, &secret), "keys")
                .is_ok()
        );

        // Another key's public half.
        let error =
            PairingKeys::from_persisted_json(&persisted(&other.ml_kem_public, &secret), "keys");
        assert!(error.is_err());
        // A flipped bit in the public key embedded in the secret (its hash no
        // longer matches), and a truncated secret.
        let mut tampered = secret.to_vec();
        tampered[1152 + 5] ^= 1;
        let error =
            PairingKeys::from_persisted_json(&persisted(&keys.ml_kem_public, &tampered), "keys");
        assert!(error.is_err());
        let error = PairingKeys::from_persisted_json(
            &persisted(&keys.ml_kem_public, &secret[..2399]),
            "keys",
        );
        assert!(error.is_err());
    }

    #[tokio::test]
    async fn pairing_keys_are_written_owner_only_without_leftovers() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-mode");
        // A missing data directory is created.
        PairingKeys::load_or_generate(&data_dir).await.unwrap();
        PairingKeys::reset(&data_dir).await.unwrap();
        let entries = std::fs::read_dir(&data_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [PAIRING_KEYS_FILE]);
        // Created 0600 regardless of the process umask (typically 022).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(data_dir.join(PAIRING_KEYS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_pairing_key_write_leaves_no_temp_file() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-fail");
        // The target is a directory, so the final rename fails.
        let path = data_dir.join(PAIRING_KEYS_FILE);
        std::fs::create_dir_all(&path).unwrap();
        assert!(PairingKeys::generate().persist(&path).await.is_err());
        let entries = std::fs::read_dir(&data_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [PAIRING_KEYS_FILE]);
        assert!(path.is_dir());
        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    #[tokio::test]
    async fn pairing_keys_reset_replaces_persisted_public_keys() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-reset");
        let first = PairingKeys::reset(&data_dir).await.unwrap();
        let second = PairingKeys::reset(&data_dir).await.unwrap();
        for encryption in [PairingEncryption::X25519, PairingEncryption::MlKem768] {
            assert_ne!(
                second.transport_public_key(encryption),
                first.transport_public_key(encryption)
            );
        }
        let reloaded = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        assert_eq!(
            reloaded.transport_public_key(PairingEncryption::MlKem768),
            second.transport_public_key(PairingEncryption::MlKem768)
        );

        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    #[tokio::test]
    async fn key_store_follows_a_reset_and_fails_closed_on_a_broken_file() {
        let data_dir = unique_tmp_dir("todex-pairing-key-store");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let store = PairingKeyStore::load(&data_dir).await.unwrap();
        let first = store.current().unwrap();
        // Unchanged file: the same in-memory keys, no re-read.
        assert!(Arc::ptr_eq(&first, &store.current().unwrap()));

        let reset = PairingKeys::reset(&data_dir).await.unwrap();
        // The reset rewrites an equal-length file; on Windows (no inode in
        // the stamp) one within the load's clock tick needs a later mtime.
        let path = data_dir.join(PAIRING_KEYS_FILE);
        let loaded = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(loaded + std::time::Duration::from_secs(1))
            .unwrap();
        let current = store.current().unwrap();
        assert_eq!(
            current.transport_public_key(PairingEncryption::MlKem768),
            reset.transport_public_key(PairingEncryption::MlKem768)
        );
        assert_ne!(
            current.transport_public_key(PairingEncryption::X25519),
            first.transport_public_key(PairingEncryption::X25519)
        );

        // Never fall back to the previous keys.
        tokio::fs::write(data_dir.join(PAIRING_KEYS_FILE), "{}")
            .await
            .unwrap();
        assert!(store.current().is_err());
        tokio::fs::remove_file(data_dir.join(PAIRING_KEYS_FILE))
            .await
            .unwrap();
        assert!(store.current().is_err());

        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    pub(super) fn test_config() -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 7345,
            pairing_encryption: PairingEncryption::default(),
            data_dir: PathBuf::from("/tmp/todex-test"),
            workspace_roots: vec![PathBuf::from("/tmp/todex-test/workspace")],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                ..Default::default()
            },
            api: Default::default(),
            deployment: Default::default(),
        }
    }

    pub(super) fn unique_tmp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }
}
