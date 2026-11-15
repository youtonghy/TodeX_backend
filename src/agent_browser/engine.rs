//! Which engine runs the agent browser, persisted in
//! `$DATA_DIR/agent-browser/engine.json`. Chromium is the default; Kitesurf
//! is experimental and only runs when chosen.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const ENGINE_FILE: &str = "engine.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BrowserEngineKind {
    /// Chrome for Testing: headed, or headless without a display.
    #[default]
    Chromium,
    /// Cloudflare's Kitesurf on a local `workerd`: headless, no state kept
    /// between tabs.
    Kitesurf,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EngineSettings {
    #[serde(default)]
    engine: BrowserEngineKind,
}

fn file(data_dir: &Path) -> PathBuf {
    data_dir.join("agent-browser").join(ENGINE_FILE)
}

/// The saved engine; Chromium when none is saved or the file is unreadable.
pub(crate) fn load(data_dir: &Path) -> BrowserEngineKind {
    let path = file(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<EngineSettings>(&bytes) {
            Ok(settings) => settings.engine,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "agent browser: ignoring an unreadable engine setting");
                BrowserEngineKind::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BrowserEngineKind::default(),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "agent browser: cannot read the engine setting");
            BrowserEngineKind::default()
        }
    }
}

pub(crate) fn save(data_dir: &Path, engine: BrowserEngineKind) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&EngineSettings { engine })?;
    crate::secure_fs::write_owner_only_atomic(&file(data_dir), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_round_trips_and_defaults_to_chromium() {
        let root = std::env::temp_dir().join(format!("todex-engine-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("agent-browser")).unwrap();
        assert_eq!(load(&root), BrowserEngineKind::Chromium);
        save(&root, BrowserEngineKind::Kitesurf).unwrap();
        assert_eq!(load(&root), BrowserEngineKind::Kitesurf);
        std::fs::write(file(&root), b"{ not json").unwrap();
        assert_eq!(load(&root), BrowserEngineKind::Chromium);
        let _ = std::fs::remove_dir_all(root);
    }
}
