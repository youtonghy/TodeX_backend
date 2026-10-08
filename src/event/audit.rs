//! Append-only audit trails under `$DATA_DIR/audit/`.
//!
//! [`AuditLog::default`] writes `audit.jsonl` (authorization, Git, Codex,
//! provider decisions, ...). [`AuditLog::terminal`] writes
//! `audit-terminal.jsonl`: terminal input and resize are audited per
//! message, so they get their own file and size budget and cannot rotate the
//! main trail away.
//!
//! One file handle stays open between writes; it is reopened when the target
//! path changes or after a failed write. Past [`MAX_AUDIT_FILE_BYTES`] the
//! file rotates to `<name>.1` (older files shift up to
//! [`RETAINED_AUDIT_FILES`]; the oldest is replaced). A rotation that fails
//! is logged and retried later; meanwhile records keep going to the live
//! file, so a stuck rotation neither fails writes nor deletes old files.
//! Every record is synced before `append` returns, on the blocking pool so
//! async workers never wait on the disk.
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::warn;

use super::EventRecord;
use crate::error::AppError;

const AUDIT_FILE_NAME: &str = "audit.jsonl";
const TERMINAL_AUDIT_FILE_NAME: &str = "audit-terminal.jsonl";
/// The live file rotates once a write would take it past this size.
const MAX_AUDIT_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Rotated files kept beside the live one (`<name>.1` is the newest).
const RETAINED_AUDIT_FILES: usize = 3;
/// After a failed rotation, appends go to the live file for this long
/// before rotation is tried again.
const ROTATION_RETRY_DELAY: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AuditLog {
    file_name: &'static str,
    open: Arc<Mutex<Option<OpenAuditFile>>>,
}

impl Default for AuditLog {
    /// The main trail, `audit.jsonl`.
    fn default() -> Self {
        Self::with_file_name(AUDIT_FILE_NAME)
    }
}

struct OpenAuditFile {
    path: PathBuf,
    file: fs::File,
    len: u64,
    /// Set after a failed rotation: do not try again before this instant.
    rotation_retry_at: Option<Instant>,
}

impl AuditLog {
    /// The terminal trail, `audit-terminal.jsonl`.
    pub fn terminal() -> Self {
        Self::with_file_name(TERMINAL_AUDIT_FILE_NAME)
    }

    fn with_file_name(file_name: &'static str) -> Self {
        Self {
            file_name,
            open: Arc::default(),
        }
    }

    /// Appends `event` as one JSON line under `data_dir/audit` and syncs it.
    pub async fn append(&self, data_dir: &Path, event: &EventRecord) -> Result<(), AppError> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let directory = data_dir.join("audit");
        let open = self.open.clone();
        let file_name = self.file_name;
        tokio::task::spawn_blocking(move || {
            // A panic while holding the lock leaves at worst a stale handle,
            // which the path/len checks below tolerate.
            let mut open = open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let result = append_line(
                &mut open,
                &directory,
                file_name,
                &line,
                MAX_AUDIT_FILE_BYTES,
            );
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
    file_name: &str,
    line: &[u8],
    max_bytes: u64,
) -> io::Result<()> {
    let path = directory.join(file_name);
    if open.as_ref().is_none_or(|current| current.path != path) {
        *open = Some(open_audit_file(directory, path.clone())?);
    }
    let current = open.as_mut().expect("audit file was just opened");
    let rotation_due = current.len > 0
        && current.len + line.len() as u64 > max_bytes
        && current
            .rotation_retry_at
            .is_none_or(|retry_at| Instant::now() >= retry_at);
    if rotation_due {
        // Close the handle first: Windows cannot rename an open file.
        *open = None;
        let rotated = rotate(directory, file_name);
        let mut reopened = open_audit_file(directory, path)?;
        if let Err(error) = rotated {
            warn!(
                file = file_name,
                error = %error,
                "audit log rotation failed; appending to the live file"
            );
            reopened.rotation_retry_at = Some(Instant::now() + ROTATION_RETRY_DELAY);
        }
        *open = Some(reopened);
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
    Ok(OpenAuditFile {
        path,
        file,
        len,
        rotation_retry_at: None,
    })
}

/// Moves the live file aside first, then shifts `<name>.N` to `.N+1` (the
/// rename replaces, and so drops, the oldest) and puts the live file at
/// `.1`. Nothing is deleted before the live file is known to be movable,
/// and a failure moves it back so writes continue where they were.
fn rotate(directory: &Path, file_name: &str) -> io::Result<()> {
    let live = directory.join(file_name);
    let rotated = |index: usize| directory.join(format!("{file_name}.{index}"));
    let staged = directory.join(format!("{file_name}.rotating"));
    // A staged file left by an earlier failure would be overwritten.
    if fs::symlink_metadata(&staged).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is in the way", staged.display()),
        ));
    }
    fs::rename(&live, &staged)?;
    let shifted = (1..RETAINED_AUDIT_FILES)
        .rev()
        .try_for_each(
            |index| match fs::rename(rotated(index), rotated(index + 1)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        )
        .and_then(|()| fs::rename(&staged, rotated(1)));
    if let Err(error) = shifted {
        if let Err(restore) = fs::rename(&staged, &live) {
            warn!(
                file = file_name,
                error = %restore,
                "could not move the staged audit log back; it stays beside the live file"
            );
        }
        return Err(error);
    }
    Ok(())
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
        let line: &[u8] = b"0123456789\n";
        // Two lines per file: 12 lines leave the live file plus 3 rotations.
        for _ in 0..12 {
            append_line(&mut open, &directory, AUDIT_FILE_NAME, line, 22).unwrap();
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
        append_line(&mut open, &directory, AUDIT_FILE_NAME, b"first\n", 1024).unwrap();
        let other = temp.join("other");
        append_line(&mut open, &other, AUDIT_FILE_NAME, b"second\n", 1024).unwrap();
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

    #[test]
    fn a_failed_rotation_keeps_writing_and_keeps_old_files() {
        let temp = temp_dir();
        let directory = temp.join("audit");
        fs::create_dir_all(&directory).unwrap();
        let line: &[u8] = b"0123456789\n";
        fs::write(directory.join("audit.jsonl"), [line, line].concat()).unwrap();
        fs::write(directory.join("audit.jsonl.1"), b"one\n").unwrap();
        // The file `.2` cannot be moved onto the directory `.3`: the shift
        // fails (a directory could replace a file on Windows instead).
        fs::write(directory.join("audit.jsonl.2"), b"two\n").unwrap();
        fs::create_dir_all(directory.join("audit.jsonl.3/inside")).unwrap();

        let mut open = None;
        append_line(&mut open, &directory, AUDIT_FILE_NAME, line, 22).unwrap();
        // The retry delay keeps the next write from rotating again.
        append_line(&mut open, &directory, AUDIT_FILE_NAME, line, 22).unwrap();
        assert_eq!(
            fs::read(directory.join("audit.jsonl")).unwrap(),
            [line, line, line, line].concat()
        );
        assert_eq!(fs::read(directory.join("audit.jsonl.1")).unwrap(), b"one\n");
        assert_eq!(fs::read(directory.join("audit.jsonl.2")).unwrap(), b"two\n");
        assert!(directory.join("audit.jsonl.3/inside").is_dir());
        assert!(!directory.join("audit.jsonl.rotating").exists());
        assert!(open.as_ref().unwrap().rotation_retry_at.is_some());

        // Once the obstacle is gone, the next due rotation succeeds.
        fs::remove_dir_all(directory.join("audit.jsonl.3")).unwrap();
        open.as_mut().unwrap().rotation_retry_at = Some(Instant::now());
        append_line(&mut open, &directory, AUDIT_FILE_NAME, line, 22).unwrap();
        assert_eq!(fs::read(directory.join("audit.jsonl")).unwrap(), line);
        assert_eq!(
            fs::read(directory.join("audit.jsonl.1")).unwrap(),
            [line, line, line, line].concat()
        );
        assert_eq!(fs::read(directory.join("audit.jsonl.2")).unwrap(), b"one\n");
        assert_eq!(fs::read(directory.join("audit.jsonl.3")).unwrap(), b"two\n");
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn a_live_file_that_cannot_move_is_not_rotated() {
        let temp = temp_dir();
        let directory = temp.join("audit");
        fs::create_dir_all(&directory).unwrap();
        let line: &[u8] = b"0123456789\n";
        fs::write(directory.join("audit.jsonl"), [line, line].concat()).unwrap();
        fs::write(directory.join("audit.jsonl.3"), b"three\n").unwrap();
        // A leftover staged file blocks the first step.
        fs::create_dir_all(directory.join("audit.jsonl.rotating")).unwrap();
        let mut open = None;
        append_line(&mut open, &directory, AUDIT_FILE_NAME, line, 22).unwrap();
        assert_eq!(
            fs::read(directory.join("audit.jsonl")).unwrap(),
            [line, line, line].concat()
        );
        assert_eq!(
            fs::read(directory.join("audit.jsonl.3")).unwrap(),
            b"three\n"
        );
        let _ = fs::remove_dir_all(&temp);
    }

    #[tokio::test]
    async fn terminal_records_have_their_own_file() {
        let temp = temp_dir();
        AuditLog::default().append(&temp, &record(1)).await.unwrap();
        AuditLog::terminal()
            .append(&temp, &record(2))
            .await
            .unwrap();
        let main = fs::read_to_string(temp.join("audit/audit.jsonl")).unwrap();
        let terminal = fs::read_to_string(temp.join("audit/audit-terminal.jsonl")).unwrap();
        assert!(main.contains("\"index\":1") && !main.contains("\"index\":2"));
        assert!(terminal.contains("\"index\":2") && !terminal.contains("\"index\":1"));
        let _ = fs::remove_dir_all(&temp);
    }
}
