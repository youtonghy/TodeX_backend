//! Minimal `ssh_config(5)` reading and writing.
//!
//! TodeX never interprets connection options itself — OpenSSH resolves them
//! (`ssh -G`). This module only discovers concrete `Host` aliases (following
//! `Include`), and validates/renders the hosts TodeX manages in its own file.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
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

/// Options that define config structure or run commands on every connection
/// setup outside ssh's own proxying; kept out of the free-form option list.
/// (`ProxyCommand` and provider libraries stay allowed: they are ordinary
/// connection settings, and only paired devices can edit hosts.)
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
    #[serde(default, skip_serializing_if = "SshAuthMethod::is_key")]
    pub auth_method: SshAuthMethod,
}

/// How a managed host authenticates. Passwords themselves are never stored:
/// `Password` only tells OpenSSH to skip keys and ask for one.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SshAuthMethod {
    #[default]
    Key,
    Password,
}

impl SshAuthMethod {
    fn is_key(&self) -> bool {
        *self == Self::Key
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SshOption {
    pub key: String,
    pub value: String,
}

pub(crate) const SERVER_ALIVE_INTERVAL_RANGE: std::ops::RangeInclusive<u32> = 5..=600;
pub(crate) const SERVER_ALIVE_COUNT_MAX_RANGE: std::ops::RangeInclusive<u32> = 1..=10;

/// Connection defaults for every TodeX ssh invocation. Rendered after the
/// user's config, so a value set there for a host still wins.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionSettings {
    /// Off by default: a keepalive drops sessions on outages longer than
    /// interval × count that would otherwise recover.
    pub server_alive_enabled: bool,
    /// Seconds between keepalive probes. Kept while disabled.
    pub server_alive_interval: u32,
    /// Unanswered probes before ssh closes the connection.
    pub server_alive_count_max: u32,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            server_alive_enabled: false,
            server_alive_interval: 15,
            server_alive_count_max: 3,
        }
    }
}

impl ConnectionSettings {
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if !SERVER_ALIVE_INTERVAL_RANGE.contains(&self.server_alive_interval) {
            return Err(AppError::InvalidRequest(format!(
                "serverAliveInterval must be between {} and {} seconds",
                SERVER_ALIVE_INTERVAL_RANGE.start(),
                SERVER_ALIVE_INTERVAL_RANGE.end()
            )));
        }
        if !SERVER_ALIVE_COUNT_MAX_RANGE.contains(&self.server_alive_count_max) {
            return Err(AppError::InvalidRequest(format!(
                "serverAliveCountMax must be between {} and {}",
                SERVER_ALIVE_COUNT_MAX_RANGE.start(),
                SERVER_ALIVE_COUNT_MAX_RANGE.end()
            )));
        }
        Ok(())
    }
}

/// [`AliasCache::discover`] without the cache.
#[cfg(test)]
fn discover_aliases(config: &Path, home: &Path) -> Vec<DiscoveredAlias> {
    discover_with_inputs(config, home).0
}

/// A path's modification time, length and, for a directory, sorted entry
/// names (`None` when it cannot be read), compared to tell whether a
/// discovery input changed.
type Stamp = Option<(Option<SystemTime>, u64, Vec<OsString>)>;

fn stamp(path: &Path) -> Stamp {
    let metadata = fs::metadata(path).ok()?;
    // A directory's mtime misses an entry added within the same clock tick
    // (about 16 ms on Windows, where its length is also 0), so it is stamped
    // by its listing too.
    let mut names = Vec::new();
    if metadata.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            names = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name())
                .collect();
            names.sort();
        }
    }
    Some((metadata.modified().ok(), metadata.len(), names))
}

/// The aliases, and every input they were read from: each config file
/// opened (or tried) and each directory an `Include` glob listed, stamped
/// before it was read.
fn discover_with_inputs(
    config: &Path,
    home: &Path,
) -> (Vec<DiscoveredAlias>, Vec<(PathBuf, Stamp)>) {
    let mut walker = Walker {
        ssh_dir: home.join(".ssh"),
        home: home.to_path_buf(),
        seen_aliases: HashSet::new(),
        files_read: 0,
        aliases: Vec::new(),
        inputs: Vec::new(),
    };
    walker.walk(config, 0);
    (walker.aliases, walker.inputs)
}

/// [`discover_aliases`] for repeated calls (every agent turn checks the
/// hosts): the last result is reused while none of its inputs changed, which
/// costs a `stat` per input (and a listing per globbed directory) instead
/// of re-reading the whole `Include` tree.
#[derive(Default)]
pub(crate) struct AliasCache {
    last: Mutex<Option<CachedAliases>>,
}

struct CachedAliases {
    config: PathBuf,
    home: PathBuf,
    inputs: Vec<(PathBuf, Stamp)>,
    aliases: Vec<DiscoveredAlias>,
}

impl AliasCache {
    /// Lists concrete aliases (no wildcards or negations) from `config`,
    /// following `Include` the way OpenSSH does for a user configuration
    /// file: relative paths resolve against `~/.ssh`, globs expand in sorted
    /// order. Blocking: call from the blocking pool.
    pub(crate) fn discover(&self, config: &Path, home: &Path) -> Vec<DiscoveredAlias> {
        {
            let last = self
                .last
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(cached) = last.as_ref().filter(|cached| {
                cached.config == config
                    && cached.home == home
                    && cached
                        .inputs
                        .iter()
                        .all(|(path, recorded)| stamp(path) == *recorded)
            }) {
                return cached.aliases.clone();
            }
        }
        let (aliases, inputs) = discover_with_inputs(config, home);
        *self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedAliases {
            config: config.to_path_buf(),
            home: home.to_path_buf(),
            inputs,
            aliases: aliases.clone(),
        });
        aliases
    }
}

struct Walker {
    ssh_dir: PathBuf,
    home: PathBuf,
    seen_aliases: HashSet<String>,
    files_read: usize,
    aliases: Vec<DiscoveredAlias>,
    inputs: Vec<(PathBuf, Stamp)>,
}

impl Walker {
    fn walk(&mut self, path: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH || self.files_read >= MAX_CONFIG_FILES {
            return;
        }
        self.inputs.push((path.to_path_buf(), stamp(path)));
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

    fn expand_include(&mut self, argument: &str) -> Vec<PathBuf> {
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
        expand_glob(&expanded, &mut self.inputs)
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
/// Expands `*` and `?` components; every directory listed is recorded in
/// `listed`, since a file appearing there can change the expansion.
fn expand_glob(path: &Path, listed: &mut Vec<(PathBuf, Stamp)>) -> Vec<PathBuf> {
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
            listed.push((candidate.clone(), stamp(candidate)));
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
        self.identity_file = match self.auth_method {
            SshAuthMethod::Key => clean(self.identity_file),
            SshAuthMethod::Password => None,
        };
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
        if self.auth_method == SshAuthMethod::Password {
            // Skipping keys avoids "Too many authentication failures" when
            // the agent holds several keys.
            out.push_str("  PreferredAuthentications keyboard-interactive,password\n");
            out.push_str("  PubkeyAuthentication no\n");
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

/// TodeX's connection defaults as a `Host *` block; empty when none apply.
pub(crate) fn render_connection_defaults(settings: &ConnectionSettings) -> String {
    let mut out = String::from(
        "# Generated by TodeX from its SSH connection settings. Manual edits are overwritten.\n",
    );
    if settings.server_alive_enabled {
        out.push_str(&format!(
            "\nHost *\n  ServerAliveInterval {}\n  ServerAliveCountMax {}\n",
            settings.server_alive_interval, settings.server_alive_count_max
        ));
    }
    out
}

/// The `-F` file every TodeX ssh invocation uses: TodeX-managed hosts first
/// (first value wins in ssh_config), then the user's config, TodeX's
/// connection defaults and the system config, so connections behave like a
/// plain `ssh` from the user's shell and the user's own values win.
pub(crate) fn render_wrapper_config(
    managed: &Path,
    user: &Path,
    defaults: &Path,
    system: Option<&Path>,
) -> String {
    let mut out = String::from("# Generated by TodeX. Do not edit.\n");
    for path in [Some(managed), Some(user), Some(defaults), system]
        .into_iter()
        .flatten()
    {
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
    fn cached_aliases_follow_changes_to_any_included_file() {
        let home = temp_home();
        let ssh = home.join(".ssh");
        fs::create_dir_all(ssh.join("conf.d")).unwrap();
        let cache = AliasCache::default();
        let config = ssh.join("config");
        let aliases = |cache: &AliasCache| -> Vec<String> {
            cache
                .discover(&config, &home)
                .into_iter()
                .map(|alias| alias.alias)
                .collect()
        };
        // A config that does not exist yet is an input too.
        assert!(aliases(&cache).is_empty());
        fs::write(&config, "Host web\nInclude conf.d/*.conf\n").unwrap();
        assert_eq!(aliases(&cache), ["web"]);
        assert_eq!(aliases(&cache), ["web"]);
        // A new file matching the glob, then a changed included file.
        fs::write(ssh.join("conf.d/a.conf"), "Host alpha\n").unwrap();
        assert_eq!(aliases(&cache), ["web", "alpha"]);
        fs::write(ssh.join("conf.d/a.conf"), "Host alpha beta\n").unwrap();
        assert_eq!(aliases(&cache), ["web", "alpha", "beta"]);
        fs::remove_file(ssh.join("conf.d/a.conf")).unwrap();
        assert_eq!(aliases(&cache), ["web"]);
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
            auth_method: SshAuthMethod::Key,
        };
        let rendered = render_managed_hosts(&[host]);
        assert!(rendered.contains(
            "Host web\n  HostName 10.0.0.1\n  User ops\n  Port 2222\n  IdentityFile \"~/.ssh/id ed25519\"\n  ProxyJump bastion\n  ServerAliveInterval \"30\"\n"
        ));
        let wrapper = render_wrapper_config(
            Path::new("/data/ssh/hosts.conf"),
            Path::new("/home/u/.ssh/config"),
            Path::new("/data/ssh/defaults.conf"),
            Some(Path::new("/etc/ssh/ssh_config")),
        );
        assert_eq!(
            wrapper,
            "# Generated by TodeX. Do not edit.\nInclude \"/data/ssh/hosts.conf\"\nInclude \"/home/u/.ssh/config\"\nInclude \"/data/ssh/defaults.conf\"\nInclude \"/etc/ssh/ssh_config\"\n"
        );
    }

    #[test]
    fn password_hosts_skip_keys_and_drop_identity_files() {
        let host: ManagedHost = serde_json::from_str(
            r#"{"alias":"db","hostName":"db.example","identityFile":"~/.ssh/id","authMethod":"password"}"#,
        )
        .unwrap();
        let host = host.normalized().unwrap();
        assert_eq!(host.identity_file, None);
        assert_eq!(
            render_managed_hosts(std::slice::from_ref(&host)),
            "# Generated by TodeX from its SSH host list. Manual edits are overwritten.

Host db
  HostName db.example
  PreferredAuthentications keyboard-interactive,password
  PubkeyAuthentication no
"
        );
        assert_eq!(
            serde_json::to_value(&host).unwrap()["authMethod"],
            "password"
        );

        // Hosts saved before auth methods existed load as key hosts and
        // serialize without the field.
        let legacy: ManagedHost =
            serde_json::from_str(r#"{"alias":"web","hostName":"web.example"}"#).unwrap();
        assert_eq!(legacy.auth_method, SshAuthMethod::Key);
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("authMethod")
            .is_none());
    }

    #[test]
    fn renders_and_validates_connection_defaults() {
        let mut settings = ConnectionSettings::default();
        assert!(!render_connection_defaults(&settings).contains("Host"));
        settings.server_alive_enabled = true;
        assert!(render_connection_defaults(&settings)
            .ends_with("\nHost *\n  ServerAliveInterval 15\n  ServerAliveCountMax 3\n"));
        assert!(settings.validate().is_ok());
        for (interval, count) in [(4, 3), (601, 3), (15, 0), (15, 11)] {
            let invalid = ConnectionSettings {
                server_alive_interval: interval,
                server_alive_count_max: count,
                ..settings.clone()
            };
            assert!(
                matches!(invalid.validate(), Err(AppError::InvalidRequest(_))),
                "{interval}/{count}"
            );
        }
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
