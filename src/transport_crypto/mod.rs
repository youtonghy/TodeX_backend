//! Pairing keys and transport encryption.
//!
//! - [`PairingKeys`]: the daemon's static X25519 / ML-KEM-768 keys that
//!   clients pin at pairing, plus the pairing QR payloads.
//! - Transport v2 (`docs/transport-v2.md`): [`handshake`] (key agreement and
//!   key schedule), [`channel`] (sealed records and WebSocket frames) and
//!   [`envelope`] (REST record streams and the inner request/response).
//! - [`legacy`]: the v1 `todex.crypto.v1` WebSocket wrapper, accepted while
//!   clients migrate.
pub(crate) mod channel;
pub(crate) mod envelope;
pub(crate) mod handshake;
mod legacy;
pub(crate) mod pairing_browser;
#[cfg(test)]
mod vector_tests;

pub(crate) use channel::TransportCryptoError;
pub use legacy::TransportCryptoSession;

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use pqcrypto_mlkem::mlkem768;
use pqcrypto_traits::kem::{PublicKey as MlKemPublicKey, SecretKey as MlKemSecretKey};
use qrcodegen::{QrCode, QrCodeEcc};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use crate::config::{Config, PairingEncryption};
use crate::error::AppError;

const PAIRING_VERSION: u8 = 1;
const PAIRING_QR_SEGMENT_DATA_LENGTH: usize = 160;
const PAIRING_KEYS_FILE: &str = "pairing_keys.json";

#[derive(Clone)]
pub struct PairingKeys {
    x25519_secret: Arc<X25519Secret>,
    x25519_public: [u8; 32],
    ml_kem_secret: Arc<mlkem768::SecretKey>,
    ml_kem_public: Vec<u8>,
}

impl PairingKeys {
    pub fn generate() -> Self {
        let x25519_secret = X25519Secret::random_from_rng(OsRng);
        let x25519_public = X25519PublicKey::from(&x25519_secret).to_bytes();
        let (ml_kem_public, ml_kem_secret) = mlkem768::keypair();

        Self {
            x25519_secret: Arc::new(x25519_secret),
            x25519_public,
            ml_kem_secret: Arc::new(ml_kem_secret),
            ml_kem_public: ml_kem_public.as_bytes().to_vec(),
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

    pub(crate) fn pairing_public_key(&self, encryption: PairingEncryption) -> Option<String> {
        match encryption {
            PairingEncryption::None => None,
            PairingEncryption::X25519 => Some(encode_b64(&self.x25519_public)),
            PairingEncryption::MlKem768 => Some(encode_b64(&self.ml_kem_public)),
        }
    }

    async fn persist(&self, path: &Path) -> Result<(), AppError> {
        let persisted = PersistedPairingKeys {
            version: PAIRING_VERSION,
            x25519_secret: encode_b64(self.x25519_secret.to_bytes().as_slice()),
            ml_kem_public: encode_b64(&self.ml_kem_public),
            ml_kem_secret: encode_b64(self.ml_kem_secret.as_bytes()),
        };
        let json = serde_json::to_string_pretty(&persisted)?;
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        set_owner_only_permissions(&tmp_path).await?;
        #[cfg(windows)]
        if tokio::fs::try_exists(path).await? {
            tokio::fs::remove_file(path).await?;
        }
        tokio::fs::rename(&tmp_path, path).await?;
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
        let ml_kem_secret_bytes =
            decode_b64(&persisted.ml_kem_secret, "ml-kem-768 pairing secret")?;
        let ml_kem_secret =
            mlkem768::SecretKey::from_bytes(&ml_kem_secret_bytes).map_err(|_| {
                AppError::InvalidRequest("ml-kem-768 pairing secret is invalid".to_owned())
            })?;
        let ml_kem_public = decode_b64(&persisted.ml_kem_public, "ml-kem-768 pairing public key")?;
        mlkem768::PublicKey::from_bytes(&ml_kem_public).map_err(|_| {
            AppError::InvalidRequest("ml-kem-768 pairing public key is invalid".to_owned())
        })?;

        Ok(Self {
            x25519_secret: Arc::new(x25519_secret),
            x25519_public,
            ml_kem_secret: Arc::new(ml_kem_secret),
            ml_kem_public,
        })
    }

    #[cfg(test)]
    fn pairing_qr_text(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        render_qr_text(&self.pairing_qr_payload(config, port, preferred_encryption)?)
    }

    #[allow(dead_code)]
    pub(crate) fn pairing_qr_payload(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        self.pairing_link_json(config, port, preferred_encryption)
    }

    pub(crate) fn pairing_qr_payloads(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<Vec<String>, AppError> {
        let payload = self.pairing_link_json(config, port, preferred_encryption)?;
        if preferred_encryption != PairingEncryption::MlKem768 {
            return Ok(vec![payload]);
        }

        self.segment_pairing_qr_payload(&payload)
    }

    fn pairing_link_json(
        &self,
        config: &Config,
        port: u16,
        preferred_encryption: PairingEncryption,
    ) -> Result<String, AppError> {
        let advertise_host = pairing_advertise_host(&config.host);
        Ok(serde_json::to_string(&PairingLinkPayload {
            kind: "todex-pairing-link".to_owned(),
            version: PAIRING_VERSION,
            server_url: format!("http://{advertise_host}:{port}"),
            // Pairing links never carry credentials: devices authenticate by
            // signature after the device-verification ceremony registers them.
            preferred_encryption: Some(preferred_encryption),
            protocol: self.pairing_protocol_for(preferred_encryption),
        })?)
    }

    fn pairing_protocol_for(&self, encryption: PairingEncryption) -> Option<PairingProtocol> {
        match encryption {
            PairingEncryption::None => None,
            PairingEncryption::X25519 => Some(PairingProtocol {
                id: EncryptionProtocol::X25519.as_str().to_owned(),
                public_key: encode_b64(&self.x25519_public),
            }),
            PairingEncryption::MlKem768 => Some(PairingProtocol {
                id: EncryptionProtocol::MlKem768.as_str().to_owned(),
                public_key: encode_b64(&self.ml_kem_public),
            }),
        }
    }

    fn segment_pairing_qr_payload(&self, payload: &str) -> Result<Vec<String>, AppError> {
        let encoded = encode_b64(payload.as_bytes());
        let checksum = encode_b64(&Sha256::digest(payload.as_bytes()));
        let chunks: Vec<&[u8]> = encoded
            .as_bytes()
            .chunks(PAIRING_QR_SEGMENT_DATA_LENGTH)
            .collect();
        let total = chunks.len() as u16;

        chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| {
                serde_json::to_string(&PairingQrChunkPayload {
                    kind: "todex-pairing-chunk".to_owned(),
                    version: PAIRING_VERSION,
                    checksum: checksum.clone(),
                    index: (index + 1) as u16,
                    total,
                    data: String::from_utf8(chunk.to_vec())
                        .expect("base64url chunk should always be valid UTF-8"),
                })
                .map_err(Into::into)
            })
            .collect()
    }
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingProtocol {
    id: String,
    public_key: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingQrChunkPayload {
    kind: String,
    version: u8,
    checksum: String,
    index: u16,
    total: u16,
    data: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingLinkPayload {
    kind: String,
    version: u8,
    server_url: String,
    preferred_encryption: Option<PairingEncryption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<PairingProtocol>,
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

#[cfg(unix)]
async fn set_owner_only_permissions(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_owner_only_permissions(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

/// Render locally generated QR geometry only. Payload text never becomes HTML,
/// a script literal, a URL, or an attribute value.
pub(crate) fn render_pairing_qr_browser_html(payloads: &[String]) -> Result<String, AppError> {
    if payloads.is_empty() || payloads.len() > 128 {
        return Err(AppError::InvalidRequest(
            "invalid pairing QR frame count".to_owned(),
        ));
    }
    let mut frames = String::new();
    for (index, payload) in payloads.iter().enumerate() {
        let qr = QrCode::encode_text(payload, QrCodeEcc::Low).map_err(|_| {
            AppError::InvalidRequest("pairing payload is too large for QR".to_owned())
        })?;
        let modules = qr.size() + 8;
        let pixels = modules * 8;
        let mut path = String::new();
        for y in 0..qr.size() {
            for x in 0..qr.size() {
                if qr.get_module(x, y) {
                    use std::fmt::Write;
                    write!(&mut path, "M{} {}h1v1h-1z", x + 4, y + 4).expect("writing to String");
                }
            }
        }
        use std::fmt::Write;
        write!(&mut frames,
            r##"<div class="qr-frame"{}><svg xmlns="http://www.w3.org/2000/svg" role="img" aria-label="配对二维码 {} / {}" width="{}" height="{}" viewBox="0 0 {} {}" shape-rendering="crispEdges"><rect width="{}" height="{}" fill="#ffffff"/><path fill="#000000" d="{}"/></svg></div>"##,
            if index == 0 { "" } else { " hidden" }, index + 1, payloads.len(), pixels, pixels,
            modules, modules, modules, modules, path,
        ).expect("writing to String");
    }
    let script_hash = base64::engine::general_purpose::STANDARD
        .encode(Sha256::digest(PAIRING_BROWSER_SCRIPT.as_bytes()));
    let controls_disabled = if payloads.len() == 1 { " disabled" } else { "" };
    Ok(format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light"><meta name="referrer" content="no-referrer"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'sha256-{script_hash}'; base-uri 'none'; form-action 'none'; object-src 'none'; connect-src 'none'"><title>TodeX 配对</title><style>{style}</style></head><body><main><h1>TodeX 配对</h1><p>在客户端导入同一组的全部二维码。</p><div class="qr-window">{frames}</div><nav aria-label="二维码分片"><button id="previous" type="button"{controls_disabled}>← 上一张</button><output id="counter" aria-live="polite">1 / {total}</output><button id="next" type="button"{controls_disabled}>下一张 →</button></nav><p class="hint">也可使用方向键切换；二维码已完整保留白色边缘。</p></main><script>{script}</script></body></html>"#,
        style = PAIRING_BROWSER_STYLE,
        total = payloads.len(),
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
.qr-frame[hidden] { display: none; }
svg { display: block; }
nav { display: flex; align-items: center; justify-content: center; gap: 20px; margin-top: 20px; }
button { border: 1px solid #bac5ce; border-radius: 8px; background: #fff; color: #172129; padding: 10px 16px; font: inherit; cursor: pointer; }
button:focus-visible { outline: 3px solid #168a70; outline-offset: 3px; }
button:disabled { opacity: .45; cursor: default; }
output { min-width: 64px; font-variant-numeric: tabular-nums; }
.hint { font-size: 13px; color: #52606b; margin-bottom: 0; }
"#;

const PAIRING_BROWSER_SCRIPT: &str = r#"
'use strict';
const frames = Array.from(document.querySelectorAll('.qr-frame'));
const counter = document.getElementById('counter');
let active = 0;
function fit() {
  const svg = frames[active].querySelector('svg');
  const modules = svg.viewBox.baseVal.width;
  const availableWidth = Math.max(1, document.documentElement.clientWidth - 48);
  const availableHeight = Math.max(1, window.innerHeight - 210);
  const scale = Math.max(1, Math.min(8, Math.floor(availableWidth / modules), Math.floor(availableHeight / modules)));
  svg.setAttribute('width', String(modules * scale));
  svg.setAttribute('height', String(modules * scale));
}
function show(index) {
  frames[active].hidden = true;
  active = (index + frames.length) % frames.length;
  frames[active].hidden = false;
  counter.textContent = (active + 1) + ' / ' + frames.length;
  fit();
}
document.getElementById('previous').addEventListener('click', () => show(active - 1));
document.getElementById('next').addEventListener('click', () => show(active + 1));
document.addEventListener('keydown', event => {
  if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') { event.preventDefault(); show(active - 1); }
  if (event.key === 'ArrowRight' || event.key === 'ArrowDown') { event.preventDefault(); show(active + 1); }
});
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
        let html = render_pairing_qr_browser_html(&[payload.clone()]).unwrap();
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
        assert!(html.contains("type=\"button\" disabled"));
        let hash = base64::engine::general_purpose::STANDARD
            .encode(Sha256::digest(PAIRING_BROWSER_SCRIPT.as_bytes()));
        assert!(html.contains(&format!("script-src 'sha256-{hash}'")));
        assert!(html.contains("ArrowLeft") && html.contains("ArrowRight"));
        assert!(html.contains("ArrowUp") && html.contains("ArrowDown"));
    }

    #[test]
    fn browser_pairing_page_keeps_all_segments_with_one_visible_frame() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let payloads = keys
            .pairing_qr_payloads(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();
        let html = render_pairing_qr_browser_html(&payloads).unwrap();
        assert!(payloads.len() > 1);
        assert_eq!(html.matches("<svg ").count(), payloads.len());
        assert_eq!(
            html.matches("class=\"qr-frame\" hidden").count(),
            payloads.len() - 1
        );
        assert!(!html.contains("todex-pairing-chunk"));
        assert!(!html.contains("publicKey"));
        assert!(render_pairing_qr_browser_html(&[]).is_err());
        assert!(render_pairing_qr_browser_html(&vec!["synthetic".to_owned(); 129]).is_err());
    }

    #[test]
    fn pairing_qr_embeds_selected_public_key() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_eq!(value["kind"], "todex-pairing-link");
        // Pairing links carry no credentials; devices prove identity by
        // signature after the verification ceremony registers their key.
        assert!(value.get("authToken").is_none());
        assert_eq!(value["preferredEncryption"], "x25519");
        assert_eq!(value["protocol"]["id"], "x25519");
        assert!(value["protocol"]["publicKey"].as_str().unwrap().len() > 40);
        assert!(value.get("protocols").is_none());
        assert!(value.get("pairingUrl").is_none());

        let qr = keys
            .pairing_qr_text(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let max_width = qr.lines().map(|line| line.chars().count()).max().unwrap();
        assert!(
            max_width <= 80,
            "x25519 pairing QR should fit common terminal widths, got {max_width}"
        );
    }

    #[test]
    fn pairing_public_key_returns_only_the_selected_public_material() {
        let keys = PairingKeys::generate();

        assert_eq!(keys.pairing_public_key(PairingEncryption::None), None);
        assert_eq!(
            keys.pairing_public_key(PairingEncryption::X25519),
            Some(encode_b64(&keys.x25519_public))
        );
        assert_eq!(
            keys.pairing_public_key(PairingEncryption::MlKem768),
            Some(encode_b64(&keys.ml_kem_public))
        );
        assert_ne!(
            keys.pairing_public_key(PairingEncryption::X25519),
            Some(encode_b64(&keys.x25519_secret.to_bytes()))
        );
    }

    #[test]
    fn ml_kem_pairing_qr_is_split_into_segments() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let frames = keys
            .pairing_qr_payloads(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();

        assert!(frames.len() > 1, "ml-kem pairing QR should be segmented");

        let mut encoded = String::new();
        let mut checksum = String::new();

        for (index, frame) in frames.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(frame).unwrap();
            assert_eq!(value["kind"], "todex-pairing-chunk");
            assert_eq!(value["version"], PAIRING_VERSION);
            assert_eq!(value["index"], (index + 1) as u64);
            assert_eq!(value["total"], frames.len() as u64);
            if checksum.is_empty() {
                checksum = value["checksum"].as_str().unwrap().to_owned();
            } else {
                assert_eq!(value["checksum"], checksum);
            }
            encoded.push_str(value["data"].as_str().unwrap());
        }

        let decoded = decode_b64(&encoded, "pairing qr chunk payload").unwrap();
        assert_eq!(encode_b64(&Sha256::digest(&decoded)), checksum);
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["kind"], "todex-pairing-link");
        assert_eq!(value["preferredEncryption"], "ml-kem-768");
    }

    #[test]
    fn plaintext_pairing_qr_does_not_embed_a_public_key() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::None)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_eq!(value["preferredEncryption"], "none");
        assert!(value.get("protocol").is_none());
    }

    #[test]
    fn pairing_qr_renderer_uses_block_cells_instead_of_braille_dots() {
        let keys = PairingKeys::generate();
        let config = test_config();
        let payload = keys
            .pairing_qr_payload(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let rendered = render_qr_text_for_bounds(&payload, 76, 20).unwrap();

        assert!(
            rendered.width <= 76,
            "pairing QR should fit terminal content width, got {}",
            rendered.width
        );
        assert!(
            rendered.height > 20,
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
        let keys = PairingKeys::generate();
        let mut config = test_config();
        config.host = "0.0.0.0".to_owned();
        let link = keys
            .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&link).unwrap();

        assert_ne!(value["serverUrl"], "http://0.0.0.0:7345");
        assert!(value.get("authToken").is_none());
        assert!(value.get("pairingUrl").is_none());
    }

    #[tokio::test]
    async fn pairing_keys_persist_across_restarts() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-persist");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let config = test_config();
        let first = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        let first_x25519 = first
            .pairing_link_json(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let first_ml_kem = first
            .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();

        let second = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        assert_eq!(
            second
                .pairing_link_json(&config, 7345, PairingEncryption::X25519)
                .unwrap(),
            first_x25519
        );
        assert_eq!(
            second
                .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
                .unwrap(),
            first_ml_kem
        );

        // The reloaded secret agrees with the public key a client pinned.
        let first_value: serde_json::Value = serde_json::from_str(&first_x25519).unwrap();
        let server_public = decode_fixed_32(
            first_value["protocol"]["publicKey"].as_str().unwrap(),
            "persisted x25519 public key",
        )
        .unwrap();
        let client = X25519Secret::random_from_rng(OsRng);
        let client_public = X25519PublicKey::from(&client).to_bytes();
        let server_shared = second
            .agree(EncryptionProtocol::X25519, &client_public)
            .unwrap();
        let client_shared = client.diffie_hellman(&X25519PublicKey::from(server_public));
        assert_eq!(server_shared.as_slice(), client_shared.as_bytes());

        tokio::fs::remove_dir_all(&data_dir).await.unwrap();
    }

    #[tokio::test]
    async fn pairing_keys_reset_replaces_persisted_public_keys() {
        let data_dir = unique_tmp_dir("todex-pairing-keys-reset");
        let config = test_config();

        let first = PairingKeys::reset(&data_dir).await.unwrap();
        let first_x25519 = first
            .pairing_link_json(&config, 7345, PairingEncryption::X25519)
            .unwrap();
        let first_ml_kem = first
            .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
            .unwrap();

        let second = PairingKeys::reset(&data_dir).await.unwrap();
        assert_ne!(
            second
                .pairing_link_json(&config, 7345, PairingEncryption::X25519)
                .unwrap(),
            first_x25519
        );
        assert_ne!(
            second
                .pairing_link_json(&config, 7345, PairingEncryption::MlKem768)
                .unwrap(),
            first_ml_kem
        );

        let reloaded = PairingKeys::load_or_generate(&data_dir).await.unwrap();
        assert_eq!(
            reloaded
                .pairing_link_json(&config, 7345, PairingEncryption::X25519)
                .unwrap(),
            second
                .pairing_link_json(&config, 7345, PairingEncryption::X25519)
                .unwrap()
        );

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
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
        }
    }

    pub(super) fn unique_tmp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }
}
