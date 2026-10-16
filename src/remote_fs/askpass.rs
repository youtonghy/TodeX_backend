//! `SSH_ASKPASS` helper: `todex-agentd ssh-askpass <prompt>`.
//!
//! OpenSSH runs the askpass program as `<program> <prompt>` without extra
//! arguments, so `main` maps that invocation to the hidden `ssh-askpass`
//! subcommand when [`FILE_ENV`] is present. The password never enters an
//! environment: the daemon writes it to an owner-only file under
//! `$DATA_DIR/ssh/askpass`, the ssh child's environment carries only that
//! file's path, and the helper deletes the file once it answered a password
//! prompt. [`AskpassSecret`] removes whatever the helper did not consume.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use uuid::Uuid;

use crate::secure_fs;

/// Path of the one-shot secret file; set only on an ssh child started with
/// a request-supplied password.
pub(crate) const FILE_ENV: &str = "TODEX_SSH_ASKPASS_FILE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Credential {
    /// Consumes the secret: ssh asks for a password at most once.
    Password,
    /// A key passphrase; answered without consuming, so a password prompt
    /// that follows a rejected key still gets the secret.
    Passphrase,
}

/// What `prompt` asks for, or `None` to refuse. Host-key confirmations and
/// anything that is not a password or passphrase prompt (one-time codes,
/// arbitrary questions) are refused.
pub(crate) fn classify(prompt: &str) -> Option<Credential> {
    let prompt = prompt.to_lowercase();
    let confirmation = [
        "yes/no",
        "(yes",
        "fingerprint",
        "continue connecting",
        "host key",
    ]
    .iter()
    .any(|marker| prompt.contains(marker));
    if confirmation {
        None
    } else if prompt.contains("passphrase") {
        Some(Credential::Passphrase)
    } else if prompt.contains("password") {
        Some(Credential::Password)
    } else {
        None
    }
}

/// Runs the helper; returns the process exit code.
pub(crate) fn run(prompt: Option<&str>) -> i32 {
    let file = std::env::var_os(FILE_ENV).map(PathBuf::from);
    respond(
        prompt.unwrap_or_default(),
        file.as_deref(),
        &mut std::io::stdout().lock(),
    )
}

fn respond(prompt: &str, file: Option<&Path>, out: &mut impl Write) -> i32 {
    let (Some(credential), Some(file)) = (classify(prompt), file) else {
        return 1;
    };
    let Ok(secret) = std::fs::read(file) else {
        return 1;
    };
    let secret = zeroize::Zeroizing::new(secret);
    if credential == Credential::Password {
        // Without the delete a later prompt could reuse the password.
        if std::fs::remove_file(file).is_err() {
            return 1;
        }
    }
    if secret.is_empty() {
        return 1;
    }
    match out
        .write_all(&secret)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
    {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Rewrites `todex-agentd <prompt>` (how ssh invokes `SSH_ASKPASS`) into
/// `todex-agentd ssh-askpass <prompt>`. Other invocations are unchanged.
pub(crate) fn rewrite_args(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    if std::env::var_os(FILE_ENV).is_some() && args.len() == 2 && args[1] != "ssh-askpass" {
        args.insert(1, "ssh-askpass".into());
    }
    args
}

/// A one-shot secret file for one ssh child; dropping it deletes the file.
#[must_use = "dropping the guard deletes the secret file"]
pub(crate) struct AskpassSecret {
    path: PathBuf,
}

impl AskpassSecret {
    /// Writes `password` to a new owner-only file in `dir` (created 0700).
    /// Blocking: call from the blocking pool.
    pub(crate) fn create(dir: &Path, password: &str) -> std::io::Result<Self> {
        secure_fs::ensure_owner_only_dir(dir)?;
        let path = dir.join(Uuid::new_v4().simple().to_string());
        secure_fs::create_owner_only(&path, password.as_bytes())?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Keeps the file for at most `deadline` in the background, for an ssh
    /// child the caller does not wait for (a terminal, which may become the
    /// shared master). The helper deletes it earlier once it answered.
    pub(crate) fn expire_after(self, deadline: Duration) {
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            drop(self);
        });
    }
}

impl Drop for AskpassSecret {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Deletes secret files a previous daemon left behind (crash, kill).
/// Blocking: call from the blocking pool.
pub(crate) fn purge(dir: &Path) -> std::io::Result<()> {
    secure_fs::ensure_owner_only_dir(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_KEY_PROMPT: &str =
        "The authenticity of host 'db (10.0.0.5)' can't be established.\n\
         ED25519 key fingerprint is SHA256:abc.\n\
         Are you sure you want to continue connecting (yes/no/[fingerprint])? ";

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("todex-askpass-{}", Uuid::new_v4().simple()))
    }

    fn answer(prompt: &str, file: &Path) -> (i32, String) {
        let mut out = Vec::new();
        let code = respond(prompt, Some(file), &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn classifies_only_password_and_passphrase_prompts() {
        assert_eq!(
            classify("alice@db's password: "),
            Some(Credential::Password)
        );
        assert_eq!(classify("Password:"), Some(Credential::Password));
        assert_eq!(
            classify("Enter passphrase for key '/x/id_ed25519': "),
            Some(Credential::Passphrase)
        );
        assert_eq!(classify(HOST_KEY_PROMPT), None);
        assert_eq!(classify("Verification code: "), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn password_prompt_reads_and_deletes_the_file() {
        let dir = temp_dir();
        let secret = AskpassSecret::create(&dir, "hunter2").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(secret.path()), 0o600);
        }
        assert_eq!(
            answer("Password:", secret.path()),
            (0, "hunter2\n".to_owned())
        );
        assert!(!secret.path().exists());
        // A second prompt finds nothing to answer with.
        assert_eq!(answer("Password:", secret.path()), (1, String::new()));
        drop(secret);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn passphrase_prompt_does_not_consume_the_file() {
        let dir = temp_dir();
        let secret = AskpassSecret::create(&dir, "hunter2").unwrap();
        assert_eq!(
            answer("Enter passphrase for key '/x/id': ", secret.path()),
            (0, "hunter2\n".to_owned())
        );
        assert!(secret.path().exists());
        assert_eq!(
            answer("Password:", secret.path()),
            (0, "hunter2\n".to_owned())
        );
        assert!(!secret.path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refused_prompts_leave_the_file_alone() {
        let dir = temp_dir();
        let secret = AskpassSecret::create(&dir, "hunter2").unwrap();
        for prompt in [HOST_KEY_PROMPT, "Verification code: ", ""] {
            assert_eq!(answer(prompt, secret.path()), (1, String::new()));
        }
        assert!(secret.path().exists());
        let path = secret.path().to_owned();
        drop(secret);
        assert!(!path.exists(), "the guard deletes the file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_or_variable_fails() {
        let dir = temp_dir();
        assert_eq!(answer("Password:", &dir.join("gone")), (1, String::new()));
        assert_eq!(respond("Password:", None, &mut Vec::new()), 1);
    }

    #[test]
    fn purge_removes_leftover_files() {
        let dir = temp_dir();
        let leftover = AskpassSecret::create(&dir, "old").unwrap();
        let path = leftover.path().to_owned();
        std::mem::forget(leftover);
        purge(&dir).unwrap();
        assert!(!path.exists());
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn expiry_deletes_the_file_after_the_deadline() {
        let dir = temp_dir();
        let secret = AskpassSecret::create(&dir, "hunter2").unwrap();
        let path = secret.path().to_owned();
        secret.expire_after(Duration::from_millis(20));
        assert!(path.exists());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
