//! Device groups (docs/mesh-v1.md): this node's identity, the groups it is
//! in, the devices those groups authorize, node join codes and the short
//! admissions a node join grants.
//!
//! Only the daemon writes `mesh/groups/`; the CLI (`todex-agentd mesh
//! join-code`) and the daemon share `mesh/join-code.json`, which the daemon
//! re-reads on every use.

pub(crate) mod roster;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::SigningKey;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{devices::DeviceRecord, error::AppError, secure_fs};
use roster::{
    member_id, verify_chain, verify_node_info, Member, MemberKind, NodeInfoBody, Recovery,
    SignedDoc, VerifiedChain, NODE_INFO_DOMAIN, NODE_INFO_TYPE,
};

type Result<T> = std::result::Result<T, AppError>;

pub(crate) const DIRECTORY: &str = "mesh";
const GROUPS_DIRECTORY: &str = "groups";
const NODE_KEY_FILE: &str = "node-key.json";
const JOIN_CODE_FILE: &str = "join-code.json";
const MAX_GROUPS: usize = 16;
const MAX_GROUP_FILE: u64 = 32 * 1024 * 1024;
const JOIN_CODE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const JOIN_CODE_MAX_FAILURES: u32 = 5;
const ADMISSION_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_ADMISSIONS: usize = 16;
const JOIN_CODE_LABEL: &[u8] = b"todex.mesh.v1/join-code\0";
const NODE_JOIN_CLIENT_LABEL: &[u8] = b"todex.mesh.v1/node-join/client\0";
const NODE_JOIN_SERVER_LABEL: &[u8] = b"todex.mesh.v1/node-join/server\0";
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeKeyFile {
    version: u8,
    secret_key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupFile {
    version: u8,
    chain: Vec<SignedDoc>,
    #[serde(default)]
    node_infos: BTreeMap<String, SignedDoc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JoinCode {
    pub code: String,
    pub created_at: u64,
    pub expires_at: u64,
}

struct Group {
    chain: VerifiedChain,
    /// Other nodes' info, highest `seq` kept.
    node_infos: BTreeMap<String, (SignedDoc, u64)>,
}

struct Admission {
    device_id: String,
    public_key: String,
    expires: Instant,
}

/// A group device's connections to this node (`mesh.presence.get`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevicePresence {
    pub device_id: String,
    pub connections: u32,
    /// When its first open connection started.
    pub connected_at: u64,
}

struct Inner {
    /// Open connections of group devices; `last_seen` when they closed.
    presence: BTreeMap<String, DevicePresence>,
    last_seen: BTreeMap<String, u64>,
    groups: BTreeMap<String, Group>,
    admissions: Vec<Admission>,
    join_code_failures: u32,
    /// This node's latest signed info and its body.
    node_info: Option<(SignedDoc, NodeInfoBody)>,
}

/// What [`MeshStore::accept_chain`] did.
#[derive(Debug, Default)]
pub(crate) struct ChainOutcome {
    pub group_id: String,
    pub version: u64,
    pub head: String,
    /// The stored chain changed.
    pub changed: bool,
    /// This node joined the group with this chain.
    pub joined: bool,
    /// The chain no longer lists this node: it left the group.
    pub left: bool,
    /// Devices the new head lists that the old one did not (all of them
    /// on a join).
    pub added_devices: Vec<Member>,
    pub recovery: Option<Recovery>,
}

/// A group's chain and the other nodes' info, as `mesh.state.get` sends it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GroupDocs {
    pub group_id: String,
    pub chain: Vec<SignedDoc>,
    pub node_infos: Vec<SignedDoc>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GroupSummary {
    pub group_id: String,
    pub name: String,
    pub version: u64,
    pub members: Vec<MemberSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemberSummary {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
}

/// The transport facts a node info document carries.
pub(crate) struct NodeFacts {
    pub name: String,
    pub transport_protocol: String,
    pub transport_public_key: String,
    pub urls: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct MeshStore {
    directory: PathBuf,
    node_key: Arc<SigningKey>,
    node_id: String,
    inner: Arc<Mutex<Inner>>,
}

impl MeshStore {
    pub(crate) fn load(data_dir: &Path) -> Result<Self> {
        let directory = data_dir.join(DIRECTORY);
        secure_fs::ensure_owner_only_dir(&directory.join(GROUPS_DIRECTORY))?;
        secure_fs::set_owner_only(&directory, true)?;
        let node_key = load_or_create_node_key(&directory.join(NODE_KEY_FILE))?;
        let node_id = member_id("node_", node_key.verifying_key().as_bytes());
        let mut groups = BTreeMap::new();
        for entry in fs::read_dir(directory.join(GROUPS_DIRECTORY))?.take(MAX_GROUPS * 4) {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match read_group(&path, &node_id) {
                Ok(group) => {
                    groups.insert(group.chain.group_id.clone(), group);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "ignoring an unreadable mesh group");
                }
            }
        }
        Ok(Self {
            directory,
            node_key: Arc::new(node_key),
            node_id,
            inner: Arc::new(Mutex::new(Inner {
                presence: BTreeMap::new(),
                last_seen: BTreeMap::new(),
                groups,
                admissions: Vec::new(),
                join_code_failures: 0,
                node_info: None,
            })),
        })
    }

    pub(crate) fn node_id(&self) -> &str {
        &self.node_id
    }

    pub(crate) fn node_public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.node_key.verifying_key().as_bytes())
    }

    pub(crate) fn group_ids(&self) -> Result<Vec<String>> {
        Ok(self.lock()?.groups.keys().cloned().collect())
    }

    pub(crate) fn has_group(&self, group_id: &str) -> bool {
        self.lock()
            .map(|inner| inner.groups.contains_key(group_id))
            .unwrap_or(false)
    }

    /// The device `device_id` as a group member (or an admitted key), for
    /// the device registry. Unknown devices are `None`.
    pub(crate) fn device_record(&self, device_id: &str) -> Option<DeviceRecord> {
        let mut inner = self.inner.lock().ok()?;
        for group in inner.groups.values() {
            if let Some(member) = group
                .chain
                .head()
                .member(device_id)
                .filter(|member| member.kind == MemberKind::Device)
            {
                return Some(DeviceRecord {
                    device_id: member.id.clone(),
                    name: member.name.clone(),
                    public_key: member.public_key.clone(),
                    paired_at: member.added_at,
                    last_seen_at: None,
                });
            }
        }
        let now = Instant::now();
        inner.admissions.retain(|admission| admission.expires > now);
        inner
            .admissions
            .iter()
            .find(|admission| admission.device_id == device_id)
            .map(|admission| DeviceRecord {
                device_id: admission.device_id.clone(),
                name: "mesh admission".to_owned(),
                public_key: admission.public_key.clone(),
                paired_at: unix_ms(),
                last_seen_at: None,
            })
    }

    /// Lets `public_key` authenticate for [`ADMISSION_TTL`] after a node
    /// join, until it pushes a chain that lists this node.
    pub(crate) fn admit(&self, public_key: &[u8; 32]) -> Result<String> {
        let device_id = member_id("dev_", public_key);
        let mut inner = self.lock()?;
        let now = Instant::now();
        inner
            .admissions
            .retain(|admission| admission.expires > now && admission.device_id != device_id);
        if inner.admissions.len() >= MAX_ADMISSIONS {
            inner.admissions.remove(0);
        }
        inner.admissions.push(Admission {
            device_id: device_id.clone(),
            public_key: URL_SAFE_NO_PAD.encode(public_key),
            expires: now + ADMISSION_TTL,
        });
        Ok(device_id)
    }

    /// Verifies `docs` and stores it under the fork rule; see
    /// "Accepting a chain" in docs/mesh-v1.md. The caller is already
    /// authenticated, which is what joining a new group requires.
    pub(crate) fn accept_chain(&self, docs: &[SignedDoc]) -> Result<ChainOutcome> {
        let chain = verify_chain(docs)?;
        let node_key = self.node_public_key();
        let lists_this_node = chain
            .head()
            .member(&self.node_id)
            .is_some_and(|member| member.kind == MemberKind::Node && member.public_key == node_key);
        let mut inner = self.lock()?;
        let mut outcome = ChainOutcome {
            group_id: chain.group_id.clone(),
            recovery: chain.head().recovery.clone(),
            ..ChainOutcome::default()
        };
        let path = self.group_path(&chain.group_id);
        match inner.groups.get(&chain.group_id) {
            Some(current) if !chain.wins_over(&current.chain) => {
                outcome.version = current.chain.version();
                outcome.head = current.chain.head_hash_b64();
                return Ok(outcome);
            }
            Some(current) => {
                let before: Vec<String> = current
                    .chain
                    .head()
                    .devices()
                    .map(|member| member.id.clone())
                    .collect();
                outcome.added_devices = chain
                    .head()
                    .devices()
                    .filter(|member| !before.contains(&member.id))
                    .cloned()
                    .collect();
            }
            None => {
                if !lists_this_node {
                    return Err(AppError::NotFound(format!(
                        "this node is not in group {}",
                        chain.group_id
                    )));
                }
                if inner.groups.len() >= MAX_GROUPS {
                    return Err(AppError::ResourceExhausted(
                        "this node is in too many groups".to_owned(),
                    ));
                }
                outcome.joined = true;
                outcome.added_devices = chain.head().devices().cloned().collect();
            }
        }
        outcome.changed = true;
        outcome.version = chain.version();
        outcome.head = chain.head_hash_b64();
        if !lists_this_node {
            outcome.left = true;
            outcome.added_devices.clear();
            inner.groups.remove(&chain.group_id);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(outcome);
        }
        let node_infos = inner
            .groups
            .remove(&chain.group_id)
            .map(|group| group.node_infos)
            .unwrap_or_default()
            .into_iter()
            .filter(|(node_id, (doc, _))| {
                chain
                    .head()
                    .member(node_id)
                    .is_some_and(|member| verify_node_info(doc, &member.public_key).is_ok())
            })
            .collect();
        let group = Group { chain, node_infos };
        write_group(&path, &group)?;
        // A device the chain now lists no longer needs its admission.
        let members: Vec<String> = group
            .chain
            .head()
            .devices()
            .map(|member| member.id.clone())
            .collect();
        inner
            .admissions
            .retain(|admission| !members.contains(&admission.device_id));
        inner.groups.insert(outcome.group_id.clone(), group);
        Ok(outcome)
    }

    /// Stores other nodes' info documents for `group_id`; returns whether
    /// any changed. Invalid ones and this node's own are skipped.
    pub(crate) fn accept_node_infos(&self, group_id: &str, docs: &[SignedDoc]) -> Result<bool> {
        let mut inner = self.lock()?;
        let Some(group) = inner.groups.get_mut(group_id) else {
            return Err(AppError::NotFound(format!(
                "this node is not in group {group_id}"
            )));
        };
        let mut changed = false;
        for doc in docs.iter().take(roster::MAX_MEMBERS) {
            let Ok(bytes) = doc.body_bytes() else {
                continue;
            };
            let Ok(claimed) = serde_json::from_slice::<NodeInfoBody>(&bytes) else {
                continue;
            };
            if claimed.node_id == self.node_id {
                continue;
            }
            let Some(member) = group.chain.head().member(&claimed.node_id) else {
                continue;
            };
            let Ok(body) = verify_node_info(doc, &member.public_key) else {
                continue;
            };
            if group
                .node_infos
                .get(&body.node_id)
                .is_some_and(|(_, seq)| *seq >= body.seq)
            {
                continue;
            }
            group
                .node_infos
                .insert(body.node_id.clone(), (doc.clone(), body.seq));
            changed = true;
        }
        if changed {
            let group = inner
                .groups
                .get(group_id)
                .expect("group exists while locked");
            write_group(&self.group_path(group_id), group)?;
        }
        Ok(changed)
    }

    /// The groups (all, or one) with their chains and other nodes' info.
    pub(crate) fn groups(&self, group_id: Option<&str>) -> Result<Vec<GroupDocs>> {
        let inner = self.lock()?;
        if let Some(group_id) = group_id {
            if !inner.groups.contains_key(group_id) {
                return Err(AppError::NotFound(format!(
                    "this node is not in group {group_id}"
                )));
            }
        }
        Ok(inner
            .groups
            .iter()
            .filter(|(id, _)| group_id.is_none_or(|wanted| wanted == id.as_str()))
            .map(|(id, group)| GroupDocs {
                group_id: id.clone(),
                chain: group.chain.docs.clone(),
                node_infos: group
                    .node_infos
                    .values()
                    .map(|(doc, _)| doc.clone())
                    .collect(),
            })
            .collect())
    }

    /// This node's info, re-signed when a fact changed.
    pub(crate) fn node_info(&self, facts: NodeFacts) -> Result<SignedDoc> {
        let mut inner = self.lock()?;
        let mut urls = facts.urls;
        urls.dedup();
        urls.truncate(roster::MAX_NODE_URLS);
        if let Some((doc, body)) = &inner.node_info {
            if body.name == facts.name
                && body.transport_protocol == facts.transport_protocol
                && body.transport_public_key == facts.transport_public_key
                && body.urls == urls
            {
                return Ok(doc.clone());
            }
        }
        let now = unix_ms();
        let seq = inner
            .node_info
            .as_ref()
            .map_or(now, |(_, body)| now.max(body.seq + 1));
        let body = NodeInfoBody {
            doc_type: NODE_INFO_TYPE.to_owned(),
            v: 1,
            node_id: self.node_id.clone(),
            public_key: self.node_public_key(),
            seq,
            name: facts.name,
            transport_protocol: facts.transport_protocol,
            transport_public_key: facts.transport_public_key,
            urls,
            signed_at: now,
        };
        let doc = SignedDoc::sign(
            NODE_INFO_DOMAIN,
            &serde_json::to_vec(&body)?,
            &self.node_key,
        );
        inner.node_info = Some((doc.clone(), body));
        Ok(doc)
    }

    /// The current join code, created when missing or expired (`new`
    /// replaces it either way).
    pub(crate) fn join_code(&self, new: bool) -> Result<JoinCode> {
        let path = self.directory.join(JOIN_CODE_FILE);
        if !new {
            if let Some(code) = read_join_code(&path)? {
                return Ok(code);
            }
        }
        let mut bytes = [0u8; 10];
        OsRng.fill_bytes(&mut bytes);
        let created_at = unix_ms();
        let code = JoinCode {
            code: format_join_code(&bytes),
            created_at,
            expires_at: created_at + JOIN_CODE_TTL.as_millis() as u64,
        };
        secure_fs::write_owner_only_atomic(&path, &serde_json::to_vec_pretty(&code)?)?;
        if let Ok(mut inner) = self.lock() {
            inner.join_code_failures = 0;
        }
        Ok(code)
    }

    /// A join code exists and has not expired.
    pub(crate) fn has_join_code(&self) -> Result<bool> {
        Ok(read_join_code(&self.directory.join(JOIN_CODE_FILE))?.is_some())
    }

    /// Checks a node join's client proof; on success consumes the code and
    /// returns its key for the server proof. Five wrong proofs discard the
    /// code.
    pub(crate) fn check_join_proof(
        &self,
        transcript_hash: &[u8; 32],
        proof: &[u8],
    ) -> Result<Zeroizing<[u8; 32]>> {
        let path = self.directory.join(JOIN_CODE_FILE);
        let mut inner = self.lock()?;
        let Some(code) = read_join_code(&path)? else {
            return Err(AppError::Unauthenticated);
        };
        let key = join_code_key(&code.code);
        let expected = join_mac(&key, &[NODE_JOIN_CLIENT_LABEL, transcript_hash]);
        if proof.len() != 32 || !bool::from(expected.ct_eq(proof)) {
            inner.join_code_failures += 1;
            if inner.join_code_failures >= JOIN_CODE_MAX_FAILURES {
                inner.join_code_failures = 0;
                remove_if_exists(&path)?;
                tracing::warn!("mesh join code discarded after repeated wrong proofs");
            }
            return Err(AppError::Unauthenticated);
        }
        inner.join_code_failures = 0;
        remove_if_exists(&path)?;
        Ok(key)
    }

    /// A connection of `device_id` opened; `false` when it is not a group
    /// device (nothing is tracked then).
    pub(crate) fn presence_open(&self, device_id: &str) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        let member = inner.groups.values().any(|group| {
            group
                .chain
                .head()
                .member(device_id)
                .is_some_and(|member| member.kind == MemberKind::Device)
        });
        if !member {
            return false;
        }
        let entry = inner
            .presence
            .entry(device_id.to_owned())
            .or_insert_with(|| DevicePresence {
                device_id: device_id.to_owned(),
                connections: 0,
                connected_at: unix_ms(),
            });
        entry.connections += 1;
        true
    }

    /// A connection [`Self::presence_open`] tracked closed.
    pub(crate) fn presence_close(&self, device_id: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let gone = match inner.presence.get_mut(device_id) {
            Some(entry) => {
                entry.connections = entry.connections.saturating_sub(1);
                entry.connections == 0
            }
            None => false,
        };
        if gone {
            inner.presence.remove(device_id);
            inner.last_seen.insert(device_id.to_owned(), unix_ms());
        }
    }

    /// Connected devices of `group_id` and when the others were last seen
    /// here (since this daemon started).
    pub(crate) fn presence(
        &self,
        group_id: &str,
    ) -> Result<(Vec<DevicePresence>, BTreeMap<String, u64>)> {
        let inner = self.lock()?;
        let Some(group) = inner.groups.get(group_id) else {
            return Err(AppError::NotFound(format!(
                "this node is not in group {group_id}"
            )));
        };
        let devices: Vec<String> = group
            .chain
            .head()
            .devices()
            .map(|member| member.id.clone())
            .collect();
        Ok((
            devices
                .iter()
                .filter_map(|id| inner.presence.get(id).cloned())
                .collect(),
            devices
                .iter()
                .filter_map(|id| inner.last_seen.get(id).map(|at| (id.clone(), *at)))
                .collect(),
        ))
    }

    /// Groups that list `device_id` as a device.
    pub(crate) fn groups_of_device(&self, device_id: &str) -> Vec<String> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .groups
                    .iter()
                    .filter(|(_, group)| group.chain.head().member(device_id).is_some())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Who may relay to `target` through this node (docs/mesh-v1.md
    /// "Relay"): a group lists this node (not `denied`), `target` as another
    /// node reachable by relay, and `device_id` as a device. Returns the
    /// device's key and the target's addresses (pinned first, then announced).
    pub(crate) fn relay_route(
        &self,
        device_id: &str,
        target: &str,
    ) -> Result<(String, Vec<String>)> {
        let inner = self.lock()?;
        for group in inner.groups.values() {
            let head = group.chain.head();
            let (Some(this), Some(node), Some(device)) = (
                head.member(&self.node_id),
                head.member(target),
                head.member(device_id),
            ) else {
                continue;
            };
            if target == self.node_id
                || !this.relays()
                || !node.reachable_by_relay()
                || device.kind != MemberKind::Device
            {
                continue;
            }
            let mut urls = node.addresses.clone();
            if let Some((doc, _)) = group.node_infos.get(target) {
                if let Ok(info) = verify_node_info(doc, &node.public_key) {
                    urls.extend(info.urls);
                }
            }
            let mut seen = std::collections::HashSet::new();
            urls.retain(|url| seen.insert(url.clone()));
            return Ok((device.public_key.clone(), urls));
        }
        Err(AppError::Unauthorized(
            "this node does not relay to that node for this device".to_owned(),
        ))
    }

    /// Name, version and members of every group, for status output.
    pub(crate) fn summaries(&self) -> Result<Vec<GroupSummary>> {
        Ok(self
            .lock()?
            .groups
            .iter()
            .map(|(group_id, group)| {
                let head = group.chain.head();
                GroupSummary {
                    group_id: group_id.clone(),
                    name: head.name.clone(),
                    version: head.version,
                    members: head
                        .members
                        .iter()
                        .map(|member| MemberSummary {
                            id: member.id.clone(),
                            kind: match member.kind {
                                MemberKind::Device => "device",
                                MemberKind::Node => "node",
                            },
                            name: member.name.clone(),
                        })
                        .collect(),
                }
            })
            .collect())
    }

    fn group_path(&self, group_id: &str) -> PathBuf {
        self.directory
            .join(GROUPS_DIRECTORY)
            .join(format!("{group_id}.json"))
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| AppError::InvalidRequest("mesh state is unavailable".to_owned()))
    }
}

/// `b64u(HMAC(codeKey, server label || transcript hash || node key))`.
pub(crate) fn node_join_server_proof(
    code_key: &[u8; 32],
    transcript_hash: &[u8; 32],
    node_public_key: &[u8; 32],
) -> String {
    URL_SAFE_NO_PAD.encode(join_mac(
        code_key,
        &[NODE_JOIN_SERVER_LABEL, transcript_hash, node_public_key],
    ))
}

#[cfg(test)]
pub(crate) fn client_proof_for_tests(code_key: &[u8; 32], transcript_hash: &[u8; 32]) -> Vec<u8> {
    join_mac(code_key, &[NODE_JOIN_CLIENT_LABEL, transcript_hash]).to_vec()
}

/// Uppercase, without `-` and whitespace, `0→O`, `1→I`, `8→B`.
pub(crate) fn normalize_join_code(code: &str) -> String {
    code.chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .map(|character| match character.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            other => other,
        })
        .collect()
}

pub(crate) fn join_code_key(code: &str) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(JOIN_CODE_LABEL);
    hasher.update(normalize_join_code(code).as_bytes());
    Zeroizing::new(hasher.finalize().into())
}

fn join_mac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// 80 bits as 16 base32 characters, `XXXX-XXXX-XXXX-XXXX`.
fn format_join_code(bytes: &[u8; 10]) -> String {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = String::with_capacity(19);
    for byte in bytes {
        bits = (bits << 8) | u32::from(*byte);
        count += 8;
        while count >= 5 {
            count -= 5;
            if out.len() % 5 == 4 {
                out.push('-');
            }
            out.push(BASE32[((bits >> count) & 31) as usize] as char);
        }
    }
    out
}

fn read_join_code(path: &Path) -> Result<Option<JoinCode>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let code: JoinCode = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::InvalidRequest("mesh join code file is malformed".to_owned()))?;
    Ok((code.expires_at > unix_ms()).then_some(code))
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn load_or_create_node_key(path: &Path) -> Result<SigningKey> {
    match fs::read(path) {
        Ok(bytes) => {
            let file: NodeKeyFile = serde_json::from_slice(&bytes).map_err(|_| {
                AppError::InvalidRequest("mesh node key file is malformed".to_owned())
            })?;
            let seed: [u8; 32] = URL_SAFE_NO_PAD
                .decode(&file.secret_key)
                .ok()
                .and_then(|seed| seed.try_into().ok())
                .ok_or_else(|| AppError::InvalidRequest("mesh node key is malformed".to_owned()))?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut seed = Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(seed.as_mut());
            let file = NodeKeyFile {
                version: 1,
                secret_key: URL_SAFE_NO_PAD.encode(seed.as_ref()),
            };
            let bytes = Zeroizing::new(serde_json::to_vec(&file)?);
            secure_fs::create_owner_only(path, &bytes)?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(error) => Err(error.into()),
    }
}

fn read_group(path: &Path, node_id: &str) -> Result<Group> {
    if fs::metadata(path)?.len() > MAX_GROUP_FILE {
        return Err(AppError::InvalidRequest(
            "mesh group file is too large".to_owned(),
        ));
    }
    let file: GroupFile = serde_json::from_slice(&fs::read(path)?)
        .map_err(|_| AppError::InvalidRequest("mesh group file is malformed".to_owned()))?;
    let chain = verify_chain(&file.chain)?;
    if chain.head().member(node_id).is_none() {
        return Err(AppError::InvalidRequest(
            "stored mesh group no longer lists this node".to_owned(),
        ));
    }
    let node_infos = file
        .node_infos
        .into_iter()
        .filter_map(|(id, doc)| {
            let member = chain.head().member(&id)?;
            let body = verify_node_info(&doc, &member.public_key).ok()?;
            Some((id, (doc, body.seq)))
        })
        .collect();
    Ok(Group { chain, node_infos })
}

fn write_group(path: &Path, group: &Group) -> Result<()> {
    let file = GroupFile {
        version: 1,
        chain: group.chain.docs.clone(),
        node_infos: group
            .node_infos
            .iter()
            .map(|(id, (doc, _))| (id.clone(), doc.clone()))
            .collect(),
    };
    secure_fs::write_owner_only_atomic(path, &serde_json::to_vec(&file)?)?;
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::roster::test_support::*;
    use super::*;

    fn store() -> (PathBuf, MeshStore) {
        let root = std::env::temp_dir().join(format!("todex-mesh-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = MeshStore::load(&root).unwrap();
        (root, store)
    }

    fn node_key(store: &MeshStore) -> ed25519_dalek::VerifyingKey {
        roster::parse_signing_public_key(&store.node_public_key()).unwrap()
    }

    #[test]
    fn the_node_key_survives_a_restart() {
        let (root, store) = store();
        let again = MeshStore::load(&root).unwrap();
        assert_eq!(store.node_id(), again.node_id());
        assert!(store.node_id().starts_with("node_"));
    }

    #[test]
    fn a_chain_listing_the_node_joins_and_authorizes_its_devices() {
        let (root, store) = store();
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        // A chain that does not list this node is refused.
        assert!(matches!(
            store.accept_chain(&chain),
            Err(AppError::NotFound(_))
        ));
        let head = verify_chain(&chain).unwrap().head().members.clone();
        let device_id = head[0].id.clone();
        let joined = append(
            &chain,
            &creator,
            [head, vec![node_member(&node_key(&store), &device_id)]].concat(),
        );
        let outcome = store.accept_chain(&joined).unwrap();
        assert!(outcome.joined && outcome.changed);
        assert_eq!(outcome.added_devices.len(), 1);
        assert!(store.device_record(&device_id).is_some());
        // The same chain again changes nothing; a restart keeps the group.
        assert!(!store.accept_chain(&joined).unwrap().changed);
        let reloaded = MeshStore::load(&root).unwrap();
        assert!(reloaded.device_record(&device_id).is_some());
    }

    #[test]
    fn a_chain_without_the_node_leaves_the_group() {
        let (root, store) = store();
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let head = verify_chain(&chain).unwrap().head().members.clone();
        let device_id = head[0].id.clone();
        let joined = append(
            &chain,
            &creator,
            [
                head.clone(),
                vec![node_member(&node_key(&store), &device_id)],
            ]
            .concat(),
        );
        store.accept_chain(&joined).unwrap();
        let removed = append(&joined, &creator, head);
        let outcome = store.accept_chain(&removed).unwrap();
        assert!(outcome.left);
        assert!(store.device_record(&device_id).is_none());
        assert!(MeshStore::load(&root)
            .unwrap()
            .group_ids()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn removed_devices_stop_authenticating() {
        let (_root, store) = store();
        let creator = device_key(1);
        let other = device_key(2);
        let chain = genesis(&creator, 1);
        let head = verify_chain(&chain).unwrap().head().members.clone();
        let creator_id = head[0].id.clone();
        let node = node_member(&node_key(&store), &creator_id);
        let other_member = device_member(&other, 2, &creator_id);
        let other_id = other_member.id.clone();
        let both = append(
            &chain,
            &creator,
            [head.clone(), vec![node.clone(), other_member]].concat(),
        );
        assert_eq!(store.accept_chain(&both).unwrap().added_devices.len(), 2);
        assert!(store.device_record(&other_id).is_some());
        let without = append(&both, &creator, [head, vec![node]].concat());
        store.accept_chain(&without).unwrap();
        assert!(store.device_record(&other_id).is_none());
    }

    #[test]
    fn join_codes_are_single_use_and_wrong_proofs_are_counted() {
        let (_root, store) = store();
        let code = store.join_code(false).unwrap();
        assert_eq!(code.code.len(), 19);
        assert_eq!(store.join_code(false).unwrap().code, code.code);
        let transcript = [7u8; 32];
        for _ in 0..JOIN_CODE_MAX_FAILURES - 1 {
            assert!(store.check_join_proof(&transcript, &[0; 32]).is_err());
        }
        let key = join_code_key(&code.code.to_lowercase());
        let proof = join_mac(&key, &[NODE_JOIN_CLIENT_LABEL, &transcript]);
        assert!(store.check_join_proof(&transcript, &proof).is_ok());
        assert!(!store.has_join_code().unwrap());
        // Five wrong proofs discard a code.
        store.join_code(true).unwrap();
        for _ in 0..JOIN_CODE_MAX_FAILURES {
            assert!(store.check_join_proof(&transcript, &[0; 32]).is_err());
        }
        assert!(!store.has_join_code().unwrap());
    }

    #[test]
    fn admissions_authenticate_until_the_chain_lists_the_device() {
        let (_root, store) = store();
        let creator = device_key(1);
        let device_id = store.admit(creator.verifying_key().as_bytes()).unwrap();
        assert!(store.device_record(&device_id).is_some());
        let chain = genesis(&creator, 1);
        let head = verify_chain(&chain).unwrap().head().members.clone();
        let joined = append(
            &chain,
            &creator,
            [head, vec![node_member(&node_key(&store), &device_id)]].concat(),
        );
        store.accept_chain(&joined).unwrap();
        assert!(store.lock().unwrap().admissions.is_empty());
        assert!(store.device_record(&device_id).is_some());
    }

    #[test]
    fn node_info_is_resigned_only_when_a_fact_changes() {
        let (_root, store) = store();
        let facts = || NodeFacts {
            name: "nas".to_owned(),
            transport_protocol: "x25519".to_owned(),
            transport_public_key: "AAAA".to_owned(),
            urls: vec!["http://10.0.0.2:7345".to_owned()],
        };
        let first = store.node_info(facts()).unwrap();
        assert_eq!(store.node_info(facts()).unwrap(), first);
        let moved = store
            .node_info(NodeFacts {
                urls: vec!["http://10.0.0.3:7345".to_owned()],
                ..facts()
            })
            .unwrap();
        assert_ne!(moved, first);
        let body = verify_node_info(&moved, &store.node_public_key()).unwrap();
        assert_eq!(body.urls, vec!["http://10.0.0.3:7345".to_owned()]);
    }

    #[test]
    fn relays_follow_the_roster_settings_and_presence_counts_connections() {
        let (_root, store) = store();
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let me = verify_chain(&chain).unwrap().head().members[0].clone();
        let this = node_member(&node_key(&store), &me.id);
        let mut target = node_member(&device_key(9).verifying_key(), &me.id);
        target.addresses = vec!["http://10.0.0.9:7345".to_owned()];
        let joined = append(
            &chain,
            &creator,
            vec![me.clone(), this.clone(), target.clone()],
        );
        store.accept_chain(&joined).unwrap();
        let (key, urls) = store.relay_route(&me.id, &target.id).unwrap();
        assert_eq!(key, me.public_key);
        assert_eq!(urls, vec!["http://10.0.0.9:7345".to_owned()]);
        // Not to itself, not for strangers.
        assert!(store.relay_route(&me.id, &this.id).is_err());
        assert!(store.relay_route("dev_stranger", &target.id).is_err());
        // A target that refuses relays, then a relay that is denied.
        let closed = Member {
            relay_target: Some(false),
            ..target.clone()
        };
        let joined = append(&joined, &creator, vec![me.clone(), this.clone(), closed]);
        store.accept_chain(&joined).unwrap();
        assert!(store.relay_route(&me.id, &target.id).is_err());
        let denied = Member {
            relay: Some(roster::RelayRole::Denied),
            ..this.clone()
        };
        let joined = append(&joined, &creator, vec![me.clone(), denied, target.clone()]);
        store.accept_chain(&joined).unwrap();
        assert!(store.relay_route(&me.id, &target.id).is_err());

        let group_id = verify_chain(&joined).unwrap().group_id;
        assert!(!store.presence_open("dev_stranger"));
        assert!(store.presence_open(&me.id) && store.presence_open(&me.id));
        assert_eq!(store.presence(&group_id).unwrap().0[0].connections, 2);
        store.presence_close(&me.id);
        store.presence_close(&me.id);
        let (connected, last_seen) = store.presence(&group_id).unwrap();
        assert!(connected.is_empty() && last_seen.contains_key(&me.id));
    }

    #[test]
    fn join_codes_use_the_base32_alphabet() {
        assert_eq!(format_join_code(&[0; 10]), "AAAA-AAAA-AAAA-AAAA");
        assert_eq!(format_join_code(&[0xff; 10]), "7777-7777-7777-7777");
        assert_eq!(normalize_join_code("ab01-8 cd"), "ABOIBCD");
    }
}
