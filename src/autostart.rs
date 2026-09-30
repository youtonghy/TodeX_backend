//! Registers the daemon to start when the user logs in: a launchd agent on
//! macOS, a systemd user service on Linux, and the Run registry key on
//! Windows. Every registration launches `daemon-run` in the foreground, so
//! the supervisor owns the process lifecycle while `daemon status`/`stop`
//! keep working through the pid file.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;

use crate::config::Config;

/// Whether a login-time launch is registered, and where it lives.
#[derive(Debug)]
pub struct Registration {
    pub enabled: bool,
    /// The platform registration: a plist path, a systemd unit, or a
    /// registry value.
    pub location: String,
    /// A non-fatal caveat worth showing the user (for example, lingering
    /// could not be enabled).
    pub note: Option<String>,
}

pub fn status() -> Result<Registration> {
    platform::status()
}

pub fn enable(config: &Config) -> Result<Registration> {
    platform::enable(config)
}

pub fn disable() -> Result<Registration> {
    platform::disable()
}

/// The launch arguments shared by `daemon start` and the login entry.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn daemon_run_args(config: &Config) -> Vec<String> {
    crate::daemon::daemon_run_args(config)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .context("$HOME is not set")
}

/// A launchd/systemd launch gets a sparse environment, so the registrations
/// snapshot the PATH the user installed providers under.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn inherited_path() -> Option<String> {
    std::env::var_os("PATH")
        .filter(|path| !path.is_empty())
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
mod platform {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result};

    use super::{daemon_run_args, home_dir, inherited_path, Registration};
    use crate::config::Config;
    use crate::daemon::log_file_path;

    const LABEL: &str = "com.todex.agentd";

    fn plist_path() -> Result<PathBuf> {
        Ok(home_dir()?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// launchctl's persisted per-user overrides, keyed `gui/<uid>/<label>`.
    fn launchctl_target() -> String {
        format!("gui/{}/{LABEL}", unsafe { libc::getuid() })
    }

    pub fn status() -> Result<Registration> {
        let path = plist_path()?;
        Ok(Registration {
            enabled: path.is_file(),
            location: format!("LaunchAgent {}", path.display()),
            note: None,
        })
    }

    pub fn enable(config: &Config) -> Result<Registration> {
        let executable = std::env::current_exe().context("failed to resolve current executable")?;
        let log = log_file_path(&config.data_dir);
        if let Some(directory) = log.parent() {
            fs::create_dir_all(directory).with_context(|| {
                format!("failed to create log directory {}", directory.display())
            })?;
        }
        let path = plist_path()?;
        let directory = path
            .parent()
            .context("LaunchAgents directory has no parent")?;
        fs::create_dir_all(directory).with_context(|| {
            format!(
                "failed to create LaunchAgents directory {}",
                directory.display()
            )
        })?;
        fs::write(
            &path,
            plist_xml(
                &executable,
                &daemon_run_args(config),
                &log,
                inherited_path(),
            ),
        )
        .with_context(|| format!("failed to write {}", path.display()))?;

        // A `launchctl disable` override survives a new plist; clear it so the
        // registration actually loads at the next login. Failure only means
        // the stale override stays, so report it instead of aborting.
        let cleared = Command::new("launchctl")
            .args(["enable", &launchctl_target()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        let mut registration = status()?;
        if !cleared {
            registration.note = Some(
                "could not clear a previous `launchctl disable` override; if autostart was \
                 disabled before, run `launchctl enable gui/$(id -u)/com.todex.agentd`"
                    .to_owned(),
            );
        }
        Ok(registration)
    }

    pub fn disable() -> Result<Registration> {
        // Persist the disabled state without booting out (and killing) a
        // daemon launchd may be running right now.
        let _ = Command::new("launchctl")
            .args(["disable", &launchctl_target()])
            .status();
        let path = plist_path()?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()))
            }
        }
        status()
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    pub(super) fn plist_xml(
        executable: &Path,
        args: &[String],
        log: &Path,
        path_env: Option<String>,
    ) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key>\n\
             \t<string>com.todex.agentd</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n",
        );
        let executable = executable.to_string_lossy();
        xml.push_str(&format!(
            "\t\t<string>{}</string>\n",
            xml_escape(&executable)
        ));
        xml.push_str("\t\t<string>daemon-run</string>\n");
        for arg in args {
            xml.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(arg)));
        }
        xml.push_str("\t</array>\n");
        if let Some(path_env) = path_env {
            xml.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n");
            xml.push_str(&format!(
                "\t\t<string>{}</string>\n\t</dict>\n",
                xml_escape(&path_env)
            ));
        }
        let log = xml_escape(&log.to_string_lossy());
        xml.push_str(&format!(
            "\t<key>RunAtLoad</key>\n\t<true/>\n\
             \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
             \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
             </dict>\n</plist>\n"
        ));
        xml
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{bail, Context, Result};

    use super::{daemon_run_args, home_dir, inherited_path, Registration};
    use crate::config::Config;

    const UNIT_NAME: &str = "todex-agentd.service";

    fn unit_dir() -> Result<PathBuf> {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .map_or_else(|| home_dir().map(|home| home.join(".config")), Ok)?;
        Ok(config_home.join("systemd").join("user"))
    }

    fn unit_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join(UNIT_NAME))
    }

    /// `systemctl --user enable` records this symlink.
    fn enabled_link() -> Result<PathBuf> {
        Ok(unit_dir()?.join("default.target.wants").join(UNIT_NAME))
    }

    fn systemctl(args: &[&str]) -> Result<()> {
        let status = Command::new("systemctl")
            .args(args)
            .status()
            .context("systemctl not found; autostart needs systemd user services")?;
        if !status.success() {
            bail!("`systemctl {}` failed", args.join(" "));
        }
        Ok(())
    }

    pub fn status() -> Result<Registration> {
        Ok(Registration {
            enabled: enabled_link()?.exists(),
            location: format!("systemd user unit {}", unit_path()?.display()),
            note: None,
        })
    }

    pub fn enable(config: &Config) -> Result<Registration> {
        let executable = std::env::current_exe().context("failed to resolve current executable")?;
        let path = unit_path()?;
        let directory = path
            .parent()
            .context("systemd user directory has no parent")?;
        fs::create_dir_all(directory).with_context(|| {
            format!(
                "failed to create systemd user directory {}",
                directory.display()
            )
        })?;
        fs::write(
            &path,
            unit_text(&executable, &daemon_run_args(config), inherited_path()),
        )
        .with_context(|| format!("failed to write {}", path.display()))?;

        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", UNIT_NAME])?;

        // User services normally run only while a session is open; lingering
        // starts the daemon at boot and keeps it across logins.
        let lingering = Command::new("loginctl")
            .args(["enable-linger"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        let mut registration = status()?;
        if !lingering {
            registration.note = Some(
                "loginctl enable-linger failed; the daemon will start at login, not at boot"
                    .to_owned(),
            );
        }
        Ok(registration)
    }

    pub fn disable() -> Result<Registration> {
        // `disable` (not `stop`): a running daemon stays up until `daemon stop`.
        if unit_path()?.exists() {
            let _ = systemctl(&["--user", "disable", UNIT_NAME]);
        }
        match fs::remove_file(unit_path()?) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to remove systemd user unit"),
        }
        let _ = systemctl(&["--user", "daemon-reload"]);
        status()
    }

    /// Unit files expand `%` specifiers and parse quotes/backslashes in
    /// command lines, so values are always quoted with those escaped.
    pub(super) fn systemd_quote(value: &str) -> String {
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('"');
        for ch in value.chars() {
            match ch {
                '"' | '\\' => quoted.push('\\'),
                '%' => {
                    quoted.push('%');
                    quoted.push('%');
                    continue;
                }
                _ => {}
            }
            quoted.push(ch);
        }
        quoted.push('"');
        quoted
    }

    pub(super) fn unit_text(
        executable: &Path,
        args: &[String],
        path_env: Option<String>,
    ) -> String {
        let mut unit =
            String::from("[Unit]\nDescription=TodeX agent daemon\n\n[Service]\nType=simple\n");
        if let Some(path_env) = path_env {
            unit.push_str(&format!(
                "Environment={}\n",
                systemd_quote(&format!("PATH={path_env}"))
            ));
        }
        let mut exec = systemd_quote(&executable.to_string_lossy());
        for arg in args {
            exec.push(' ');
            exec.push_str(&systemd_quote(arg));
        }
        unit.push_str(&format!(
            "ExecStart={exec}\nRestart=on-failure\nRestartSec=3\nSyslogIdentifier=todex-agentd\n\n\
             [Install]\nWantedBy=default.target\n"
        ));
        unit
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::process::Command;

    use anyhow::{bail, Context, Result};

    use super::{daemon_run_args, Registration};
    use crate::config::Config;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "TodeXAgentd";

    fn location() -> String {
        format!("{RUN_KEY}\\{VALUE_NAME}")
    }

    pub fn status() -> Result<Registration> {
        let enabled = Command::new("reg")
            .args(["query", RUN_KEY, "/v", VALUE_NAME])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        Ok(Registration {
            enabled,
            location: location(),
            note: None,
        })
    }

    pub fn enable(config: &Config) -> Result<Registration> {
        let executable = std::env::current_exe().context("failed to resolve current executable")?;
        let mut command_line = format!("\"{}\"", executable.to_string_lossy());
        command_line.push_str(" \"daemon-run\"");
        for arg in daemon_run_args(config) {
            command_line.push_str(&format!(" \"{arg}\""));
        }
        let status = Command::new("reg")
            .args([
                "add",
                RUN_KEY,
                "/v",
                VALUE_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &command_line,
                "/f",
            ])
            .status()
            .context("failed to run `reg add`")?;
        if !status.success() {
            bail!("`reg add` failed for {}", location());
        }
        status()
    }

    pub fn disable() -> Result<Registration> {
        let status = Command::new("reg")
            .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
            .status()
            .context("failed to run `reg delete`")?;
        if !status.success() && status()?.enabled {
            bail!("`reg delete` failed for {}", location());
        }
        status()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use anyhow::{bail, Result};

    use super::Registration;
    use crate::config::Config;

    pub fn status() -> Result<Registration> {
        bail!("login autostart is not supported on this platform")
    }

    pub fn enable(_config: &Config) -> Result<Registration> {
        bail!("login autostart is not supported on this platform")
    }

    pub fn disable() -> Result<Registration> {
        bail!("login autostart is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::path::Path;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::platform;

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_runs_daemon_run_at_login() {
        let args = vec![
            "--host".to_owned(),
            "127.0.0.1".to_owned(),
            "--data-dir".to_owned(),
            "/Users/a&b/.todex-agent".to_owned(),
        ];
        let xml = platform::plist_xml(
            Path::new("/opt/todex agent/todex-agentd"),
            &args,
            Path::new("/data/logs/todex-agentd-daemon.log"),
            Some("/opt/bin:/usr/bin".to_owned()),
        );
        assert!(xml.contains("<string>com.todex.agentd</string>"));
        assert!(xml.contains("<string>daemon-run</string>"));
        assert!(xml.contains("<string>/Users/a&amp;b/.todex-agent</string>"));
        assert!(xml.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(xml.contains("<string>/data/logs/todex-agentd-daemon.log</string>"));
        assert!(xml.contains("<string>/opt/bin:/usr/bin</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_quotes_args_and_escapes_specifiers() {
        assert_eq!(platform::systemd_quote("/a b/c"), "\"/a b/c\"");
        assert_eq!(platform::systemd_quote("100%"), "\"100%%\"");
        assert_eq!(platform::systemd_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");

        let unit = platform::unit_text(
            Path::new("/opt/todex agent/todex-agentd"),
            &[
                "daemon-run".to_owned(),
                "--data-dir".to_owned(),
                "/d dir".to_owned(),
            ],
            Some("/bin".to_owned()),
        );
        assert!(unit.contains("ExecStart=\"/opt/todex agent/todex-agentd\" \"daemon-run\""));
        assert!(unit.contains("Environment=\"PATH=/bin\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
