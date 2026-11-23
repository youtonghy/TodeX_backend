//! Hubs (docs/mesh-v1.md "Hub"): a node a group marks `hub: true` is the
//! one its other nodes keep an uplink to whatever their relay settings. Over
//! that control socket both ends hand each other the state of the groups
//! they share with a hub (chains and signed node infos, as `mesh.state.push`
//! carries them), so nodes learn each other's addresses without a device in
//! between. The hub also tells the node the public address it connected
//! from, once it checked that the node answers there; the node announces it
//! in its info.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use serde::Deserialize;
use serde_json::json;

use super::mesh::{announce, Announcement};
use crate::{
    app_state::AppState,
    mesh::{roster::SignedDoc, MeshStore},
};

/// How often a link checks whether its state changed and sends it.
pub(super) const SYNC_INTERVAL: Duration = Duration::from_secs(5);
/// How often a hub checks a node's observed address again.
pub(super) const OBSERVED_RECHECK: Duration = Duration::from_secs(10 * 60);
/// Groups one `state` frame may carry (a node is in at most 16).
const MAX_FRAME_GROUPS: usize = 16;
/// Ports of a node's info tried at its observed address.
const MAX_OBSERVED_PORTS: usize = 3;

/// One group of a `state` frame.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PushedGroup {
    group_id: String,
    chain: Vec<SignedDoc>,
    #[serde(default)]
    node_infos: Vec<SignedDoc>,
}

/// A hub frame on an uplink control socket; `open` stays with the uplink.
#[derive(Debug)]
pub(super) enum Frame {
    /// `{"t":"state","groups":[{groupId, chain, nodeInfos}]}`
    State(Vec<PushedGroup>),
    /// `{"t":"observed","url":"http://<ip>:<port>"|null}`
    Observed(Option<String>),
}

/// The hub frame `text` is, if any. Unknown frames are ignored, as older
/// nodes ignore these.
pub(super) fn parse_frame(text: &str) -> Option<Frame> {
    #[derive(Deserialize)]
    #[serde(tag = "t", rename_all = "camelCase")]
    enum Raw {
        State { groups: Vec<PushedGroup> },
        Observed { url: Option<String> },
    }
    match serde_json::from_str::<Raw>(text).ok()? {
        Raw::State { groups } => Some(Frame::State(groups)),
        Raw::Observed { url } => Some(Frame::Observed(url)),
    }
}

pub(super) fn observed_frame(url: Option<&str>) -> String {
    json!({ "t": "observed", "url": url }).to_string()
}

/// Sends a link's state only when it changed since the last frame.
#[derive(Default)]
pub(super) struct StateSync {
    last: Option<String>,
}

impl StateSync {
    /// The `state` frame for `peer` when it changed; `None` while it did
    /// not, or while no hub joins the two.
    pub(super) fn next(&mut self, mesh: &MeshStore, peer: &str) -> Option<String> {
        // The digest is cheap; the state (whole chains) is built only when
        // it changed.
        let digest = mesh.hub_state_digest(peer);
        if self.last.as_deref() == Some(digest.as_str()) {
            return None;
        }
        let groups = mesh.hub_state(peer);
        if groups.is_empty() {
            self.last = None;
            return None;
        }
        self.last = Some(digest);
        Some(json!({ "t": "state", "groups": groups }).to_string())
    }
}

/// Stores a `state` frame from `peer` as `mesh.state.push` would, for the
/// groups this node shares with it through a hub only: a hub cannot add this
/// node to a group, nor learn of a group it is not in. Every chain and info
/// is verified; failures are logged and skipped.
pub(super) async fn accept_state(
    state: &AppState,
    mesh: &MeshStore,
    peer: &str,
    groups: Vec<PushedGroup>,
) {
    let shared = mesh.hub_groups(peer);
    for group in groups.into_iter().take(MAX_FRAME_GROUPS) {
        if !shared.contains(&group.group_id) {
            continue;
        }
        let same_group = crate::mesh::roster::verify_chain(&group.chain)
            .is_ok_and(|chain| chain.group_id == group.group_id);
        if !same_group {
            tracing::debug!(%peer, group_id = %group.group_id, "hub state carried an invalid chain");
            continue;
        }
        let outcome = match super::mesh::accept_chain(state, mesh, &group.chain).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::debug!(%peer, error = %error, "hub state was not accepted");
                continue;
            }
        };
        if outcome.left || group.node_infos.is_empty() {
            continue;
        }
        match mesh.accept_node_infos(&outcome.group_id, &group.node_infos) {
            Ok(true) => {
                announce(
                    state,
                    Announcement::new("node-info.updated", &outcome.group_id),
                )
                .await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(%peer, error = %error, "hub node infos were not accepted");
            }
        }
    }
}

/// Where a hub saw a node connect from: its TCP peer, or the client a
/// trusted reverse proxy names.
pub(super) fn connecting_address(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    extensions: &axum::http::Extensions,
) -> Option<IpAddr> {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|axum::extract::ConnectInfo(address)| address.ip())?;
    let forwarded = crate::listen_addrs::forwarded(
        headers,
        extensions,
        Some(peer),
        &state.config.security.trusted_proxies,
    );
    Some(crate::listen_addrs::client_address(peer, forwarded))
}

/// The addresses to try for a node that connected from `ip`: that IP with
/// each port its info announces (at most three). None for an IP that is
/// not public: a node on the hub's LAN announces its LAN addresses itself.
pub(super) fn observed_candidates(ip: IpAddr, urls: &[String]) -> Vec<String> {
    let ip = ip.to_canonical();
    if !is_public(ip) {
        return Vec::new();
    }
    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let mut ports = Vec::new();
    for url in urls {
        let Some(port) = reqwest::Url::parse(url)
            .ok()
            .and_then(|url| url.port_or_known_default())
        else {
            continue;
        };
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
        .into_iter()
        .take(MAX_OBSERVED_PORTS)
        .map(|port| format!("http://{host}:{port}"))
        .collect()
}

/// The first of `candidates` where `node` answers as itself.
pub(super) async fn check_observed(node: &str, candidates: &[String]) -> Option<String> {
    for url in candidates {
        if super::mesh_relay::answers_as(node, url).await {
            return Some(url.clone());
        }
    }
    None
}

/// An `observed` address a node takes from its hub: `http://` and a public
/// IP, as [`observed_candidates`] builds them.
pub(super) fn acceptable_observed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "http"
        && parsed.path() == "/"
        && crate::mesh::roster::valid_address(url)
        && parsed
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(is_public)
}

/// An IP other machines on the internet can reach: not private, shared
/// (CGNAT), loopback, link-local, multicast, documentation or unspecified.
fn is_public(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (b & 0xc0) == 64))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first == 0x2001 && v6.segments()[1] == 0x0db8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_parse_and_unknown_ones_are_ignored() {
        assert!(matches!(
            parse_frame(r#"{"t":"state","groups":[{"groupId":"g","chain":[]}]}"#),
            Some(Frame::State(groups)) if groups.len() == 1 && groups[0].node_infos.is_empty()
        ));
        assert!(matches!(
            parse_frame(r#"{"t":"observed","url":"http://8.8.8.8:7345"}"#),
            Some(Frame::Observed(Some(url))) if url == "http://8.8.8.8:7345"
        ));
        assert!(matches!(
            parse_frame(&observed_frame(None)),
            Some(Frame::Observed(None))
        ));
        assert!(parse_frame(r#"{"t":"open","id":"x"}"#).is_none());
        assert!(parse_frame("not json").is_none());
    }

    #[test]
    fn observed_addresses_are_public_ips_with_the_announced_ports() {
        let urls = [
            "http://192.168.1.20:7345".to_owned(),
            "http://10.0.0.2:7345".to_owned(),
            "https://nas.example.net".to_owned(),
            "http://[fd00::2]:8000".to_owned(),
            "http://10.0.0.2:9000".to_owned(),
        ];
        assert_eq!(
            observed_candidates("8.8.4.4".parse().unwrap(), &urls),
            vec![
                "http://8.8.4.4:7345",
                "http://8.8.4.4:443",
                "http://8.8.4.4:8000"
            ]
        );
        assert_eq!(
            observed_candidates("::ffff:8.8.4.4".parse().unwrap(), &urls[..1]),
            vec!["http://8.8.4.4:7345"]
        );
        assert_eq!(
            observed_candidates("2606:4700::1".parse().unwrap(), &urls[..1]),
            vec!["http://[2606:4700::1]:7345"]
        );
        for private in [
            "192.168.1.9",
            "10.1.2.3",
            "172.16.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "203.0.113.5",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(
                observed_candidates(private.parse().unwrap(), &urls).is_empty(),
                "{private}"
            );
        }
        assert!(observed_candidates("8.8.4.4".parse().unwrap(), &[]).is_empty());
    }

    #[test]
    fn nodes_take_only_plain_public_observed_addresses() {
        assert!(acceptable_observed("http://8.8.4.4:7345"));
        assert!(acceptable_observed("http://[2606:4700::1]:7345"));
        assert!(acceptable_observed("http://8.8.4.4:80"));
        for refused in [
            "http://192.168.1.2:7345",
            "http://127.0.0.1:7345",
            "https://8.8.4.4:7345",
            "http://8.8.4.4:7345/v2/mesh/relay/x/y",
            "http://example.com:7345",
            "ftp://8.8.4.4:21",
        ] {
            assert!(!acceptable_observed(refused), "{refused}");
        }
    }
}
