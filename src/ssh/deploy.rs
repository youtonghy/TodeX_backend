//! Deploys a key from the backend user's `~/.ssh` to a managed host: the
//! public key is appended to the remote `~/.ssh/authorized_keys`, the host is
//! switched to sign in with that key, the switch is kept only once the key
//! really logs in, and optionally the server's password login is turned off
//! (`sshd_config` edited, checked with `sshd -t`, sshd restarted).
//!
//! Remote work runs fixed `sh` scripts. Data (the public key, a sudo
//! password) reaches them on stdin only, never in argv or the environment.
//! The scripts need a POSIX `sh` on the remote side, reached through a login
//! shell that understands single quotes (sh, bash, zsh, fish).

use std::{
    ffi::OsString,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::{
    keys, test_result, validate_password, ConnectionTestResult, ManagedHost, SshAuthMethod,
    SshFailureKind, SshMode, SshService, ERROR_DETAIL_LIMIT, TEST_LIMITS,
};
use crate::{
    error::AppError,
    external_command::{self, bounded_text, prepare_captured, CommandLimits, CommandOutput},
};

const INSTALL_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(30),
    output_limit: 64 * 1024,
};
/// `sshd -t`, a service restart and `sshd -T` can take a while on slow hosts.
const SSHD_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(90),
    output_limit: 64 * 1024,
};

/// Appends the key read from stdin unless its type and body are present.
/// Exit 64: bad input, 65: `~/.ssh` could not be written.
const INSTALL_SCRIPT: &str = r#"set -f
umask 077
IFS= read -r key || [ -n "$key" ] || exit 64
set -- $key
case "$1" in ssh-*|ecdsa-*|sk-*) ;; *) exit 64 ;; esac
[ -n "$2" ] || exit 64
body="$1 $2"
dir="$HOME/.ssh"
file="$dir/authorized_keys"
mkdir -p "$dir" || exit 65
chmod 700 "$dir" || exit 65
if [ -f "$file" ] && grep -qF -- "$body" "$file"; then
  echo todex-key-present
  exit 0
fi
if [ -s "$file" ] && [ -n "$(tail -c 1 "$file")" ]; then
  echo >> "$file" || exit 65
fi
printf '%s\n' "$key" >> "$file" || exit 65
chmod 600 "$file" || exit 65
echo todex-key-added
exit 0"#;

/// Runs as root. Puts a marked block first in `sshd_config` (sshd uses the
/// first value it reads, so the block wins over later lines and includes),
/// checks the result with `sshd -t`, restarts sshd and confirms with
/// `sshd -T`. Any failure before the confirmation restores the backup.
/// Exits 0 or 70-76 only (see [`SshdExit`]).
const SSHD_SCRIPT: &str = r#"set -f
[ "$(uname -s)" = Linux ] || exit 70
conf=/etc/ssh/sshd_config
[ -f "$conf" ] || exit 71
sshd=
for candidate in "$(command -v sshd 2>/dev/null)" /usr/sbin/sshd /usr/local/sbin/sshd /sbin/sshd; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then sshd=$candidate; break; fi
done
[ -n "$sshd" ] || exit 71
begin='# >>> TodeX: password login disabled >>>'
end='# <<< TodeX <<<'
backup="$conf.todex-$(date +%Y%m%d%H%M%S)"
cp -p "$conf" "$backup" || exit 76
rest=$(awk -v b="$begin" -v e="$end" '$0 == b { skip = 1; next } skip && $0 == e { skip = 0; next } !skip' "$backup") || exit 76
if ! printf '%s\n' "$begin" 'PasswordAuthentication no' 'KbdInteractiveAuthentication no' "$end" "$rest" > "$conf"; then
  cat "$backup" > "$conf"
  exit 76
fi
if ! "$sshd" -t -f "$conf" >&2; then cat "$backup" > "$conf"; exit 72; fi
restarted=
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
  for unit in sshd ssh; do
    if systemctl restart "$unit" >/dev/null 2>&1; then restarted=1; break; fi
  done
fi
if [ -z "$restarted" ] && command -v service >/dev/null 2>&1; then
  for unit in sshd ssh; do
    if service "$unit" restart >/dev/null 2>&1; then restarted=1; break; fi
  done
fi
if [ -z "$restarted" ] && command -v rc-service >/dev/null 2>&1; then
  rc-service sshd restart >/dev/null 2>&1 && restarted=1
fi
if [ -z "$restarted" ]; then cat "$backup" > "$conf"; exit 73; fi
effective=$("$sshd" -T -f "$conf" 2>/dev/null | awk '$1 == "passwordauthentication" { print $2 }')
[ "$effective" = no ] || exit 74
echo todex-password-login-disabled
exit 0"#;

/// Runs [`SSHD_SCRIPT`] (`$1`) as root: directly, through passwordless sudo,
/// or through sudo with the password on stdin's first line. Exit 75 when
/// none of these is possible.
const ELEVATE_SCRIPT: &str = r#"IFS= read -r pass || true
if [ "$(id -u)" = 0 ]; then sh -c "$1"; exit $?; fi
command -v sudo >/dev/null 2>&1 || exit 75
if sudo -n true >/dev/null 2>&1; then
  sudo -n sh -c "$1"
  rc=$?
else
  [ -n "$pass" ] || exit 75
  printf '%s\n' "$pass" | sudo -S -p '' sh -c "$1"
  rc=$?
fi
pass=
[ "$rc" = 0 ] && exit 0
[ "$rc" -ge 70 ] && [ "$rc" -le 76 ] && exit "$rc"
exit 75"#;

/// Body of `POST /v2/ssh/keys/{name}/deploy`. No `Debug`: it carries a
/// password.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct KeyDeployRequest {
    pub alias: String,
    /// Login (and sudo) password of a password host; blank means none.
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub disable_password_login: bool,
}

impl Drop for KeyDeployRequest {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// Which step of a failed deploy went wrong.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DeployStage {
    /// Signing in with the host's current method or writing the key.
    Install,
    /// The installed key does not log in; the host was left unchanged.
    Verify,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeyDeployResult {
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<DeployStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<SshFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `authorized_keys` already held the key.
    pub already_installed: bool,
    /// The key logged in on its own. False for encrypted keys outside the
    /// agent, which cannot be tried without their passphrase.
    pub verified: bool,
    /// The host as saved afterwards (on failure, as it was before).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<ManagedHost>,
    /// Present when `disablePasswordLogin` was requested and the key was
    /// deployed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_login: Option<PasswordLoginOutcome>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PasswordLoginOutcome {
    pub status: PasswordLoginStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<PasswordLoginReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PasswordLoginStatus {
    Disabled,
    Skipped,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PasswordLoginReason {
    /// The key was not verified, so turning passwords off could lock the
    /// user out.
    NotVerified,
    /// Only Linux servers are supported.
    UnsupportedOs,
    /// No `/etc/ssh/sshd_config` or `sshd` binary.
    NoSshd,
    /// `sshd -t` rejected the edited file; the original was restored.
    ConfigInvalid,
    /// No service manager restarted sshd; the original was restored.
    RestartFailed,
    /// `sshd -T` still reports password login after the restart.
    StillEnabled,
    /// Neither root, passwordless sudo nor sudo with the login password.
    NeedsRoot,
    /// The key-based connection for this step failed.
    Connection,
    Other,
}

impl PasswordLoginOutcome {
    fn new(
        status: PasswordLoginStatus,
        reason: Option<PasswordLoginReason>,
        detail: Option<String>,
    ) -> Self {
        Self {
            status,
            reason,
            detail,
        }
    }

    fn failed(reason: PasswordLoginReason, detail: Option<String>) -> Self {
        Self::new(PasswordLoginStatus::Failed, Some(reason), detail)
    }
}

/// What a remote script run came to.
enum ScriptOutcome {
    Finished(CommandOutput),
    /// ssh itself failed (exit 255, timeout, spawn error).
    Connection(ConnectionTestResult),
}

impl SshService {
    /// Deploys key `name` to managed host `request.alias`; see the module
    /// docs. Errors are for requests that cannot start (unknown key or host,
    /// unusable key); connection and remote failures come back as results.
    pub async fn deploy_key(
        &self,
        name: &str,
        request: KeyDeployRequest,
    ) -> Result<KeyDeployResult, AppError> {
        keys::valid_key_name(name)?;
        let started = Instant::now();
        let alias = request.alias.trim().to_owned();
        let disable_password_login = request.disable_password_login;
        let password = request
            .password
            .as_deref()
            .filter(|password| !password.is_empty())
            .map(|password| Zeroizing::new(password.to_owned()));
        drop(request);
        if let Some(password) = &password {
            validate_password(password)?;
        }
        let host = self.deploy_target(&alias).await?;
        let key = self.key_view(name).await?;
        let private_path = key.private_key_path.clone().ok_or_else(|| {
            AppError::InvalidRequest(format!("ssh key {name} has no private key file"))
        })?;
        let public = key.public_key.clone().ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "the public half of ssh key {name} is unknown; add {name}.pub next to it"
            ))
        })?;
        let can_verify = !key.encrypted.unwrap_or(false) || key.loaded_in_agent;
        let failed = |stage, result: ConnectionTestResult, host: ManagedHost| KeyDeployResult {
            ok: false,
            duration_ms: started.elapsed().as_millis() as u64,
            stage: Some(stage),
            failure: result.failure,
            detail: result.detail,
            already_installed: false,
            verified: false,
            host: Some(host),
            password_login: None,
        };

        let already_installed = match self
            .install_public_key(&alias, &public, password.as_deref().map(String::as_str))
            .await?
        {
            Ok(already) => already,
            Err(result) => return Ok(failed(DeployStage::Install, result, host)),
        };

        let mut next = host.clone();
        next.auth_method = SshAuthMethod::Key;
        next.identity_file = Some(private_path.clone());
        let switched = if next == host {
            host.clone()
        } else {
            self.update_host(&alias, next).await?
        };
        let mut verified = false;
        if can_verify {
            let check = self.key_login(&alias, &private_path).await?;
            if !check.ok {
                self.restore_host(&alias, &switched, &host).await;
                return Ok(failed(DeployStage::Verify, check, host));
            }
            verified = true;
        }

        let password_login = if !disable_password_login {
            None
        } else if !verified {
            Some(PasswordLoginOutcome::new(
                PasswordLoginStatus::Skipped,
                Some(PasswordLoginReason::NotVerified),
                None,
            ))
        } else {
            Some(
                self.disable_password_login(
                    &alias,
                    &private_path,
                    password.as_deref().map(String::as_str),
                )
                .await?,
            )
        };
        tracing::info!(
            alias = %alias,
            key = %name,
            verified,
            already_installed,
            password_login = ?password_login.as_ref().map(|outcome| outcome.status),
            "ssh key deployed"
        );
        Ok(KeyDeployResult {
            ok: true,
            duration_ms: started.elapsed().as_millis() as u64,
            stage: None,
            failure: None,
            detail: None,
            already_installed,
            verified,
            host: Some(switched),
            password_login,
        })
    }

    /// The managed host `alias`; `~/.ssh/config` hosts are read-only.
    async fn deploy_target(&self, alias: &str) -> Result<ManagedHost, AppError> {
        let managed = self
            .inner
            .store
            .read()
            .await
            .managed_hosts
            .iter()
            .find(|host| host.alias == alias)
            .cloned();
        if let Some(host) = managed {
            return Ok(host);
        }
        self.require_host(alias).await?;
        Err(AppError::InvalidRequest(format!(
            "ssh host {alias} comes from ~/.ssh/config; only hosts managed by TodeX can switch to a key"
        )))
    }

    /// Puts the pre-deploy host back unless it was edited meanwhile.
    async fn restore_host(&self, alias: &str, switched: &ManagedHost, original: &ManagedHost) {
        if switched == original {
            return;
        }
        let current = self
            .inner
            .store
            .read()
            .await
            .managed_hosts
            .iter()
            .find(|host| host.alias == alias)
            .cloned();
        if current.as_ref() != Some(switched) {
            return;
        }
        if let Err(error) = self.update_host(alias, original.clone()).await {
            tracing::warn!(alias = %alias, %error, "could not restore ssh host after a failed key check");
        }
    }

    /// Appends `public` to the remote `authorized_keys`, signing in the way
    /// the host does today (with `password` on a connection of its own).
    /// `Ok(true)` when the key was already there.
    async fn install_public_key(
        &self,
        alias: &str,
        public: &str,
        password: Option<&str>,
    ) -> Result<Result<bool, ConnectionTestResult>, AppError> {
        // The password file lives until ssh exited.
        let (mut command, _secret) = match password {
            Some(password) => {
                let mut command = self.unshared_command(SshMode::Askpass);
                let (vars, secret) = self.askpass_env(alias, password).await?;
                command.envs(vars);
                (command, Some(secret))
            }
            None => (self.command(SshMode::Batch), None),
        };
        command
            .arg("--")
            .arg(alias)
            .arg(remote_sh(INSTALL_SCRIPT, &[]));
        prepare_captured(&mut command, true);
        let stdin = format!("{public}\n").into_bytes();
        Ok(match run_script(command, stdin, INSTALL_LIMITS).await? {
            ScriptOutcome::Connection(result) => Err(result),
            ScriptOutcome::Finished(output) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).contains("todex-key-present"))
            }
            ScriptOutcome::Finished(output) => Err(remote_failure(
                &output,
                "could not write ~/.ssh/authorized_keys",
            )),
        })
    }

    /// Runs `exit 0` signing in with the key at `private_path` only, on a
    /// connection of its own.
    async fn key_login(
        &self,
        alias: &str,
        private_path: &str,
    ) -> Result<ConnectionTestResult, AppError> {
        let mut command = self.unshared_command(SshMode::Batch);
        command
            .args(key_only_options(private_path))
            .arg("--")
            .arg(alias)
            .arg("exit 0");
        prepare_captured(&mut command, false);
        let started = Instant::now();
        let outcome = external_command::run(command, None, TEST_LIMITS).await;
        test_result(outcome, started.elapsed())
    }

    /// Turns off password login on the server, signed in with the verified
    /// key; `password` doubles as the sudo password.
    async fn disable_password_login(
        &self,
        alias: &str,
        private_path: &str,
        password: Option<&str>,
    ) -> Result<PasswordLoginOutcome, AppError> {
        let mut command = self.unshared_command(SshMode::Batch);
        command
            .args(key_only_options(private_path))
            .arg("--")
            .arg(alias)
            .arg(remote_sh(ELEVATE_SCRIPT, &[SSHD_SCRIPT]));
        prepare_captured(&mut command, true);
        let stdin = format!("{}\n", password.unwrap_or_default()).into_bytes();
        let output = match run_script(command, stdin, SSHD_LIMITS).await? {
            ScriptOutcome::Connection(result) => {
                return Ok(PasswordLoginOutcome::failed(
                    PasswordLoginReason::Connection,
                    result.detail,
                ))
            }
            ScriptOutcome::Finished(output) => output,
        };
        let detail = || {
            Some(
                bounded_text(&output.stderr, ERROR_DETAIL_LIMIT)
                    .trim()
                    .to_owned(),
            )
            .filter(|detail| !detail.is_empty())
        };
        let reason = match output.status.code() {
            Some(0) => None,
            Some(70) => Some(PasswordLoginReason::UnsupportedOs),
            Some(71) => Some(PasswordLoginReason::NoSshd),
            Some(72) => Some(PasswordLoginReason::ConfigInvalid),
            Some(73) => Some(PasswordLoginReason::RestartFailed),
            Some(74) => Some(PasswordLoginReason::StillEnabled),
            Some(75) => Some(PasswordLoginReason::NeedsRoot),
            _ => Some(PasswordLoginReason::Other),
        };
        if let Some(reason) = reason {
            return Ok(PasswordLoginOutcome::failed(reason, detail()));
        }
        // sshd restarted: the key must still get in.
        let check = self.key_login(alias, private_path).await?;
        Ok(if check.ok {
            PasswordLoginOutcome::new(PasswordLoginStatus::Disabled, None, None)
        } else {
            PasswordLoginOutcome::failed(PasswordLoginReason::Connection, check.detail)
        })
    }
}

async fn run_script(
    command: tokio::process::Command,
    stdin: Vec<u8>,
    limits: CommandLimits,
) -> Result<ScriptOutcome, AppError> {
    let started = Instant::now();
    Ok(
        match external_command::run(command, Some(stdin), limits).await {
            Ok(output) if output.status.code() == Some(255) => {
                ScriptOutcome::Connection(test_result(Ok(output), started.elapsed())?)
            }
            Ok(output) => ScriptOutcome::Finished(output),
            // A missing ssh binary is an error, anything else a failed connection.
            Err(error) => ScriptOutcome::Connection(test_result(Err(error), started.elapsed())?),
        },
    )
}

/// A script that ran but failed; not an ssh problem, so never classified as
/// an authentication failure (which would ask for another password).
fn remote_failure(output: &CommandOutput, fallback: &str) -> ConnectionTestResult {
    let stderr = bounded_text(&output.stderr, ERROR_DETAIL_LIMIT)
        .trim()
        .to_owned();
    let detail = if stderr.is_empty() {
        format!(
            "{fallback} (exit {})",
            output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string())
        )
    } else {
        stderr
    };
    ConnectionTestResult {
        ok: false,
        duration_ms: 0,
        failure: Some(SshFailureKind::Other),
        detail: Some(detail),
    }
}

/// Options that make ssh try only the key at `path`, and no password.
fn key_only_options(path: &str) -> Vec<OsString> {
    // `-o` values are config lines: quoted for spaces, `%` escaped from
    // token expansion.
    let identity = format!("IdentityFile=\"{}\"", path.replace('%', "%%"));
    [
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "PubkeyAuthentication=yes",
        "-o",
        "PreferredAuthentications=publickey",
        "-o",
        identity.as_str(),
    ]
    .map(OsString::from)
    .into()
}

/// The remote command `sh -c '<script>' todex '<arg>'…`.
fn remote_sh(script: &str, args: &[&str]) -> String {
    let mut line = format!("sh -c {} todex", shell_quote(script));
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    line
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ssh::keys::{KeyGenerateRequest, KeyKind};
    use crate::ssh::tests::{fixture, Fixture};
    use std::os::unix::fs::PermissionsExt;

    /// Replaces the fixture's ssh with one that understands the deploy
    /// scripts: it logs argv to `ssh.log` (one line per run) and each script's stdin to
    /// `stdin-<kind>.log`. Hosts named `bad*` refuse the install login,
    /// `<root>/verify-fail` makes key-only logins fail, `<root>/present`
    /// reports the key as installed and `<root>/sshd-rc` sets the sshd
    /// script's exit code.
    async fn deploy_fixture(user_config: &str) -> Fixture {
        let fixture = fixture(user_config).await;
        let root = fixture.root.display().to_string();
        let script = format!(
            r#"#!/bin/sh
# One line per run: the scripts in argv span several.
printf '%s' "$*" | tr '\n' ' ' >> "{root}/ssh.log"; echo >> "{root}/ssh.log"
last=""; for arg in "$@"; do prev="$last"; last="$arg"; done
case " $* " in
  *" -G "*) printf 'hostname %s.example\nuser ops\nport 22\n' "$last"; exit 0;;
  *" -O "*) exit 255;;
esac
case "$last" in
  *sshd_config*) cat >> "{root}/stdin-sshd.log"; rc=0; [ -f "{root}/sshd-rc" ] && rc=$(cat "{root}/sshd-rc"); exit "$rc";;
  *authorized_keys*)
    cat >> "{root}/stdin-install.log"
    case "$prev" in bad*) echo "ops@$prev: Permission denied (password)." >&2; exit 255;; esac
    [ -f "{root}/present" ] && echo todex-key-present
    exit 0;;
  "exit 0")
    [ -f "{root}/verify-fail" ] && {{ echo "ops@$prev: Permission denied (publickey)." >&2; exit 255; }}
    exit 0;;
esac
exit 0
"#
        );
        let fake = fixture.root.join("fake-ssh");
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    async fn add_host(fixture: &Fixture, alias: &str, auth_method: SshAuthMethod) {
        fixture
            .service
            .create_host(ManagedHost {
                alias: alias.into(),
                host_name: "10.0.0.5".into(),
                identity_file: (auth_method == SshAuthMethod::Key)
                    .then(|| "~/.ssh/old_key".to_owned()),
                auth_method,
                ..ManagedHost::default()
            })
            .await
            .unwrap();
    }

    async fn add_key(fixture: &Fixture, name: &str, passphrase: Option<&str>) -> String {
        let key = fixture
            .service
            .generate_key(KeyGenerateRequest {
                name: name.into(),
                algorithm: KeyKind::Ed25519,
                comment: Some("deploy test".into()),
                passphrase: passphrase.map(str::to_owned),
            })
            .await
            .unwrap();
        key.public_key.unwrap()
    }

    fn request(alias: &str, password: Option<&str>, disable: bool) -> KeyDeployRequest {
        KeyDeployRequest {
            alias: alias.into(),
            password: password.map(str::to_owned),
            disable_password_login: disable,
        }
    }

    fn read(fixture: &Fixture, file: &str) -> String {
        std::fs::read_to_string(fixture.root.join(file)).unwrap_or_default()
    }

    fn private_path(fixture: &Fixture, name: &str) -> String {
        fixture
            .root
            .join("home/.ssh")
            .join(name)
            .display()
            .to_string()
    }

    #[tokio::test]
    async fn password_host_switches_to_the_verified_key() {
        let fixture = deploy_fixture("").await;
        add_host(&fixture, "web", SshAuthMethod::Password).await;
        let public = add_key(&fixture, "deploy_ed", None).await;
        let result = fixture
            .service
            .deploy_key("deploy_ed", request("web", Some("hunter2-secret"), false))
            .await
            .unwrap();
        assert!(result.ok, "{result:?}");
        assert!(result.verified);
        assert!(result.password_login.is_none());
        let host = result.host.unwrap();
        assert_eq!(host.auth_method, SshAuthMethod::Key);
        assert_eq!(
            host.identity_file,
            Some(private_path(&fixture, "deploy_ed"))
        );
        assert_eq!(read(&fixture, "stdin-install.log"), format!("{public}\n"));

        let log = read(&fixture, "ssh.log");
        assert!(!log.contains("hunter2-secret"), "{log}");
        let install = log.lines().find(|l| l.contains("authorized_keys")).unwrap();
        assert!(install.contains("ControlPath=none"), "{install}");
        assert!(install.contains("BatchMode=no"), "{install}");
        let verify = log.lines().find(|l| l.ends_with("web exit 0")).unwrap();
        assert!(verify.contains("-o IdentitiesOnly=yes"), "{verify}");
        assert!(
            verify.contains("PreferredAuthentications=publickey"),
            "{verify}"
        );
        assert!(verify.contains("ControlPath=none"), "{verify}");
        let hosts = std::fs::read_to_string(fixture.root.join("data/ssh/hosts.conf")).unwrap();
        assert!(!hosts.contains("PubkeyAuthentication no"), "{hosts}");
    }

    #[tokio::test]
    async fn failed_logins_and_key_checks_leave_the_host_alone() {
        let fixture = deploy_fixture("").await;
        add_host(&fixture, "bad1", SshAuthMethod::Password).await;
        add_host(&fixture, "web", SshAuthMethod::Password).await;
        add_key(&fixture, "deploy_ed", None).await;

        let refused = fixture
            .service
            .deploy_key("deploy_ed", request("bad1", Some("wrong"), true))
            .await
            .unwrap();
        assert!(!refused.ok);
        assert_eq!(refused.stage, Some(DeployStage::Install));
        assert_eq!(refused.failure, Some(SshFailureKind::AuthenticationFailed));
        assert_eq!(refused.host.unwrap().auth_method, SshAuthMethod::Password);

        std::fs::write(fixture.root.join("verify-fail"), "").unwrap();
        let unverified = fixture
            .service
            .deploy_key("deploy_ed", request("web", Some("pw"), true))
            .await
            .unwrap();
        assert!(!unverified.ok);
        assert_eq!(unverified.stage, Some(DeployStage::Verify));
        let hosts = fixture.service.list_hosts().await.unwrap();
        let web = hosts.iter().find(|h| h.alias == "web").unwrap();
        assert_eq!(
            web.managed.as_ref().unwrap().auth_method,
            SshAuthMethod::Password
        );
        assert!(read(&fixture, "stdin-sshd.log").is_empty());
    }

    #[tokio::test]
    async fn key_hosts_are_overwritten_with_their_current_login() {
        let fixture = deploy_fixture("").await;
        add_host(&fixture, "web", SshAuthMethod::Key).await;
        add_key(&fixture, "deploy_ed", None).await;
        std::fs::write(fixture.root.join("present"), "").unwrap();
        let result = fixture
            .service
            .deploy_key("deploy_ed", request("web", None, false))
            .await
            .unwrap();
        assert!(result.ok && result.already_installed, "{result:?}");
        assert_eq!(
            result.host.unwrap().identity_file,
            Some(private_path(&fixture, "deploy_ed"))
        );
        let log = read(&fixture, "ssh.log");
        let install = log.lines().find(|l| l.contains("authorized_keys")).unwrap();
        assert!(install.contains("BatchMode=yes"), "{install}");
    }

    #[tokio::test]
    async fn password_login_is_disabled_only_after_the_key_works() {
        let fixture = deploy_fixture("").await;
        add_host(&fixture, "web", SshAuthMethod::Password).await;
        add_key(&fixture, "deploy_ed", None).await;
        let result = fixture
            .service
            .deploy_key("deploy_ed", request("web", Some("sudo-secret"), true))
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(
            result.password_login,
            Some(PasswordLoginOutcome::new(
                PasswordLoginStatus::Disabled,
                None,
                None
            ))
        );
        // The sudo password goes to the script's stdin only.
        assert_eq!(read(&fixture, "stdin-sshd.log"), "sudo-secret\n");
        let log = read(&fixture, "ssh.log");
        assert!(!log.contains("sudo-secret"), "{log}");
        let sshd = log.lines().find(|l| l.contains("sshd_config")).unwrap();
        assert!(
            sshd.contains("PreferredAuthentications=publickey"),
            "{sshd}"
        );
        // Checked again after the restart.
        assert_eq!(log.lines().filter(|l| l.ends_with("web exit 0")).count(), 2);

        std::fs::write(fixture.root.join("sshd-rc"), "75").unwrap();
        let denied = fixture
            .service
            .deploy_key("deploy_ed", request("web", None, true))
            .await
            .unwrap();
        assert!(denied.ok);
        assert_eq!(
            denied.password_login.unwrap().reason,
            Some(PasswordLoginReason::NeedsRoot)
        );
        assert_eq!(denied.host.unwrap().auth_method, SshAuthMethod::Key);
    }

    #[tokio::test]
    async fn encrypted_keys_switch_unverified_and_keep_password_login() {
        let fixture = deploy_fixture("").await;
        add_host(&fixture, "web", SshAuthMethod::Password).await;
        add_key(&fixture, "locked", Some("key passphrase")).await;
        let result = fixture
            .service
            .deploy_key("locked", request("web", Some("pw"), true))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(!result.verified);
        assert_eq!(result.host.unwrap().auth_method, SshAuthMethod::Key);
        assert_eq!(
            result.password_login.unwrap().reason,
            Some(PasswordLoginReason::NotVerified)
        );
        let log = read(&fixture, "ssh.log");
        assert!(!log.lines().any(|l| l.ends_with("web exit 0")), "{log}");
        assert!(read(&fixture, "stdin-sshd.log").is_empty());
    }

    #[tokio::test]
    async fn only_managed_hosts_and_existing_keys_are_accepted() {
        let fixture = deploy_fixture("Host from-config\n").await;
        add_host(&fixture, "web", SshAuthMethod::Password).await;
        add_key(&fixture, "deploy_ed", None).await;
        let error = |result: Result<KeyDeployResult, AppError>| result.unwrap_err();
        assert!(matches!(
            error(
                fixture
                    .service
                    .deploy_key("deploy_ed", request("from-config", None, false))
                    .await
            ),
            AppError::InvalidRequest(_)
        ));
        assert!(matches!(
            error(
                fixture
                    .service
                    .deploy_key("deploy_ed", request("nowhere", None, false))
                    .await
            ),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            error(
                fixture
                    .service
                    .deploy_key("missing", request("web", None, false))
                    .await
            ),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            error(
                fixture
                    .service
                    .deploy_key("../etc", request("web", None, false))
                    .await
            ),
            AppError::InvalidRequest(_)
        ));
        assert!(matches!(
            error(
                fixture
                    .service
                    .deploy_key("deploy_ed", request("web", Some("a\nb"), false))
                    .await
            ),
            AppError::InvalidRequest(_)
        ));
    }

    #[test]
    fn remote_commands_quote_their_scripts() {
        assert_eq!(
            remote_sh("echo 'a'", &["b c"]),
            r#"sh -c 'echo '\''a'\''' todex 'b c'"#
        );
        let options: Vec<String> = key_only_options("/home/a b/%d/k")
            .into_iter()
            .map(|option| option.into_string().unwrap())
            .collect();
        assert_eq!(options.last().unwrap(), r#"IdentityFile="/home/a b/%%d/k""#);
    }

    /// The scripts run under the local `sh`: installing twice adds one line.
    #[test]
    fn install_script_is_idempotent() {
        let home =
            std::env::temp_dir().join(format!("todex-deploy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/authorized_keys"), "ssh-rsa AAAAold old").unwrap();
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample deploy test";
        for expected in ["todex-key-added", "todex-key-present"] {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(INSTALL_SCRIPT)
                .env("HOME", &home)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write as _;
            writeln!(child.stdin.take().unwrap(), "{key}").unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
        let file = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        assert_eq!(file, format!("ssh-rsa AAAAold old\n{key}\n"));
        let mode = std::fs::metadata(home.join(".ssh/authorized_keys"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
