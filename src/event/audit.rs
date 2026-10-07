//! Append-only audit trail at `$DATA_DIR/audit/audit.jsonl`.
//!
//! One file handle stays open between writes; it is reopened when the target
//! path changes or after a failed write. Past [`MAX_AUDIT_FILE_BYTES`] the
//! file rotates to `audit.jsonl.1` (older files shift up to
//! [`RETAINED_AUDIT_FILES`]; the oldest is deleted). Every record is synced
//! before `append` returns, on the blocking pool so async workers never wait
//! on the disk.
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::EventRecord;
use crate::error::AppError;

const AUDIT_FILE_NAME: &str = "audit.jsonl";
/// The live file rotates once a write would take it past this size.
const MAX_AUDIT_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Rotated files kept beside the live one (`audit.jsonl.1` is the newest).
const RETAINED_AUDIT_FILES: usize = 3;

#[derive(Clone, Default)]
pub struct AuditLog {
    open: Arc<Mutex<Option<OpenAuditFile>>>,
}

struct OpenAuditFile {
    path: PathBuf,
    file: fs::File,
    len: u64,
}

impl AuditLog {
    /// Appends `event` as one JSON line under `data_dir/audit` and syncs it.
    pub async fn append(&self, data_dir: &Path, event: &EventRecord) -> Result<(), AppError> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let directory = data_dir.join("audit");
        let open = self.open.clone();
        tokio::task::spawn_blocking(move || {
            // A panic while holding the lock leaves at worst a stale handle,
            // which the path/len checks below tolerate.
            let mut open = open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let result = append_line(&mut open, &directory, &line, MAX_AUDIT_FILE_BYTES);
            if result.is_err() {
                // Reopen on the next write instead of reusing a handle in an
                // unknown state.
                *open = None;
            }
            result
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map_err(AppError::from)
    }
}

fn append_line(
    open: &mut Option<OpenAuditFile>,
    directory: &Path,
    line: &[u8],
    max_bytes: u64,
) -> io::Result<()> {
    let path = directory.join(AUDIT_FILE_NAME);
    if open.as_ref().is_none_or(|current| current.path != path) {
        *open = Some(open_audit_file(directory, path)?);
    }
    let current = open.as_mut().expect("audit file was just opened");
    if current.len > 0 && current.len + line.len() as u64 > max_bytes {
        *open = None;
        rotate(directory)?;
        *open = Some(open_audit_file(directory, directory.join(AUDIT_FILE_NAME))?);
    }
    let current = open.as_mut().expect("audit file is open");
    current.file.write_all(line)?;
    current.file.sync_data()?;
    current.len += line.len() as u64;
    Ok(())
}

fn open_audit_file(directory: &Path, path: PathBuf) -> io::Result<OpenAuditFile> {
    crate::secure_fs::ensure_owner_only_dir(directory)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    // Files written by older daemons may predate the 0600 mode.
    crate::secure_fs::set_owner_only(&path, false)?;
    let len = file.metadata()?.len();
    Ok(OpenAuditFile { path, file, len })
}

/// Shifts `audit.jsonl.N` to `.N+1` (dropping the oldest) and moves the live
/// file to `.1`.
fn rotate(directory: &Path) -> io::Result<()> {
    let rotated = |index: usize| directory.join(format!("{AUDIT_FILE_NAME}.{index}"));
    match fs::remove_file(rotated(RETAINED_AUDIT_FILES)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for index in (1..RETAINED_AUDIT_FILES).rev() {
        match fs::rename(rotated(index), rotated(index + 1)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::rename(directory.join(AUDIT_FILE_NAME), rotated(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("todex-audit-{}", uuid::Uuid::new_v4().simple()))
    }

    fn record(index: usize) -> EventRecord {
        EventRecord::new(
            "fixture.audit",
            None,
            None,
            None,
            serde_json::json!({ "index": index }),
        )
    }

    #[tokio::test]
    async fn appends_lines_through_one_handle() {
        let temp = temp_dir();
        let log = AuditLog::default();
        log.append(&temp, &record(1)).await.unwrap();
        log.append(&temp, &record(2)).await.unwrap();
        let text = fs::read_to_string(temp.join("audit/audit.jsonl")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("\"index\":2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(temp.join("audit/audit.jsonl"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn rotates_and_keeps_a_bounded_number_of_files() {
        let temp = temp_dir();
        let directory = temp.join("audit");
        let mut open = None;
        let line = b"0123456789\n";
        // Two lines per file: 12 lines leave the live file plus 3 rotations.
        for _ in 0..12 {
            append_line(&mut open, &directory, line, 22).unwrap();
        }
        assert_eq!(
            fs::read(directory.join("audit.jsonl")).unwrap().len(),
            2 * line.len()
        );
        for index in 1..=RETAINED_AUDIT_FILES {
            let rotated = directory.join(format!("audit.jsonl.{index}"));
            assert_eq!(fs::read(rotated).unwrap().len(), 2 * line.len());
        }
        assert!(!directory
            .join(format!("audit.jsonl.{}", RETAINED_AUDIT_FILES + 1))
            .exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn switches_files_when_the_data_dir_changes() {
        let temp = temp_dir();
        let directory = temp.join("audit");
        let mut open = None;
        append_line(&mut open, &directory, b"first\n", 1024).unwrap();
        let other = temp.join("other");
        append_line(&mut open, &other, b"second\n", 1024).unwrap();
        assert_eq!(
            fs::read_to_string(other.join("audit.jsonl")).unwrap(),
            "second\n"
        );
        assert_eq!(
            fs::read_to_string(directory.join("audit.jsonl")).unwrap(),
            "first\n"
        );
        let _ = fs::remove_dir_all(&temp);
    }
}
