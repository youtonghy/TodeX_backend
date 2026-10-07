//! Golden-file assertions for snapshots of provider behaviour.
//!
//! Regenerate with `TODEX_UPDATE_GOLDEN=1 cargo test golden` and review the
//! diff: every change to a golden file is a client- or CLI-visible change.

use std::path::Path;

use serde_json::Value;

const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/provider/golden");

/// Compares `actual` with `src/provider/golden/<name>.json`.
pub(crate) fn assert_golden(name: &str, actual: &Value) {
    let path = Path::new(GOLDEN_DIR).join(format!("{name}.json"));
    let rendered = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    if std::env::var_os("TODEX_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(GOLDEN_DIR).unwrap();
        std::fs::write(&path, rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing golden {}: {error}", path.display()));
    let expected: Value = serde_json::from_str(&expected).unwrap();
    assert!(
        &expected == actual,
        "golden {name} changed; expected:\n{}\nactual:\n{rendered}",
        serde_json::to_string_pretty(&expected).unwrap()
    );
}
