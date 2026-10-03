//! `SSH_ASKPASS` helper: `todex-agentd ssh-askpass <prompt>`.
//!
//! OpenSSH runs the askpass program as `<program> <prompt>` without extra
//! arguments, so `main` maps that invocation to the hidden `ssh-askpass`
//! subcommand when [`SECRET_ENV`] is present. The secret lives only in the
//! environment of the one ssh child that needs it, never on disk.

use std::io::Write;

/// Set only on an ssh child started with a request-supplied password.
pub(crate) const SECRET_ENV: &str = "TODEX_SSH_ASKPASS_SECRET";

/// The secret to print for `prompt`, or `None` to refuse. Host-key
/// confirmations and anything that is not a password or passphrase prompt
/// (one-time codes, arbitrary questions) are refused.
pub(crate) fn answer<'a>(prompt: &str, secret: Option<&'a str>) -> Option<&'a str> {
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
    let credential = prompt.contains("password") || prompt.contains("passphrase");
    if confirmation || !credential {
        return None;
    }
    secret.filter(|secret| !secret.is_empty())
}

/// Runs the helper; returns the process exit code.
pub(crate) fn run(prompt: Option<&str>) -> i32 {
    let secret = std::env::var(SECRET_ENV).ok();
    let Some(secret) = answer(prompt.unwrap_or_default(), secret.as_deref()) else {
        return 1;
    };
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{secret}").and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Rewrites `todex-agentd <prompt>` (how ssh invokes `SSH_ASKPASS`) into
/// `todex-agentd ssh-askpass <prompt>`. Other invocations are unchanged.
pub(crate) fn rewrite_args(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    if std::env::var_os(SECRET_ENV).is_some() && args.len() == 2 && args[1] != "ssh-askpass" {
        args.insert(1, "ssh-askpass".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_only_password_prompts() {
        let secret = Some("hunter2");
        assert_eq!(answer("alice@db's password: ", secret), Some("hunter2"));
        assert_eq!(answer("Password:", secret), Some("hunter2"));
        assert_eq!(
            answer("Enter passphrase for key '/x/id_ed25519': ", secret),
            Some("hunter2")
        );
        assert_eq!(
            answer(
                "The authenticity of host 'db (10.0.0.5)' can't be established.\n\
                 ED25519 key fingerprint is SHA256:abc.\n\
                 Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
                secret
            ),
            None
        );
        assert_eq!(answer("Verification code: ", secret), None);
        assert_eq!(answer("", secret), None);
        assert_eq!(answer("Password:", None), None);
        assert_eq!(answer("Password:", Some("")), None);
    }
}
