//! Minimal `ssh_config(5)` reading and writing.
//!
//! TodeX never interprets connection options itself — OpenSSH resolves them
//! (`ssh -G`). This module only discovers concrete `Host` aliases (following
//! `Include`), and validates/renders the hosts TodeX manages in its own file.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// OpenSSH's own `Include` recursion limit.
const MAX_INCLUDE_DEPTH: usize = 16;
const MAX_CONFIG_FILES: usize = 256;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_ALIAS_LEN: usize = 64;
const MAX_VALUE_LEN: usize = 1024;
pub(crate) const MAX_MANAGED_HOSTS: usize = 512;

/// Options that either define structure or run local commands; a managed host
/// must not smuggle them in through the free-form option list.
const FORBIDDEN_OPTIONS: &[&str] = &[
    "host",
    "match",
    "include",
    "localcommand",
    "permitlocalcommand",
    "knownhostscommand",
];
/// Options with a dedicated field on [`ManagedHost`].
const DEDICATED_OPTIONS: &[&str] = &["hostname", "user", "port", "identityfile", "proxyjump"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscoveredAlias {
    pub alias: String,
    pub source: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ManagedHost {
    pub alias: String,
    pub host_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SshOption>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SshOption {
    pub key: String,
    pub value: String,
}

/// Lists concrete aliases (no wildcards or negations) from `config`, following
/// `Include` the way OpenSSH does for a user configuration file: relative
/// paths resolve against `~/.ssh`, globs expand in sorted order.
pub(crate) fn discover_aliases(config: &Path, home: &Path) -> Vec<DiscoveredAlias> {
    let mut walker = Walker {
        ssh_dir: home.join(".ssh"),
        home: home.to_path_buf(),
        seen_aliases: HashSet::new(),
        files_read: 0,
        aliases: Vec::new(),
    };
    walker.walk(config, 0);
    walker.aliases
}

struct Walker {
    ssh_dir: PathBuf,
    home: PathBuf,
    seen_aliases: HashSet<String>,
    files_read: usize,
    aliases: Vec<DiscoveredAlias>,
}

impl Walker {
    fn walk(&mut self, path: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH || self.files_read >= MAX_CONFIG_FILES {
            return;
        }
        let Some(text) = read_bounded(path) else {
            return;
        };
        self.files_read += 1;
        for line in text.lines() {
            let Some((keyword, args)) = parse_line(line) else {
                continue;
            };
            match keyword.as_str() {
                "host" => {
                    for pattern in args {
                        if is_concrete_alias(&pattern) && self.seen_aliases.insert(pattern.clone())
                        {
                            self.aliases.push(DiscoveredAlias {
                                alias: pattern,
                                source: path.to_path_buf(),
                            });
                        }
                    }
                }
                "include" => {
                    for argument in args {
                        for included in self.expand_include(&argument) {
                            self.walk(&included, depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn expand_include(&self, argument: &str) -> Vec<PathBuf> {
        let expanded = if argument == "~" {
            self.home.clone()
        } else if let Some(rest) = argument.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            let path = PathBuf::from(argument);
            if path.is_absolute() {
                path
            } else {
                self.ssh_dir.join(path)
            }
        };
        expand_glob(&expanded)
    }
}

fn read_bounded(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Expands `*`/`?` per path component; matches are sorted like glob(3).
fn expand_glob(path: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::new()];
    for component in path.components() {
        let part = component.as_os_str().to_string_lossy();
        if !part.contains(['*', '?']) {
            for candidate in &mut candidates {
                candidate.push(component.as_os_str());
            }
            continue;
        }
        let mut next = Vec::new();
        for candidate in &candidates {
            let Ok(entries) = fs::read_dir(candidate) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.') && wildcard_match(&part, name))
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|name| candidate.join(name)));
        }
        candidates = next;
    }
    candidates
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(star) = star {
            p = star + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

fn is_concrete_alias(pattern: &str) -> bool {
    !pattern.is_empty() && !pattern.contains(['*', '?', '!']) && valid_alias(pattern)
}

/// Splits one config line into a lower-cased keyword and its arguments.
/// Supports `Keyword value`, `Keyword=value`, comments, and double quotes.
fn parse_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let split = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = line[..split].to_ascii_lowercase();
    let mut rest = line[split..].trim_start();
    if let Some(stripped) = rest.strip_prefix('=') {
        rest = stripped.trim_start();
    }
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    for c in rest.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '#' if !in_quotes && !has_token => break,
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        args.push(current);
    }
    Some((keyword, args))
}

pub(crate) fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= MAX_ALIAS_LEN
        && !alias.starts_with('-')
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':'))
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::InvalidRequest(message.into())
}

fn validate_token(field: &str, value: &str, max: usize) -> Result<(), AppError> {
    if value.is_empty() || value.len() > max {
        return Err(invalid(format!("{field} must be 1-{max} characters")));
    }
    if value.starts_with('-') {
        return Err(invalid(format!("{field} must not start with '-'")));
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '"')
    {
        return Err(invalid(format!(
            "{field} must not contain whitespace, quotes or control characters"
        )));
    }
    Ok(())
}

fn validate_value(field: &str, value: &str) -> Result<(), AppError> {
    if value.trim().is_empty() || value.len() > MAX_VALUE_LEN {
        return Err(invalid(format!(
            "{field} must be 1-{MAX_VALUE_LEN} characters"
        )));
    }
    if value.chars().any(|c| c.is_control() || c == '"') {
        return Err(invalid(format!(
            "{field} must not contain quotes or control characters"
        )));
    }
    Ok(())
}

impl ManagedHost {
    /// Trims optional fields to `None` and rejects anything that could break
    /// out of its line in the rendered config or inject ssh arguments.
    pub(crate) fn normalized(mut self) -> Result<Self, AppError> {
        fn clean(value: Option<String>) -> Option<String> {
            value
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }
        self.alias = self.alias.trim().to_owned();
        self.host_name = self.host_name.trim().to_owned();
        self.user = clean(self.user);
        self.identity_file = clean(self.identity_file);
        self.proxy_jump = clean(self.proxy_jump);

        if !valid_alias(&self.alias) {
            return Err(invalid(
                "alias must be 1-64 characters of letters, digits, '.', '_', '-', '@' or ':' and not start with '-'",
            ));
        }
        validate_token("hostName", &self.host_name, 255)?;
        if let Some(user) = &self.user {
            validate_token("user", user, 64)?;
        }
        if self.port == Some(0) {
            return Err(invalid("port must be 1-65535"));
        }
        if let Some(identity_file) = &self.identity_file {
            validate_value("identityFile", identity_file)?;
        }
        if let Some(proxy_jump) = &self.proxy_jump {
            validate_token("proxyJump", proxy_jump, 512)?;
        }
        if self.options.len() > 32 {
            return Err(invalid("at most 32 extra options are allowed"));
        }
        for option in &mut self.options {
            option.key = option.key.trim().to_owned();
            option.value = option.value.trim().to_owned();
            let key = option.key.to_ascii_lowercase();
            if option.key.is_empty()
                || option.key.len() > 64
                || !option.key.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Err(invalid(format!("invalid option name: {}", option.key)));
            }
            if FORBIDDEN_OPTIONS.contains(&key.as_str()) {
                return Err(invalid(format!("option {} is not allowed", option.key)));
            }
            if DEDICATED_OPTIONS.contains(&key.as_str()) {
                return Err(invalid(format!(
                    "option {} has a dedicated field",
                    option.key
                )));
            }
            validate_value(&option.key, &option.value)?;
        }
        Ok(self)
    }

    fn render(&self, out: &mut String) {
        out.push_str(&format!("Host {}\n", self.alias));
        out.push_str(&format!("  HostName {}\n", self.host_name));
        if let Some(user) = &self.user {
            out.push_str(&format!("  User {user}\n"));
        }
        if let Some(port) = self.port {
            out.push_str(&format!("  Port {port}\n"));
        }
        if let Some(identity_file) = &self.identity_file {
            out.push_str(&format!("  IdentityFile \"{identity_file}\"\n"));
        }
        if let Some(proxy_jump) = &self.proxy_jump {
            out.push_str(&format!("  ProxyJump {proxy_jump}\n"));
        }
        for option in &self.options {
            out.push_str(&format!("  {} \"{}\"\n", option.key, option.value));
        }
    }
}

pub(crate) fn render_managed_hosts(hosts: &[ManagedHost]) -> String {
    let mut out = String::from(
        "# Generated by TodeX from its SSH host list. Manual edits are overwritten.\n",
    );
    for host in hosts {
        out.push('\n');
        host.render(&mut out);
    }
    out
}

/// The `-F` file every TodeX ssh invocation uses: TodeX-managed hosts first
/// (first value wins in ssh_config), then the user's and the system config,
/// so connections behave like a plain `ssh` from the user's shell.
pub(crate) fn render_wrapper_config(managed: &Path, user: &Path, system: Option<&Path>) -> String {
    let mut out = String::from("# Generated by TodeX. Do not edit.\n");
    for path in [Some(managed), Some(user), system].into_iter().flatten() {
        out.push_str(&format!("Include \"{}\"\n", config_path_text(path)));
    }
    out
}

fn config_path_text(path: &Path) -> String {
    // ssh_config treats backslashes literally inside quotes on Windows, but
    // forward slashes are accepted everywhere and avoid escaping surprises.
    path.to_string_lossy().replace('\\', "/")
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnippetImport {
    pub hosts: Vec<ManagedHost>,
    pub errors: Vec<String>,
}

/// Parses pasted `Host` blocks into managed hosts. Wildcard patterns, `Match`
/// and `Include` are reported instead of imported.
pub(crate) fn parse_snippet(text: &str) -> SnippetImport {
    let mut import = SnippetImport::default();
    let mut current: Vec<ManagedHost> = Vec::new();
    let mut in_block = false;
    let flush = |current: &mut Vec<ManagedHost>, import: &mut SnippetImport| {
        for mut host in current.drain(..) {
            if host.host_name.is_empty() {
                // Like ssh, a block without HostName connects to the alias.
                host.host_name = host.alias.clone();
            }
            push_normalized(host, import);
        }
    };
    for (index, line) in text.lines().enumerate() {
        let Some((keyword, args)) = parse_line(line) else {
            continue;
        };
        let line_number = index + 1;
        match keyword.as_str() {
            "host" => {
                flush(&mut current, &mut import);
                in_block = true;
                for pattern in args {
                    if is_concrete_alias(&pattern) {
                        current.push(ManagedHost {
                            alias: pattern,
                            ..ManagedHost::default()
                        });
                    } else {
                        import
                            .errors
                            .push(format!("line {line_number}: skipped pattern {pattern}"));
                    }
                }
            }
            "match" | "include" => {
                flush(&mut current, &mut import);
                in_block = false;
                import.errors.push(format!(
                    "line {line_number}: {keyword} blocks are not imported"
                ));
            }
            _ if !in_block => import.errors.push(format!(
                "line {line_number}: option outside a Host block ignored"
            )),
            _ if current.is_empty() => {}
            _ => {
                let value = args.join(" ");
                for host in &mut current {
                    match keyword.as_str() {
                        "hostname" => host.host_name = value.clone(),
                        "user" => host.user = Some(value.clone()),
                        "port" => match value.parse::<u16>() {
                            Ok(port) => host.port = Some(port),
                            Err(_) => import
                                .errors
                                .push(format!("line {line_number}: invalid port {value}")),
                        },
                        "identityfile" => host.identity_file = Some(value.clone()),
                        "proxyjump" => host.proxy_jump = Some(value.clone()),
                        _ => {
                            let key = line
                                .trim()
                                .split(|c: char| c.is_whitespace() || c == '=')
                                .next()
                                .unwrap_or_default()
                                .to_owned();
                            host.options.push(SshOption {
                                key,
                                value: value.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    flush(&mut current, &mut import);
    import
}

fn push_normalized(host: ManagedHost, import: &mut SnippetImport) {
    let alias = host.alias.clone();
    match host.normalized() {
        Ok(host) => import.hosts.push(host),
        Err(error) => import.errors.push(format!("{alias}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("todex-ssh-config-{}", Uuid::new_v4()));
        fs::create_dir_all(home.join(".ssh/conf.d")).unwrap();
        home
    }

    #[test]
    fn discovers_concrete_aliases_through_includes() {
        let home = temp_home();
        let ssh = home.join(".ssh");
        fs::write(
            ssh.join("config"),
            "Host *\n  ServerAliveInterval 30\n\nHost web db !bastion\n  User ops\n\
             Include conf.d/*.conf\nMatch host web\n  Port 2222\nHost=\"quoted\" # trailing\n",
        )
        .unwrap();
        fs::write(ssh.join("conf.d/b.conf"), "Host beta\n").unwrap();
        fs::write(
            ssh.join("conf.d/a.conf"),
            "Host alpha web\nInclude ~/.ssh/conf.d/a.conf\n",
        )
        .unwrap();
        fs::write(ssh.join("conf.d/skip.txt"), "Host skipped\n").unwrap();

        let aliases: Vec<String> = discover_aliases(&ssh.join("config"), &home)
            .into_iter()
            .map(|alias| alias.alias)
            .collect();
        assert_eq!(aliases, ["web", "db", "alpha", "beta", "quoted"]);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_config_has_no_aliases() {
        let home = temp_home();
        assert!(discover_aliases(&home.join(".ssh/config"), &home).is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("*.conf", "a.conf"));
        assert!(wildcard_match("h?st", "host"));
        assert!(!wildcard_match("*.conf", "a.txt"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn rejects_injection_in_managed_hosts() {
        let base = ManagedHost {
            alias: "web".into(),
            host_name: "10.0.0.1".into(),
            ..ManagedHost::default()
        };
        assert!(base.clone().normalized().is_ok());
        for host in [
            ManagedHost {
                alias: "-oProxyCommand=x".into(),
                ..base.clone()
            },
            ManagedHost {
                host_name: "a\nHost evil".into(),
                ..base.clone()
            },
            ManagedHost {
                user: Some("-l".into()),
                ..base.clone()
            },
            ManagedHost {
                options: vec![SshOption {
                    key: "LocalCommand".into(),
                    value: "touch /tmp/x".into(),
                }],
                ..base.clone()
            },
            ManagedHost {
                identity_file: Some("a\"b".into()),
                ..base.clone()
            },
        ] {
            assert!(host.normalized().is_err());
        }
    }

    #[test]
    fn renders_managed_hosts_and_wrapper() {
        let host = ManagedHost {
            alias: "web".into(),
            host_name: "10.0.0.1".into(),
            user: Some("ops".into()),
            port: Some(2222),
            identity_file: Some("~/.ssh/id ed25519".into()),
            proxy_jump: Some("bastion".into()),
            options: vec![SshOption {
                key: "ServerAliveInterval".into(),
                value: "30".into(),
            }],
        };
        let rendered = render_managed_hosts(&[host]);
        assert!(rendered.contains(
            "Host web\n  HostName 10.0.0.1\n  User ops\n  Port 2222\n  IdentityFile \"~/.ssh/id ed25519\"\n  ProxyJump bastion\n  ServerAliveInterval \"30\"\n"
        ));
        let wrapper = render_wrapper_config(
            Path::new("/data/ssh/hosts.conf"),
            Path::new("/home/u/.ssh/config"),
            Some(Path::new("/etc/ssh/ssh_config")),
        );
        assert_eq!(
            wrapper,
            "# Generated by TodeX. Do not edit.\nInclude \"/data/ssh/hosts.conf\"\nInclude \"/home/u/.ssh/config\"\nInclude \"/etc/ssh/ssh_config\"\n"
        );
    }

    #[test]
    fn imports_snippets_and_reports_skipped_parts() {
        let import = parse_snippet(
            "User stray\nHost web web2 *.corp\n  HostName 10.0.0.1\n  Port 2200\n  \
             ServerAliveInterval 15\nHost bare\nMatch all\n  User x\nHost bad\n  LocalCommand id\n",
        );
        let aliases: Vec<&str> = import.hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["web", "web2", "bare"]);
        assert_eq!(import.hosts[0].port, Some(2200));
        assert_eq!(import.hosts[0].options[0].key, "ServerAliveInterval");
        assert_eq!(import.hosts[2].host_name, "bare");
        assert_eq!(import.errors.len(), 5, "{:?}", import.errors);
    }
}
