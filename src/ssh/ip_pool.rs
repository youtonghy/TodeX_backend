//! Loopback IP pool: on-demand forwarding of a whole host to one address.
//!
//! Each assignment gives a host its own loopback address (for example
//! `127.77.0.2`). While the pool is enabled the daemon listens on that
//! address for every port of the assignment's set; a connection to `ip:P` is
//! carried to `127.0.0.1:P` on the host through a dedicated ssh master (one
//! per assignment, started by the first connection and closed after the
//! idle time) with one `ssh -W` mux client per connection. Unix only: other
//! systems report `supported: false`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsString,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{oneshot, Notify},
    task::{AbortHandle, JoinHandle},
    time::Instant,
};

use super::{
    classify_failure,
    tunnel::{self, GroupChild, StderrTail},
    Launcher, SshFailureKind, SshMode,
};
use crate::{error::AppError, external_command, remote_fs::askpass::AskpassSecret};

/// Whether this platform runs the pool.
pub(crate) const SUPPORTED: bool = cfg!(unix);
pub(crate) const UNSUPPORTED_REASON: &str =
    "the loopback IP pool needs a Unix system (Linux or macOS)";
pub(crate) const DEFAULT_CIDR: &str = "127.77.0.0/24";
pub(crate) const DEFAULT_IDLE_MINUTES: u32 = 15;
pub(crate) const MANUAL_PORT_LIMIT: usize = 4096;
/// Listeners across all assignments.
pub(crate) const MAX_TOTAL_LISTENERS: usize = 200_000;
/// Skipped ports listed by number; the count covers the rest.
const MAX_LISTED_SKIPPED: usize = 100;
/// The built-in port list of auto mode.
pub(crate) const AUTO_PORT_SPECS: &[&str] = &[
    "80",
    "443",
    "1433",
    "3000-3010",
    "3306",
    "4000",
    "4200",
    "5000-5010",
    "5173",
    "5432",
    "5601",
    "6379",
    "8000-8100",
    "8443",
    "8888",
    "9000-9100",
    "9200",
    "11434",
    "15672",
    "27017",
];
/// How long a master may take to sign in.
const MASTER_READY_TIMEOUT: Duration = Duration::from_secs(25);
/// After an automatic start failed, connections are closed this long
/// before another automatic start is tried.
const AUTO_RETRY_AFTER: Duration = Duration::from_secs(15);
/// Longest control socket path (sun_path is 104 bytes on macOS).
const MAX_CONTROL_PATH_LEN: usize = 100;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::InvalidRequest(message.into())
}

// ---------------------------------------------------------------- config

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PortMode {
    #[default]
    Auto,
    Manual,
}

/// Body of `PUT /v2/ssh/ip-pool`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PoolConfigInput {
    pub enabled: bool,
    pub cidr: String,
    pub idle_minutes: i64,
}

/// Body of `POST /v2/ssh/ip-pool/assignments`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AssignmentInput {
    #[serde(default)]
    pub ip: Option<String>,
    pub alias: String,
    pub port_mode: PortMode,
    #[serde(default)]
    pub ports: Option<Vec<String>>,
}

/// Body of `PUT /v2/ssh/ip-pool/assignments/{ip}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AssignmentUpdate {
    pub port_mode: PortMode,
    #[serde(default)]
    pub ports: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredAssignment {
    pub ip: Ipv4Addr,
    pub alias: String,
    #[serde(default)]
    pub port_mode: PortMode,
    #[serde(default)]
    pub ports: Vec<String>,
}

/// `ipPool` in `store.json`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredIpPool {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cidr")]
    pub cidr: String,
    #[serde(default = "default_idle")]
    pub idle_minutes: u32,
    #[serde(default)]
    pub assignments: Vec<StoredAssignment>,
}

fn default_cidr() -> String {
    DEFAULT_CIDR.to_owned()
}

fn default_idle() -> u32 {
    DEFAULT_IDLE_MINUTES
}

impl Default for StoredIpPool {
    fn default() -> Self {
        Self {
            enabled: false,
            cidr: default_cidr(),
            idle_minutes: DEFAULT_IDLE_MINUTES,
            assignments: Vec::new(),
        }
    }
}

impl StoredIpPool {
    pub(crate) fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// The pool as any daemon version stored it: invalid parts fall back to
    /// defaults, invalid or clashing assignments are dropped with a warning.
    pub(crate) fn from_stored(value: serde_json::Value) -> Self {
        let mut pool: StoredIpPool = match serde_json::from_value(value) {
            Ok(pool) => pool,
            Err(error) => {
                tracing::warn!(%error, "ignoring unreadable stored ssh ip pool");
                return Self::default();
            }
        };
        let cidr = match Cidr::parse(&pool.cidr) {
            Ok(cidr) => cidr,
            Err(error) => {
                tracing::warn!(cidr = pool.cidr, error, "invalid stored ip pool cidr");
                pool.cidr = default_cidr();
                Cidr::parse(DEFAULT_CIDR).expect("default cidr")
            }
        };
        pool.cidr = cidr.to_string();
        pool.idle_minutes = pool.idle_minutes.clamp(1, 1440);
        let mut ips = HashSet::new();
        let mut aliases = HashSet::new();
        let mut total = 0usize;
        pool.assignments.retain_mut(|assignment| {
            let ports = match assignment.port_mode {
                PortMode::Auto => {
                    assignment.ports.clear();
                    Ok(auto_ports().len())
                }
                PortMode::Manual => parse_ports(&assignment.ports).map(|(normalized, set)| {
                    assignment.ports = normalized;
                    set.len()
                }),
            };
            let ok = match ports {
                Ok(count)
                    if cidr.assignable(assignment.ip)
                        && super::config_file::valid_alias(&assignment.alias)
                        && !ips.contains(&assignment.ip)
                        && !aliases.contains(&assignment.alias)
                        && total + count <= MAX_TOTAL_LISTENERS =>
                {
                    total += count;
                    true
                }
                _ => false,
            };
            if ok {
                ips.insert(assignment.ip);
                aliases.insert(assignment.alias.clone());
            } else {
                tracing::warn!(ip = %assignment.ip, alias = assignment.alias,
                    "ignoring invalid stored ip pool assignment");
            }
            ok
        });
        pool
    }

    pub(crate) fn assignment(&self, ip: Ipv4Addr) -> Result<&StoredAssignment, AppError> {
        self.assignments
            .iter()
            .find(|assignment| assignment.ip == ip)
            .ok_or_else(|| AppError::NotFound(format!("ip pool assignment {ip}")))
    }

    fn listener_count(&self, skip: Option<Ipv4Addr>) -> usize {
        self.assignments
            .iter()
            .filter(|assignment| Some(assignment.ip) != skip)
            .map(|assignment| port_set(assignment).len())
            .sum()
    }
}

/// Checks `PUT /v2/ssh/ip-pool` input against the current pool.
pub(crate) fn apply_config(
    current: &StoredIpPool,
    input: PoolConfigInput,
) -> Result<StoredIpPool, AppError> {
    let cidr = Cidr::parse(input.cidr.trim()).map_err(invalid)?;
    let idle_minutes = u32::try_from(input.idle_minutes)
        .ok()
        .filter(|minutes| (1..=1440).contains(minutes))
        .ok_or_else(|| invalid("idleMinutes must be 1-1440"))?;
    if input.enabled && !SUPPORTED {
        return Err(AppError::Unsupported(UNSUPPORTED_REASON.to_owned()));
    }
    if let Some(outside) = current
        .assignments
        .iter()
        .find(|assignment| !cidr.assignable(assignment.ip))
    {
        return Err(AppError::Conflict(format!(
            "{} (assigned to {}) is not an assignable address of {cidr}; remove the \
             assignment first",
            outside.ip, outside.alias
        )));
    }
    Ok(StoredIpPool {
        enabled: input.enabled,
        cidr: cidr.to_string(),
        idle_minutes,
        assignments: current.assignments.clone(),
    })
}

fn checked_ports(mode: PortMode, ports: Option<Vec<String>>) -> Result<Vec<String>, AppError> {
    match mode {
        // Auto mode uses the built-in list; a manual list sent along (a
        // client switching modes) is not kept.
        PortMode::Auto => Ok(Vec::new()),
        PortMode::Manual => {
            let ports = ports.unwrap_or_default();
            if ports.is_empty() {
                return Err(invalid("manual port mode needs at least one port"));
            }
            parse_ports(&ports)
                .map(|(normalized, _)| normalized)
                .map_err(invalid)
        }
    }
}

/// A new assignment for `pool` (validated; `alias` must exist already).
pub(crate) fn new_assignment(
    pool: &StoredIpPool,
    input: AssignmentInput,
) -> Result<StoredAssignment, AppError> {
    let cidr = Cidr::parse(&pool.cidr).map_err(invalid)?;
    let alias = input.alias.trim().to_owned();
    if !super::config_file::valid_alias(&alias) {
        return Err(invalid(format!("invalid ssh host: {alias}")));
    }
    let ports = checked_ports(input.port_mode, input.ports)?;
    if let Some(taken) = pool.assignments.iter().find(|a| a.alias == alias) {
        return Err(AppError::Conflict(format!(
            "{alias} already has the address {}",
            taken.ip
        )));
    }
    let used: HashSet<Ipv4Addr> = pool.assignments.iter().map(|a| a.ip).collect();
    let ip = match input
        .ip
        .as_deref()
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
    {
        Some(raw) => {
            let ip: Ipv4Addr = raw
                .parse()
                .map_err(|_| invalid(format!("{raw} is not an IPv4 address")))?;
            if !cidr.assignable(ip) {
                return Err(invalid(format!(
                    "{ip} is not an assignable address of {cidr}"
                )));
            }
            if used.contains(&ip) {
                return Err(AppError::Conflict(format!("{ip} is already assigned")));
            }
            ip
        }
        None => cidr
            .hosts()
            .find(|ip| !used.contains(ip))
            .ok_or_else(|| AppError::Conflict(format!("no free address left in {cidr}")))?,
    };
    let assignment = StoredAssignment {
        ip,
        alias,
        port_mode: input.port_mode,
        ports,
    };
    check_listener_cap(pool, &assignment, None)?;
    Ok(assignment)
}

/// `assignment` with a new port mode or list.
pub(crate) fn update_assignment(
    pool: &StoredIpPool,
    ip: Ipv4Addr,
    input: AssignmentUpdate,
) -> Result<StoredAssignment, AppError> {
    let current = pool.assignment(ip)?;
    let updated = StoredAssignment {
        port_mode: input.port_mode,
        ports: checked_ports(input.port_mode, input.ports)?,
        ..current.clone()
    };
    check_listener_cap(pool, &updated, Some(ip))?;
    Ok(updated)
}

fn check_listener_cap(
    pool: &StoredIpPool,
    assignment: &StoredAssignment,
    replacing: Option<Ipv4Addr>,
) -> Result<(), AppError> {
    if pool.listener_count(replacing) + port_set(assignment).len() > MAX_TOTAL_LISTENERS {
        return Err(AppError::Conflict(format!(
            "the IP pool is limited to {MAX_TOTAL_LISTENERS} listeners in total"
        )));
    }
    Ok(())
}

/// An IPv4 network inside `127.0.0.0/8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cidr {
    network: u32,
    prefix: u8,
}

impl Cidr {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let (address, prefix) = text
            .split_once('/')
            .ok_or_else(|| format!("{text} is not a CIDR range like {DEFAULT_CIDR}"))?;
        let address: Ipv4Addr = address
            .parse()
            .map_err(|_| format!("{text} is not a CIDR range like {DEFAULT_CIDR}"))?;
        let prefix: u8 = prefix
            .parse()
            .ok()
            .filter(|prefix| (16..=30).contains(prefix))
            .ok_or_else(|| "the cidr prefix length must be 16-30".to_owned())?;
        let address = u32::from(address);
        let mask = u32::MAX << (32 - prefix);
        let cidr = Self {
            network: address & mask,
            prefix,
        };
        if address != cidr.network {
            return Err(format!("{text} has host bits set; did you mean {}?", cidr));
        }
        if address >> 24 != 127 {
            return Err("the cidr must be inside 127.0.0.0/8 (loopback)".to_owned());
        }
        if cidr.contains(Ipv4Addr::LOCALHOST) {
            return Err("the cidr must not contain 127.0.0.1".to_owned());
        }
        Ok(cidr)
    }

    fn broadcast(&self) -> u32 {
        self.network | (u32::MAX >> self.prefix)
    }

    pub(crate) fn contains(&self, ip: Ipv4Addr) -> bool {
        let ip = u32::from(ip);
        ip >= self.network && ip <= self.broadcast()
    }

    /// Inside the range and neither its network nor broadcast address.
    pub(crate) fn assignable(&self, ip: Ipv4Addr) -> bool {
        let raw = u32::from(ip);
        self.contains(ip) && raw != self.network && raw != self.broadcast()
    }

    pub(crate) fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> {
        (self.network + 1..self.broadcast()).map(Ipv4Addr::from)
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.network), self.prefix)
    }
}

/// Parses `8080` / `7000-7010` entries. Returns the normalized entries
/// (duplicates removed, order kept) and the ports they cover.
pub(crate) fn parse_ports(entries: &[String]) -> Result<(Vec<String>, BTreeSet<u16>), String> {
    if entries.len() > MANUAL_PORT_LIMIT {
        return Err(format!("at most {MANUAL_PORT_LIMIT} port entries"));
    }
    let parse_one = |text: &str| -> Result<u16, String> {
        text.parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("{text:?} is not a port (1-65535)"))
    };
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    let mut ports = BTreeSet::new();
    for entry in entries {
        let entry = entry.trim();
        let (start, end) = match entry.split_once('-') {
            Some((start, end)) => (parse_one(start.trim())?, parse_one(end.trim())?),
            None => {
                let port = parse_one(entry)?;
                (port, port)
            }
        };
        if start > end {
            return Err(format!("{entry:?}: the range start is above its end"));
        }
        let text = if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        };
        if !seen.insert(text.clone()) {
            continue;
        }
        normalized.push(text);
        ports.extend(start..=end);
        if ports.len() > MANUAL_PORT_LIMIT {
            return Err(format!("at most {MANUAL_PORT_LIMIT} ports per address"));
        }
    }
    Ok((normalized, ports))
}

pub(crate) fn auto_ports() -> BTreeSet<u16> {
    static PORTS: std::sync::OnceLock<BTreeSet<u16>> = std::sync::OnceLock::new();
    PORTS
        .get_or_init(|| {
            let specs: Vec<String> = AUTO_PORT_SPECS.iter().map(|s| (*s).to_owned()).collect();
            parse_ports(&specs).expect("valid auto ports").1
        })
        .clone()
}

fn port_set(assignment: &StoredAssignment) -> BTreeSet<u16> {
    match assignment.port_mode {
        PortMode::Auto => auto_ports(),
        PortMode::Manual => parse_ports(&assignment.ports)
            .map(|(_, ports)| ports)
            .unwrap_or_default(),
    }
}

// ----------------------------------------------------------------- views

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AssignmentState {
    #[default]
    Disabled,
    Idle,
    Connecting,
    Connected,
    NeedsLogin,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BindErrorKind {
    AddressUnavailable,
    Other,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BindError {
    pub kind: BindErrorKind,
    pub message: String,
}

/// `SshIpAssignment` of the API.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssignmentView {
    pub ip: String,
    pub alias: String,
    pub port_mode: PortMode,
    pub ports: Vec<String>,
    pub listening_ports: usize,
    pub skipped_ports: Vec<u16>,
    pub skipped_port_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_error: Option<BindError>,
    pub state: AssignmentState,
    pub active_connections: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<SshFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `SshIpPool` of the API.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PoolView {
    pub enabled: bool,
    pub cidr: String,
    pub idle_minutes: u32,
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
    pub auto_ports: Vec<String>,
    pub manual_port_limit: usize,
    pub assignments: Vec<AssignmentView>,
}

// --------------------------------------------------------------- runtime

/// What the runtime needs to know about a host besides its alias.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostTraits {
    /// Password hosts never start a master on their own.
    pub password: bool,
}

#[derive(Clone)]
pub(crate) struct PoolRuntime {
    shared: Arc<Shared>,
}

struct Shared {
    launcher: Launcher,
    control_dir: PathBuf,
    idle: Mutex<Duration>,
    /// Tests use seconds instead of minutes.
    idle_override: Mutex<Option<Duration>>,
    assignments: Mutex<HashMap<Ipv4Addr, Arc<Assignment>>>,
}

impl Shared {
    fn idle(&self) -> Duration {
        self.idle_override
            .lock()
            .unwrap()
            .unwrap_or(*self.idle.lock().unwrap())
    }
}

struct Info {
    alias: String,
    password_host: bool,
    enabled: bool,
    ports: BTreeSet<u16>,
    listeners: BTreeMap<u16, AbortHandle>,
    skipped: BTreeSet<u16>,
    bind_error: Option<BindError>,
    state: AssignmentState,
    failure: Option<SshFailureKind>,
    error: Option<String>,
    connected_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    last_activity: Instant,
    last_auto_failure: Option<Instant>,
}

struct Master {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

struct Assignment {
    ip: Ipv4Addr,
    control_path: PathBuf,
    shared: Weak<Shared>,
    info: Mutex<Info>,
    active: AtomicUsize,
    activity: Notify,
    master: tokio::sync::Mutex<Option<Master>>,
}

/// Credentials for an explicit connect with a password.
pub(crate) type AskpassEnv = ([(&'static str, OsString); 2], AskpassSecret);

impl PoolRuntime {
    /// Prepares the control socket directory (removing sockets a previous
    /// daemon left) and raises the open-file limit once.
    pub(crate) fn new(launcher: Launcher, ssh_dir: &Path) -> Self {
        let control_dir = control_dir(ssh_dir);
        if SUPPORTED {
            if let Err(error) = prepare_control_dir(&control_dir) {
                tracing::warn!(%error, dir = %control_dir.display(), "cannot prepare ip pool control directory");
            }
            raise_open_file_limit();
        }
        Self {
            shared: Arc::new(Shared {
                launcher,
                control_dir,
                idle: Mutex::new(Duration::from_secs(u64::from(DEFAULT_IDLE_MINUTES) * 60)),
                idle_override: Mutex::new(None),
                assignments: Mutex::new(HashMap::new()),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_idle_override(&self, idle: Duration) {
        *self.shared.idle_override.lock().unwrap() = Some(idle);
    }

    /// Brings listeners and masters in line with `pool`: listeners are
    /// added and removed incrementally, masters of removed or re-pointed
    /// assignments (or of a disabled pool) are closed.
    pub(crate) async fn reconcile(&self, pool: &StoredIpPool, hosts: &HashMap<String, HostTraits>) {
        let enabled = pool.enabled && SUPPORTED;
        *self.shared.idle.lock().unwrap() = Duration::from_secs(u64::from(pool.idle_minutes) * 60);
        let (removed, current) = {
            let mut map = self.shared.assignments.lock().unwrap();
            let wanted: HashSet<Ipv4Addr> = pool.assignments.iter().map(|a| a.ip).collect();
            let removed: Vec<Arc<Assignment>> = map
                .keys()
                .filter(|ip| !wanted.contains(ip))
                .copied()
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|ip| map.remove(&ip))
                .collect();
            let current: Vec<(Arc<Assignment>, &StoredAssignment)> = pool
                .assignments
                .iter()
                .map(|stored| {
                    let assignment = map
                        .entry(stored.ip)
                        .or_insert_with(|| self.new_assignment(stored))
                        .clone();
                    (assignment, stored)
                })
                .collect();
            (removed, current)
        };
        for assignment in removed {
            assignment.shutdown().await;
        }
        for (assignment, stored) in current {
            let traits = hosts.get(&stored.alias).copied().unwrap_or_default();
            assignment
                .apply(&stored.alias, traits, enabled, port_set(stored))
                .await;
        }
    }

    fn new_assignment(&self, stored: &StoredAssignment) -> Arc<Assignment> {
        Arc::new(Assignment {
            ip: stored.ip,
            control_path: self.shared.control_dir.join(stored.ip.to_string()),
            shared: Arc::downgrade(&self.shared),
            info: Mutex::new(Info {
                alias: stored.alias.clone(),
                password_host: false,
                enabled: false,
                ports: BTreeSet::new(),
                listeners: BTreeMap::new(),
                skipped: BTreeSet::new(),
                bind_error: None,
                state: AssignmentState::Idle,
                failure: None,
                error: None,
                connected_at: None,
                last_used_at: None,
                last_activity: Instant::now(),
                last_auto_failure: None,
            }),
            active: AtomicUsize::new(0),
            activity: Notify::new(),
            master: tokio::sync::Mutex::new(None),
        })
    }

    fn get(&self, ip: Ipv4Addr) -> Option<Arc<Assignment>> {
        self.shared.assignments.lock().unwrap().get(&ip).cloned()
    }

    pub(crate) fn view(&self, pool: &StoredIpPool) -> PoolView {
        PoolView {
            enabled: pool.enabled,
            cidr: pool.cidr.clone(),
            idle_minutes: pool.idle_minutes,
            supported: SUPPORTED,
            unsupported_reason: (!SUPPORTED).then(|| UNSUPPORTED_REASON.to_owned()),
            auto_ports: AUTO_PORT_SPECS.iter().map(|s| (*s).to_owned()).collect(),
            manual_port_limit: MANUAL_PORT_LIMIT,
            assignments: pool
                .assignments
                .iter()
                .map(|stored| self.assignment_view(pool, stored))
                .collect(),
        }
    }

    pub(crate) fn assignment_view(
        &self,
        pool: &StoredIpPool,
        stored: &StoredAssignment,
    ) -> AssignmentView {
        let mut view = AssignmentView {
            ip: stored.ip.to_string(),
            alias: stored.alias.clone(),
            port_mode: stored.port_mode,
            ports: stored.ports.clone(),
            listening_ports: 0,
            skipped_ports: Vec::new(),
            skipped_port_count: 0,
            bind_error: None,
            state: AssignmentState::Disabled,
            active_connections: 0,
            connected_at: None,
            last_used_at: None,
            failure: None,
            error: None,
        };
        let Some(assignment) = self.get(stored.ip) else {
            return view;
        };
        let info = assignment.info.lock().unwrap();
        view.listening_ports = info.listeners.len();
        view.skipped_ports = info
            .skipped
            .iter()
            .take(MAX_LISTED_SKIPPED)
            .copied()
            .collect();
        view.skipped_port_count = info.skipped.len();
        view.bind_error = info.bind_error.clone();
        view.state = if pool.enabled && SUPPORTED && info.enabled {
            info.state
        } else {
            AssignmentState::Disabled
        };
        view.active_connections = assignment.active.load(Ordering::SeqCst);
        view.connected_at = info.connected_at;
        view.last_used_at = info.last_used_at;
        view.failure = info.failure;
        view.error = info.error.clone();
        view
    }

    /// Starts the master now (explicit connect), with a password if given.
    pub(crate) async fn connect(&self, ip: Ipv4Addr, askpass: Option<AskpassEnv>) {
        if let Some(assignment) = self.get(ip) {
            let _ = assignment.ensure_master(askpass, true).await;
        }
    }

    pub(crate) async fn disconnect(&self, ip: Ipv4Addr) {
        if let Some(assignment) = self.get(ip) {
            assignment.close_master().await;
            let mut info = assignment.info.lock().unwrap();
            info.state = AssignmentState::Idle;
            info.failure = None;
            info.error = None;
        }
    }

    /// Stops listeners and masters of `ip` ahead of its removal.
    pub(crate) async fn remove(&self, ip: Ipv4Addr) {
        let removed = self.shared.assignments.lock().unwrap().remove(&ip);
        if let Some(assignment) = removed {
            assignment.shutdown().await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        let all: Vec<Arc<Assignment>> = self
            .shared
            .assignments
            .lock()
            .unwrap()
            .drain()
            .map(|(_, assignment)| assignment)
            .collect();
        futures_util::future::join_all(all.iter().map(|a| a.shutdown())).await;
    }
}

impl Assignment {
    fn set_state(
        &self,
        state: AssignmentState,
        failure: Option<SshFailureKind>,
        error: Option<String>,
    ) {
        let mut info = self.info.lock().unwrap();
        info.state = state;
        info.failure = failure;
        info.error = error;
        if state != AssignmentState::Connected {
            info.connected_at = None;
        }
    }

    async fn apply(
        self: &Arc<Self>,
        alias: &str,
        traits: HostTraits,
        enabled: bool,
        ports: BTreeSet<u16>,
    ) {
        let repointed = {
            let mut info = self.info.lock().unwrap();
            let repointed = info.alias != alias || info.password_host != traits.password;
            info.alias = alias.to_owned();
            info.password_host = traits.password;
            info.enabled = enabled;
            info.ports = ports;
            repointed
        };
        if repointed || !enabled {
            self.close_master().await;
            if repointed {
                self.set_state(AssignmentState::Idle, None, None);
            }
        }
        if enabled {
            self.sync_listeners();
        } else {
            self.stop_listeners();
        }
    }

    fn stop_listeners(&self) {
        let mut info = self.info.lock().unwrap();
        for (_, handle) in std::mem::take(&mut info.listeners) {
            handle.abort();
        }
        info.skipped.clear();
        info.bind_error = None;
    }

    /// Binds missing ports (retrying skipped ones) and drops unwanted ones;
    /// existing connections are not affected.
    fn sync_listeners(self: &Arc<Self>) {
        let mut info = self.info.lock().unwrap();
        let unwanted: Vec<u16> = info
            .listeners
            .keys()
            .filter(|port| !info.ports.contains(port))
            .copied()
            .collect();
        for port in unwanted {
            if let Some(handle) = info.listeners.remove(&port) {
                handle.abort();
            }
        }
        info.skipped.clear();
        info.bind_error = None;
        let missing: Vec<u16> = info
            .ports
            .iter()
            .filter(|port| !info.listeners.contains_key(port))
            .copied()
            .collect();
        for port in missing {
            match bind(SocketAddr::new(self.ip.into(), port)) {
                Ok(listener) => {
                    let assignment = self.clone();
                    let task =
                        tokio::spawn(async move { assignment.accept_loop(listener, port).await });
                    info.listeners.insert(port, task.abort_handle());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    info.skipped.insert(port);
                }
                Err(error) => {
                    let kind = if error.kind() == std::io::ErrorKind::AddrNotAvailable {
                        BindErrorKind::AddressUnavailable
                    } else {
                        BindErrorKind::Other
                    };
                    let hint =
                        if kind == BindErrorKind::AddressUnavailable && cfg!(target_os = "macos") {
                            format!(
                                "; run `sudo ifconfig lo0 alias {}` to add the address",
                                self.ip
                            )
                        } else {
                            String::new()
                        };
                    info.bind_error = Some(BindError {
                        kind,
                        message: format!("cannot listen on {}:{port}: {error}{hint}", self.ip),
                    });
                    break;
                }
            }
        }
    }

    async fn accept_loop(self: Arc<Self>, listener: tokio::net::TcpListener, port: u16) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let assignment = self.clone();
                    tokio::spawn(async move { assignment.handle(stream, port).await });
                }
                // Out of descriptors or a connection reset before accept.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    async fn handle(self: Arc<Self>, stream: tokio::net::TcpStream, port: u16) {
        // Counted before the master is checked, so the idle timer cannot
        // close a master this connection is about to use.
        let _active = ActiveGuard::new(&self);
        if self.ensure_master(None, false).await.is_err() {
            return;
        }
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let alias = self.info.lock().unwrap().alias.clone();
        let mut command = shared
            .launcher
            .command(SshMode::Batch, mux_client_options(&self.control_path));
        command
            .arg("-W")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--")
            .arg(&alias)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = GroupChild::spawn(&mut command) else {
            return;
        };
        let (Some(stdout), Some(stdin)) = (
            child.child_mut().stdout.take(),
            child.child_mut().stdin.take(),
        ) else {
            child.terminate().await;
            return;
        };
        pipe(stream, stdout, stdin).await;
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            child.terminate().await;
        }
    }

    /// Makes sure a master runs. Concurrent callers wait for one start.
    async fn ensure_master(
        self: &Arc<Self>,
        askpass: Option<AskpassEnv>,
        explicit: bool,
    ) -> Result<(), ()> {
        let mut slot = self.master.lock().await;
        if slot
            .as_ref()
            .is_some_and(|master| !master.task.is_finished())
        {
            return Ok(());
        }
        *slot = None;
        let Some(shared) = self.shared.upgrade() else {
            return Err(());
        };
        let (alias, enabled) = {
            let info = self.info.lock().unwrap();
            if !explicit {
                if info.password_host {
                    drop(info);
                    let mut info = self.info.lock().unwrap();
                    if info.state != AssignmentState::NeedsLogin {
                        info.state = AssignmentState::NeedsLogin;
                        info.failure = None;
                        info.error =
                            Some("sign in with the host's password (connect) first".to_owned());
                    }
                    return Err(());
                }
                if info
                    .last_auto_failure
                    .is_some_and(|at| at.elapsed() < AUTO_RETRY_AFTER)
                {
                    return Err(());
                }
            }
            (info.alias.clone(), info.enabled)
        };
        if !enabled {
            return Err(());
        }
        self.set_state(AssignmentState::Connecting, None, None);
        let _ = std::fs::remove_file(&self.control_path);
        let mode = if askpass.is_some() {
            SshMode::Askpass
        } else {
            SshMode::Batch
        };
        let mut command = shared
            .launcher
            .command(mode, master_options(&self.control_path));
        command
            .args(["-N", "-o", "ClearAllForwardings=yes", "--"])
            .arg(&alias);
        let secret = askpass.map(|(vars, secret)| {
            command.envs(vars);
            secret
        });
        tunnel::piped_stderr(&mut command);
        let mut child = match GroupChild::spawn(&mut command) {
            Ok(child) => child,
            Err(error) => {
                self.fail_start(
                    SshFailureKind::Other,
                    format!("cannot start ssh: {error}"),
                    explicit,
                );
                return Err(());
            }
        };
        let tail = StderrTail::default();
        if let Some(stderr) = child.child_mut().stderr.take() {
            tunnel::drain_stderr(stderr, tail.clone(), None);
        }
        let control_path = self.control_path.clone();
        let ready = tokio::time::timeout(MASTER_READY_TIMEOUT, async {
            loop {
                tokio::select! {
                    _ = child.wait() => return false,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if control_path.exists() {
                            return true;
                        }
                    }
                }
            }
        })
        .await;
        drop(secret);
        match ready {
            Ok(true) => {
                {
                    let mut info = self.info.lock().unwrap();
                    info.state = AssignmentState::Connected;
                    info.failure = None;
                    info.error = None;
                    info.connected_at = Some(Utc::now());
                    info.last_activity = Instant::now();
                    info.last_auto_failure = None;
                }
                let (stop_tx, stop_rx) = oneshot::channel();
                let assignment = self.clone();
                let task =
                    tokio::spawn(
                        async move { assignment.supervise(child, stop_rx, tail, alias).await },
                    );
                *slot = Some(Master {
                    stop: stop_tx,
                    task,
                });
                Ok(())
            }
            Ok(false) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let failure = classify_failure(&tail.text());
                let summary = tail.summary();
                let error = if summary.is_empty() {
                    "ssh exited before signing in".to_owned()
                } else {
                    summary
                };
                self.fail_start(failure, error, explicit);
                Err(())
            }
            Err(_) => {
                child.terminate().await;
                self.fail_start(
                    SshFailureKind::TimedOut,
                    "ssh did not sign in in time".to_owned(),
                    explicit,
                );
                Err(())
            }
        }
    }

    fn fail_start(&self, failure: SshFailureKind, error: String, explicit: bool) {
        let state = if failure == SshFailureKind::AuthenticationFailed {
            AssignmentState::NeedsLogin
        } else {
            AssignmentState::Failed
        };
        self.set_state(state, Some(failure), Some(error));
        if !explicit {
            self.info.lock().unwrap().last_auto_failure = Some(Instant::now());
        }
    }

    /// Watches a running master: closes it after the idle time without
    /// connections, on request, or notes that it went away.
    async fn supervise(
        self: Arc<Self>,
        mut child: GroupChild,
        mut stop: oneshot::Receiver<()>,
        tail: StderrTail,
        alias: String,
    ) {
        loop {
            let idle = self.shared.upgrade().map(|s| s.idle()).unwrap_or_default();
            let deadline = (self.active.load(Ordering::SeqCst) == 0)
                .then(|| self.info.lock().unwrap().last_activity + idle);
            tokio::select! {
                _ = child.wait() => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let summary = tail.summary();
                    self.set_state(
                        AssignmentState::Idle,
                        None,
                        (!summary.is_empty()).then(|| format!("connection closed: {summary}")),
                    );
                    break;
                }
                _ = &mut stop => {
                    self.close_child(&mut child, &alias).await;
                    break;
                }
                _ = sleep_until_opt(deadline) => {
                    let expired = self.active.load(Ordering::SeqCst) == 0
                        && self.info.lock().unwrap().last_activity + idle <= Instant::now();
                    if expired {
                        self.close_child(&mut child, &alias).await;
                        self.set_state(AssignmentState::Idle, None, None);
                        break;
                    }
                }
                _ = self.activity.notified() => {}
            }
        }
        let _ = std::fs::remove_file(&self.control_path);
    }

    /// `ssh -O exit`, then the process group if the master lingers.
    async fn close_child(&self, child: &mut GroupChild, alias: &str) {
        if let Some(shared) = self.shared.upgrade() {
            let mut command = external_command::secure_command(&shared.launcher.ssh_bin);
            command
                .arg("-F")
                .arg(&shared.launcher.wrapper)
                .arg("-o")
                .arg(format!("ControlPath={}", self.control_path.display()))
                .args(["-O", "exit", "--", alias]);
            external_command::prepare_captured(&mut command, false);
            let _ = external_command::run(
                command,
                None,
                external_command::CommandLimits {
                    timeout: Duration::from_secs(5),
                    output_limit: 16 * 1024,
                },
            )
            .await;
        }
        if tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            child.terminate().await;
        }
    }

    async fn close_master(&self) {
        let master = self.master.lock().await.take();
        if let Some(master) = master {
            let _ = master.stop.send(());
            if tokio::time::timeout(Duration::from_secs(10), master.task)
                .await
                .is_err()
            {
                tracing::warn!(ip = %self.ip, "ip pool master did not close in time");
            }
            let mut info = self.info.lock().unwrap();
            if info.state == AssignmentState::Connected {
                info.state = AssignmentState::Idle;
            }
            info.connected_at = None;
        }
    }

    async fn shutdown(&self) {
        self.stop_listeners();
        self.close_master().await;
    }
}

/// Copies both directions with TCP half-close semantics: the client's EOF
/// closes ssh's stdin (so the remote side sees it), ssh's EOF shuts down
/// the client's write side. Ends when both directions ended.
async fn pipe(
    stream: tokio::net::TcpStream,
    mut stdout: tokio::process::ChildStdout,
    mut stdin: tokio::process::ChildStdin,
) {
    use tokio::io::AsyncWriteExt;
    let (mut client_read, mut client_write) = stream.into_split();
    let upstream = async move {
        let _ = tokio::io::copy(&mut client_read, &mut stdin).await;
        drop(stdin);
    };
    let downstream = async move {
        let _ = tokio::io::copy(&mut stdout, &mut client_write).await;
        let _ = client_write.shutdown().await;
    };
    tokio::join!(upstream, downstream);
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Counts a forwarded connection while it lives.
struct ActiveGuard {
    assignment: Arc<Assignment>,
}

impl ActiveGuard {
    fn new(assignment: &Arc<Assignment>) -> Self {
        assignment.active.fetch_add(1, Ordering::SeqCst);
        assignment.touch();
        Self {
            assignment: assignment.clone(),
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.assignment.active.fetch_sub(1, Ordering::SeqCst);
        self.assignment.touch();
    }
}

impl Assignment {
    fn touch(&self) {
        {
            let mut info = self.info.lock().unwrap();
            info.last_activity = Instant::now();
            info.last_used_at = Some(Utc::now());
        }
        self.activity.notify_one();
    }
}

/// The master: owns the control socket, stays in the foreground.
fn master_options(control_path: &Path) -> Vec<OsString> {
    vec![
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        "ControlPersist=no".into(),
        "-o".into(),
        format!("ControlPath={}", control_path.display()).into(),
    ]
}

/// A mux client of the master. Without the socket ssh would fall back to a
/// connection of its own; the failing proxy command turns that into an
/// immediate error instead of a second sign-in.
fn mux_client_options(control_path: &Path) -> Vec<OsString> {
    vec![
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        format!("ControlPath={}", control_path.display()).into(),
        "-o".into(),
        "ProxyCommand=false".into(),
    ]
}

fn bind(address: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(address)?;
    socket.listen(128)
}

/// `$DATA_DIR/ssh/pool`, or a per-data-dir directory under the temp dir
/// when that path is too long for unix sockets.
fn control_dir(ssh_dir: &Path) -> PathBuf {
    let dir = ssh_dir.join("pool");
    // `/255.255.255.255` is the longest socket name.
    if dir.as_os_str().len() + 16 <= MAX_CONTROL_PATH_LEN {
        return dir;
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(ssh_dir.as_os_str().as_encoded_bytes());
    let short: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    #[cfg(unix)]
    let user = unsafe { libc::getuid() }.to_string();
    #[cfg(not(unix))]
    let user = "user".to_owned();
    PathBuf::from("/tmp").join(format!("todex-pool-{user}-{short}"))
}

fn prepare_control_dir(dir: &Path) -> std::io::Result<()> {
    crate::secure_fs::ensure_owner_only_dir(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let _ = std::fs::remove_file(entry?.path());
    }
    Ok(())
}

/// Raises the soft open-file limit to the hard limit, once per process:
/// every listener and forwarded connection holds descriptors.
fn raise_open_file_limit() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: plain getrlimit/setrlimit calls on a local struct.
            unsafe {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0
                    || limit.rlim_cur >= limit.rlim_max
                {
                    return;
                }
                let current = limit.rlim_cur;
                for target in [limit.rlim_max.min(1 << 20), 10240] {
                    if target <= current {
                        continue;
                    }
                    let raised = libc::rlimit {
                        rlim_cur: target,
                        rlim_max: limit.rlim_max,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                        tracing::debug!(from = current, to = target, "raised the open file limit");
                        return;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_rules() {
        let cidr = Cidr::parse("127.77.0.0/24").unwrap();
        assert_eq!(cidr.to_string(), "127.77.0.0/24");
        assert!(cidr.contains("127.77.0.0".parse().unwrap()));
        assert!(!cidr.assignable("127.77.0.0".parse().unwrap()));
        assert!(!cidr.assignable("127.77.0.255".parse().unwrap()));
        assert!(cidr.assignable("127.77.0.1".parse().unwrap()));
        assert!(cidr.assignable("127.77.0.254".parse().unwrap()));
        assert!(!cidr.contains("127.77.1.1".parse().unwrap()));
        assert_eq!(cidr.hosts().count(), 254);
        assert_eq!(cidr.hosts().next(), Some("127.77.0.1".parse().unwrap()));
        assert_eq!(Cidr::parse("127.88.0.0/16").unwrap().hosts().count(), 65534);
        assert_eq!(Cidr::parse("127.1.2.4/30").unwrap().hosts().count(), 2);
        for bad in [
            "127.77.0.0",
            "127.77.0.0/15",
            "127.77.0.0/31",
            "10.0.0.0/24",
            "127.0.0.0/24",
            "127.0.0.0/16",
            "127.77.0.5/24",
            "x/24",
            "127.77.0.0/abc",
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
        // 127.0.1.0/24 is fine: it does not contain 127.0.0.1.
        assert!(Cidr::parse("127.0.1.0/24").is_ok());
    }

    #[test]
    fn port_specs() {
        let list = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let (normalized, ports) =
            parse_ports(&list(&["8080", " 7000-7002 ", "08080", "8080", "7001"])).unwrap();
        assert_eq!(normalized, ["8080", "7000-7002", "7001"]);
        assert_eq!(
            ports.into_iter().collect::<Vec<_>>(),
            [7000, 7001, 7002, 8080]
        );
        for bad in [
            &["0"][..],
            &["65536"],
            &["10-5"],
            &["a"],
            &["1-"],
            &["-5"],
            &[""],
        ] {
            assert!(parse_ports(&list(bad)).is_err(), "{bad:?}");
        }
        assert!(parse_ports(&list(&["1-4096"])).is_ok());
        assert!(parse_ports(&list(&["1-4097"])).is_err());
        assert!(parse_ports(&list(&["1-4000", "5000-5095"])).is_ok());
        assert!(parse_ports(&list(&["1-4000", "5000-5096"])).is_err());
        let auto = auto_ports();
        assert!(auto.contains(&5173) && auto.contains(&8050) && auto.contains(&27017));
        assert_eq!(auto.len(), 240);
    }

    fn pool_with(assignments: Vec<StoredAssignment>) -> StoredIpPool {
        StoredIpPool {
            assignments,
            ..StoredIpPool::default()
        }
    }

    fn input(alias: &str) -> AssignmentInput {
        AssignmentInput {
            ip: None,
            alias: alias.into(),
            port_mode: PortMode::Auto,
            ports: None,
        }
    }

    #[test]
    fn assignment_rules() {
        let mut pool = pool_with(Vec::new());
        let first = new_assignment(&pool, input("db")).unwrap();
        assert_eq!(first.ip, Ipv4Addr::new(127, 77, 0, 1));
        pool.assignments.push(first);
        let second = new_assignment(&pool, input("web")).unwrap();
        assert_eq!(second.ip, Ipv4Addr::new(127, 77, 0, 2));
        // One address per host, one host per address.
        assert!(matches!(
            new_assignment(&pool, input("db")),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            new_assignment(
                &pool,
                AssignmentInput {
                    ip: Some("127.77.0.1".into()),
                    ..input("x")
                }
            ),
            Err(AppError::Conflict(_))
        ));
        for bad in [
            "127.77.0.0",
            "127.77.0.255",
            "127.78.0.1",
            "127.0.0.1",
            "nope",
        ] {
            assert!(
                matches!(
                    new_assignment(
                        &pool,
                        AssignmentInput {
                            ip: Some(bad.into()),
                            ..input("x")
                        }
                    ),
                    Err(AppError::InvalidRequest(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            new_assignment(
                &pool,
                AssignmentInput {
                    port_mode: PortMode::Manual,
                    ..input("x")
                }
            ),
            Err(AppError::InvalidRequest(_))
        ));
        let manual = new_assignment(
            &pool,
            AssignmentInput {
                ip: Some("127.77.0.9".into()),
                port_mode: PortMode::Manual,
                ports: Some(vec!["22".into(), "22".into(), "3000-3001".into()]),
                ..input("x")
            },
        )
        .unwrap();
        assert_eq!(manual.ports, ["22", "3000-3001"]);
        // Auto mode does not keep a manual list.
        let auto = new_assignment(
            &pool,
            AssignmentInput {
                ports: Some(vec!["1".into()]),
                ..input("y")
            },
        )
        .unwrap();
        assert!(auto.ports.is_empty());
        pool.assignments.push(manual);
        let updated = update_assignment(
            &pool,
            Ipv4Addr::new(127, 77, 0, 9),
            AssignmentUpdate {
                port_mode: PortMode::Auto,
                ports: None,
            },
        )
        .unwrap();
        assert_eq!(updated.port_mode, PortMode::Auto);
        assert!(matches!(
            update_assignment(
                &pool,
                Ipv4Addr::new(127, 77, 0, 50),
                AssignmentUpdate {
                    port_mode: PortMode::Auto,
                    ports: None
                }
            ),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn config_rules() {
        let pool = pool_with(vec![StoredAssignment {
            ip: Ipv4Addr::new(127, 77, 0, 5),
            alias: "db".into(),
            port_mode: PortMode::Auto,
            ports: Vec::new(),
        }]);
        let config = |cidr: &str, idle: i64| PoolConfigInput {
            enabled: false,
            cidr: cidr.into(),
            idle_minutes: idle,
        };
        assert!(apply_config(&pool, config("127.77.0.0/16", 30)).is_ok());
        assert!(matches!(
            apply_config(&pool, config("127.78.0.0/24", 30)),
            Err(AppError::Conflict(_))
        ));
        // .5 is still assignable in 127.77.0.0/29 (.1-.6), not in .8/29.
        assert!(apply_config(&pool, config("127.77.0.0/29", 30)).is_ok());
        assert!(matches!(
            apply_config(&pool, config("127.77.0.8/29", 30)),
            Err(AppError::Conflict(_))
        ));
        for idle in [0, 1441, -3] {
            assert!(matches!(
                apply_config(&pool, config(DEFAULT_CIDR, idle)),
                Err(AppError::InvalidRequest(_))
            ));
        }
        let enabled = apply_config(
            &pool,
            PoolConfigInput {
                enabled: true,
                ..config(DEFAULT_CIDR, 1)
            },
        );
        if SUPPORTED {
            assert!(enabled.unwrap().enabled);
        } else {
            assert!(matches!(enabled, Err(AppError::Unsupported(_))));
        }
    }

    #[test]
    fn listener_cap() {
        // As many auto assignments as fit under the cap; one more does not.
        let cidr = Cidr::parse("127.88.0.0/16").unwrap();
        let per = auto_ports().len();
        let fit = MAX_TOTAL_LISTENERS / per;
        let assignments = cidr
            .hosts()
            .take(fit)
            .enumerate()
            .map(|(n, ip)| StoredAssignment {
                ip,
                alias: format!("h{n}"),
                port_mode: PortMode::Auto,
                ports: Vec::new(),
            })
            .collect();
        let pool = StoredIpPool {
            cidr: cidr.to_string(),
            ..pool_with(assignments)
        };
        assert!(matches!(
            new_assignment(&pool, input("extra")),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    fn stored_pools_load_leniently() {
        let load = |json: serde_json::Value| StoredIpPool::from_stored(json);
        assert_eq!(load(serde_json::json!({})), StoredIpPool::default());
        let pool = load(serde_json::json!({
            "enabled": true, "idleMinutes": 99999, "futureField": 1,
            "assignments": [
                {"ip": "127.77.0.2", "alias": "db"},
                {"ip": "127.77.0.2", "alias": "dup-ip"},
                {"ip": "127.77.0.3", "alias": "db"},
                {"ip": "127.99.0.3", "alias": "outside"},
                {"ip": "127.77.0.4", "alias": "m", "portMode": "manual", "ports": ["22", "22"]},
                {"ip": "127.77.0.5", "alias": "bad", "portMode": "manual", "ports": ["0"]}
            ]
        }));
        assert!(pool.enabled);
        assert_eq!(pool.idle_minutes, 1440);
        let kept: Vec<&str> = pool.assignments.iter().map(|a| a.alias.as_str()).collect();
        assert_eq!(kept, ["db", "m"]);
        assert_eq!(pool.assignments[1].ports, ["22"]);
        let pool = load(
            serde_json::json!({"cidr": "10.0.0.0/8", "assignments": [{"ip": "127.77.0.2", "alias": "db"}]}),
        );
        assert_eq!(pool.cidr, DEFAULT_CIDR);
        assert_eq!(pool.assignments.len(), 1);
        assert_eq!(load(serde_json::json!("garbage")), StoredIpPool::default());
    }

    #[test]
    fn long_data_dirs_get_a_short_control_dir() {
        let short = control_dir(Path::new("/home/u/.todex/ssh"));
        assert_eq!(short, Path::new("/home/u/.todex/ssh/pool"));
        let long = control_dir(&Path::new("/x").join("y".repeat(120)));
        assert!(
            long.as_os_str().len() + 16 <= MAX_CONTROL_PATH_LEN,
            "{}",
            long.display()
        );
    }
}
