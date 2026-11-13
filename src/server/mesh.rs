//! `mesh.*` WebSocket commands and the `mesh.updated` event
//! (docs/mesh-v1.md "WebSocket commands").

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::event::EventRecord;
use crate::mesh::{roster::SignedDoc, ChainOutcome, MeshStore, NodeFacts};

/// `mesh` in `/v2/version`.
pub(super) const MESH_VERSION: u8 = 1;
pub(super) const UPDATED_EVENT: &str = "mesh.updated";

pub(super) fn is_mesh_command(command_type: &str) -> bool {
    matches!(
        command_type,
        "mesh.node.get"
            | "mesh.presence.get"
            | "mesh.state.get"
            | "mesh.state.push"
            | "mesh.join.list"
            | "mesh.join.approve"
            | "mesh.join.reject"
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StateGet {
    #[serde(default)]
    group_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresenceGet {
    group_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StatePush {
    chain: Vec<SignedDoc>,
    #[serde(default)]
    node_infos: Vec<SignedDoc>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JoinApprove {
    request_id: String,
    chain: Vec<SignedDoc>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JoinReject {
    request_id: String,
}

/// The `mesh.updated` payload.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Announcement {
    reason: &'static str,
    group_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
}

impl Announcement {
    pub(super) fn new(reason: &'static str, group_id: &str) -> Self {
        Self {
            reason,
            group_id: group_id.to_owned(),
            request_id: None,
            version: None,
        }
    }

    pub(super) fn request(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.to_owned());
        self
    }

    fn version(mut self, version: u64) -> Self {
        self.version = Some(version);
        self
    }
}

/// Broadcasts `mesh.updated` to every `/v2/ws` connection.
pub(super) async fn announce(state: &AppState, announcement: Announcement) {
    match serde_json::to_value(&announcement) {
        Ok(payload) => {
            state
                .events
                .publish(EventRecord::new(UPDATED_EVENT, None, None, None, payload))
                .await;
        }
        Err(error) => tracing::warn!(error = %error, "mesh update event was not published"),
    }
}

pub(super) async fn dispatch(
    state: &AppState,
    command_type: &str,
    payload: &Value,
) -> Result<Value, AppError> {
    let Some(mesh) = &state.mesh else {
        return Err(AppError::Unsupported(
            "device groups need device auth".to_owned(),
        ));
    };
    match command_type {
        "mesh.node.get" => {
            parse::<Empty>(payload)?;
            Ok(json!({
                "nodeId": mesh.node_id(),
                "nodePublicKey": mesh.node_public_key(),
                "nodeInfo": node_info(state, mesh)?,
                "groups": mesh.group_ids()?,
            }))
        }
        "mesh.presence.get" => {
            let request = parse::<PresenceGet>(payload)?;
            let (connected, last_seen) = mesh.presence(&request.group_id)?;
            Ok(json!({ "nodeId": mesh.node_id(), "connected": connected, "lastSeen": last_seen }))
        }
        "mesh.state.get" => {
            let request = parse::<StateGet>(payload)?;
            let groups = mesh.groups(request.group_id.as_deref())?;
            Ok(json!({
                "nodeId": mesh.node_id(),
                "nodeInfo": node_info(state, mesh)?,
                "groups": groups,
            }))
        }
        "mesh.state.push" => {
            let request = parse::<StatePush>(payload)?;
            let outcome = accept_chain(state, mesh, &request.chain).await?;
            if !outcome.left
                && !request.node_infos.is_empty()
                && mesh.accept_node_infos(&outcome.group_id, &request.node_infos)?
            {
                announce(
                    state,
                    Announcement::new("node-info.updated", &outcome.group_id),
                )
                .await;
            }
            Ok(json!({
                "groupId": outcome.group_id,
                "version": outcome.version,
                "head": outcome.head,
                "changed": outcome.changed,
            }))
        }
        "mesh.join.list" => {
            parse::<Empty>(payload)?;
            let requests = state
                .device_pairing
                .mesh_join_requests(&mesh.group_ids()?)?;
            Ok(json!({ "requests": requests }))
        }
        "mesh.join.approve" => {
            let request = parse::<JoinApprove>(payload)?;
            let join = state
                .device_pairing
                .mesh_join_request(&request.request_id)?;
            let chain = crate::mesh::roster::verify_chain(&request.chain)?;
            if chain.group_id != join.group_id {
                return Err(AppError::InvalidRequest(
                    "the chain belongs to another group".to_owned(),
                ));
            }
            let listed = chain.head().member(&join.device_id).is_some_and(|member| {
                member.public_key == join.device_public_key
                    && member.history_key.as_deref() == Some(join.history_key.as_str())
            });
            if !listed {
                return Err(AppError::InvalidRequest(
                    "the chain does not list the joining device with its keys".to_owned(),
                ));
            }
            let outcome = accept_chain(state, mesh, &request.chain).await?;
            // The stored chain must be the one that lists the device: a chain
            // that lost the fork rule would approve a device the group does
            // not authorize.
            if outcome.left || outcome.head != chain.head_hash_b64() {
                return Err(AppError::Conflict(
                    "the group changed meanwhile; approve again on its current roster".to_owned(),
                ));
            }
            state
                .device_pairing
                .decide_mesh_join(&request.request_id, true)?;
            announce(
                state,
                Announcement::new("join.decided", &join.group_id).request(&request.request_id),
            )
            .await;
            Ok(json!({
                "approved": true,
                "groupId": outcome.group_id,
                "version": outcome.version,
            }))
        }
        "mesh.join.reject" => {
            let request = parse::<JoinReject>(payload)?;
            let join = state
                .device_pairing
                .mesh_join_request(&request.request_id)?;
            state
                .device_pairing
                .decide_mesh_join(&request.request_id, false)?;
            announce(
                state,
                Announcement::new("join.decided", &join.group_id).request(&request.request_id),
            )
            .await;
            Ok(json!({ "rejected": true }))
        }
        other => Err(AppError::Unsupported(format!(
            "v2 websocket command {other}"
        ))),
    }
}

/// Tracks one `/v2/ws` connection of a group device for `mesh.presence.get`
/// and announces its arrival and departure (`presence`).
pub(super) struct PresenceGuard {
    state: AppState,
    device_id: String,
}

impl PresenceGuard {
    pub(super) async fn open(state: &AppState, device_id: &str) -> Option<Self> {
        let mesh = state.mesh.as_ref()?;
        if !mesh.presence_open(device_id) {
            return None;
        }
        for group_id in mesh.groups_of_device(device_id) {
            announce(state, Announcement::new("presence", &group_id)).await;
        }
        Some(Self {
            state: state.clone(),
            device_id: device_id.to_owned(),
        })
    }
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let Some(mesh) = self.state.mesh.clone() else {
            return;
        };
        mesh.presence_close(&self.device_id);
        let state = self.state.clone();
        let groups = mesh.groups_of_device(&self.device_id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for group_id in groups {
                    announce(&state, Announcement::new("presence", &group_id)).await;
                }
            });
        }
    }
}

/// How often a node re-reads its own addresses and transport key.
const NODE_INFO_POLL: std::time::Duration = std::time::Duration::from_secs(30);
/// UDP port of the LAN beacon (docs/mesh-v1.md "LAN beacon").
pub(crate) const BEACON_PORT: u16 = 21047;

/// Re-signs this node's info when its addresses or transport key change
/// (a new network, a TUI key reset) and announces `node-info.updated`, so
/// connected devices carry the new addresses to the rest of the group. A
/// node in a group also sends the LAN beacon on every round.
pub(crate) fn spawn_node_watch(state: AppState) -> Option<tokio::task::JoinHandle<()>> {
    let mesh = state.mesh.clone()?;
    let beacon = beacon_enabled(&state);
    Some(tokio::spawn(async move {
        let mut last: Option<SignedDoc> = None;
        let mut interval = tokio::time::interval(NODE_INFO_POLL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let groups = mesh.group_ids().unwrap_or_default();
            match node_info(&state, &mesh) {
                Ok(doc) => {
                    if last.as_ref().is_some_and(|previous| *previous != doc) {
                        for group_id in &groups {
                            announce(&state, Announcement::new("node-info.updated", group_id))
                                .await;
                        }
                    }
                    last = Some(doc);
                }
                Err(error) => tracing::warn!(error = %error, "node info is unavailable"),
            }
            if beacon && !groups.is_empty() {
                send_beacon(mesh.node_id(), state.config.port).await;
            }
        }
    }))
}

/// The beacon needs a listener other devices can reach; `TODEX_AGENTD_MESH_BEACON=false` turns it off.
fn beacon_enabled(state: &AppState) -> bool {
    let disabled = std::env::var("TODEX_AGENTD_MESH_BEACON").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    });
    !disabled
        && state.config.network_enabled
        && !crate::listen_addrs::is_loopback_host(&state.config.host)
}

/// `{"t":"todex-mesh-beacon","v":1,"nodeId","port"}` to the IPv4 broadcast
/// address. It carries no group id or key: a device that knows the node id
/// still verifies the node through the pinned transport key.
async fn send_beacon(node_id: &str, port: u16) {
    let payload =
        json!({ "t": "todex-mesh-beacon", "v": 1, "nodeId": node_id, "port": port }).to_string();
    let result = async {
        let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await?;
        socket.set_broadcast(true)?;
        socket
            .send_to(payload.as_bytes(), ("255.255.255.255", BEACON_PORT))
            .await
    }
    .await;
    if let Err(error) = result {
        tracing::debug!(error = %error, "mesh beacon was not sent");
    }
}

/// Stores a pushed chain, then applies what it changed: history for added
/// devices and the announcements.
async fn accept_chain(
    state: &AppState,
    mesh: &MeshStore,
    chain: &[SignedDoc],
) -> Result<ChainOutcome, AppError> {
    let outcome = mesh.accept_chain(chain)?;
    if !outcome.changed {
        return Ok(outcome);
    }
    if outcome.joined {
        tracing::info!(group_id = %outcome.group_id, "this node joined a device group");
    }
    if outcome.left {
        tracing::info!(group_id = %outcome.group_id, "this node left a device group");
        announce(state, Announcement::new("group.left", &outcome.group_id)).await;
    } else {
        announce(
            state,
            Announcement::new("roster.updated", &outcome.group_id).version(outcome.version),
        )
        .await;
    }
    super::history_keys::adopt_group_members(
        state,
        &outcome.added_devices,
        outcome.recovery.as_ref().filter(|_| !outcome.left),
    )
    .await;
    Ok(outcome)
}

/// This node's signed info: transport key as the handshake uses it now, and
/// the addresses clients can reach it at.
pub(crate) fn node_info(state: &AppState, mesh: &MeshStore) -> Result<SignedDoc, AppError> {
    let config = &state.config;
    let keys = state.pairing_keys.current()?;
    let protocol = config.pairing_encryption;
    let mut urls = Vec::new();
    if let Some(advertised) = config.deployment.advertise_url.as_deref().map(str::trim) {
        if !advertised.is_empty() {
            urls.push(advertised.trim_end_matches('/').to_owned());
        }
    }
    if config.network_enabled {
        match crate::listen_addrs::connect_addresses(&config.host) {
            Ok(addresses) => urls.extend(
                addresses
                    .into_iter()
                    .map(|address| format!("http://{}:{}", address.host, config.port)),
            ),
            Err(error) => tracing::warn!(error = %error, "listener addresses are unavailable"),
        }
    }
    let mut seen = std::collections::HashSet::new();
    urls.retain(|url| seen.insert(url.clone()));
    let name = crate::computer::host_name();
    let name = if crate::device_pairing::validate_device_name(&name).is_ok() {
        name
    } else {
        "TodeX".to_owned()
    };
    mesh.node_info(NodeFacts {
        name,
        transport_protocol: protocol.as_str().to_owned(),
        transport_public_key: base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            keys.transport_public_key(protocol),
        ),
        urls,
    })
}

fn parse<T: serde::de::DeserializeOwned>(payload: &Value) -> Result<T, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|error| AppError::InvalidRequest(format!("invalid mesh request: {error}")))
}
