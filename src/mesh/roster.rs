//! Signed documents and roster chains (docs/mesh-v1.md "Signed documents").

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::error::AppError;

type Result<T> = std::result::Result<T, AppError>;

pub(crate) const ROSTER_DOMAIN: &[u8] = b"todex.mesh.v1/roster\0";
pub(crate) const NODE_INFO_DOMAIN: &[u8] = b"todex.mesh.v1/node-info\0";
pub(crate) const HISTORY_KEY_DOMAIN: &[u8] = b"todex.mesh.v1/history-key\0";
pub(crate) const ROSTER_TYPE: &str = "todex.mesh.roster";
pub(crate) const NODE_INFO_TYPE: &str = "todex.mesh.node-info";
pub(crate) const RECOVERY_SIGNER: &str = "recovery";
pub(crate) const MAX_BODY_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CHAIN: usize = 4096;
pub(crate) const MAX_MEMBERS: usize = 256;
/// X-Wing (ML-KEM-768 + X25519) public key.
pub(crate) const HISTORY_KEY_BYTES: usize = 1216;

/// `{"body": b64u(json bytes), "sig": b64u(signature)}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SignedDoc {
    pub body: String,
    pub sig: String,
}

impl SignedDoc {
    pub(crate) fn sign(domain: &[u8], body: &[u8], key: &SigningKey) -> Self {
        let signature = key.sign(&[domain, body].concat());
        Self {
            body: URL_SAFE_NO_PAD.encode(body),
            sig: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        }
    }

    pub(crate) fn body_bytes(&self) -> Result<Vec<u8>> {
        if self.body.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
            return Err(invalid("mesh document is too large"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.body)
            .map_err(|_| invalid("mesh document body is not base64url"))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(invalid("mesh document is too large"));
        }
        Ok(bytes)
    }

    pub(crate) fn verify(&self, domain: &[u8], body: &[u8], key: &VerifyingKey) -> Result<()> {
        let signature = URL_SAFE_NO_PAD
            .decode(&self.sig)
            .ok()
            .and_then(|bytes| Signature::from_slice(&bytes).ok())
            .ok_or_else(|| invalid("mesh document signature is malformed"))?;
        key.verify_strict(&[domain, body].concat(), &signature)
            .map_err(|_| invalid("mesh document signature does not verify"))
    }
}

pub(crate) fn hash(body: &[u8]) -> [u8; 32] {
    Sha256::digest(body).into()
}

/// `prefix` + b64u(SHA256(key)[0..12]): `dev_` for devices, `node_` for
/// nodes (the device form equals [`crate::devices::device_id_for`]).
pub(crate) fn member_id(prefix: &str, public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(&digest[..12]))
}

pub(crate) fn group_id_for(genesis_body: &[u8]) -> String {
    format!("grp_{}", URL_SAFE_NO_PAD.encode(&hash(genesis_body)[..16]))
}

/// Strictly usable Ed25519 key: canonical encoding, not small order.
pub(crate) fn parse_signing_public_key(value: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid("invalid Ed25519 public key"))?;
    if URL_SAFE_NO_PAD.encode(bytes) != value {
        return Err(invalid("invalid Ed25519 public key"));
    }
    let key =
        VerifyingKey::from_bytes(&bytes).map_err(|_| invalid("invalid Ed25519 public key"))?;
    if key.is_weak() {
        return Err(invalid("Ed25519 public key is weak"));
    }
    Ok(key)
}

pub(crate) fn parse_history_key(value: &str) -> Result<Vec<u8>> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| invalid("invalid history key"))?;
    if bytes.len() != HISTORY_KEY_BYTES || URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(invalid("invalid history key"));
    }
    crate::history_crypto::RecipientPublicKey::from_bytes(&bytes)
        .map_err(|_| invalid("invalid history key"))?;
    Ok(bytes)
}

/// `"todex.mesh.v1/history-key\0" || groupId || "\0" || historyKey`.
pub(crate) fn history_key_binding(group_id: &str, history_key: &[u8]) -> Vec<u8> {
    [HISTORY_KEY_DOMAIN, group_id.as_bytes(), b"\0", history_key].concat()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MemberKind {
    Device,
    Node,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Member {
    pub id: String,
    pub kind: MemberKind,
    pub name: String,
    pub public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_key: Option<String>,
    pub added_at: u64,
    pub added_by: String,
    /// Nodes: addresses a user pinned (docs/mesh-v1.md "Addresses").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// Nodes: whether this node relays for the group (`allowed` when
    /// absent); see "Relay".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayRole>,
    /// Nodes: `false` when no member may reach this node through a relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_target: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RelayRole {
    Preferred,
    Allowed,
    Denied,
}

impl Member {
    /// This node relays for its group.
    pub(crate) fn relays(&self) -> bool {
        self.kind == MemberKind::Node && self.relay != Some(RelayRole::Denied)
    }

    /// Members may reach this node through a relay.
    pub(crate) fn reachable_by_relay(&self) -> bool {
        self.kind == MemberKind::Node && self.relay_target != Some(false)
    }
}

/// Longest pinned address list of a node.
pub(crate) const MAX_PINNED_ADDRESSES: usize = 16;

/// An `http(s)://host[:port][/path]` address of at most 512 bytes.
pub(crate) fn valid_address(value: &str) -> bool {
    value.len() <= 512
        && (value.starts_with("http://") || value.starts_with("https://"))
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Recovery {
    pub signing_key: String,
    pub history_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RosterBody {
    #[serde(rename = "type")]
    pub doc_type: String,
    pub v: u32,
    pub group_id: Option<String>,
    pub version: u64,
    pub prev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    pub name: String,
    pub signer: String,
    pub signed_at: u64,
    pub recovery: Option<Recovery>,
    pub members: Vec<Member>,
}

impl RosterBody {
    pub(crate) fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.id == id)
    }

    pub(crate) fn devices(&self) -> impl Iterator<Item = &Member> {
        self.members
            .iter()
            .filter(|member| member.kind == MemberKind::Device)
    }

    /// The key that may sign the document after this one as `signer`.
    fn signer_key(&self, signer: &str) -> Result<VerifyingKey> {
        if signer == RECOVERY_SIGNER {
            let recovery = self
                .recovery
                .as_ref()
                .ok_or_else(|| invalid("roster has no recovery key"))?;
            return parse_signing_public_key(&recovery.signing_key);
        }
        let member = self
            .member(signer)
            .filter(|member| member.kind == MemberKind::Device)
            .ok_or_else(|| invalid("roster signer is not a device of the group"))?;
        parse_signing_public_key(&member.public_key)
    }

    /// Rule 3 of "Roster": members, ids, keys and names.
    fn validate_members(&self) -> Result<()> {
        if self.members.is_empty() || self.members.len() > MAX_MEMBERS {
            return Err(invalid("roster must list 1-256 members"));
        }
        crate::device_pairing::validate_device_name(&self.name)
            .map_err(|_| invalid("invalid group name"))?;
        let mut ids = HashSet::new();
        for member in &self.members {
            if !ids.insert(member.id.as_str()) {
                return Err(invalid("roster lists a member twice"));
            }
            let key = parse_signing_public_key(&member.public_key)?;
            let prefix = match member.kind {
                MemberKind::Device => "dev_",
                MemberKind::Node => "node_",
            };
            if member.id != member_id(prefix, key.as_bytes()) {
                return Err(invalid("roster member id does not match its key"));
            }
            match (&member.kind, &member.history_key) {
                (MemberKind::Device, Some(history_key)) => {
                    parse_history_key(history_key)?;
                }
                (MemberKind::Device, None) => {
                    return Err(invalid("roster device lacks a history key"));
                }
                (MemberKind::Node, Some(_)) => {
                    return Err(invalid("roster node carries a history key"));
                }
                (MemberKind::Node, None) => {}
            }
            crate::device_pairing::validate_device_name(&member.name)
                .map_err(|_| invalid("invalid member name"))?;
            if member.kind == MemberKind::Device
                && (!member.addresses.is_empty()
                    || member.relay.is_some()
                    || member.relay_target.is_some())
            {
                return Err(invalid("only nodes carry addresses and relay settings"));
            }
            if member.addresses.len() > MAX_PINNED_ADDRESSES
                || !member
                    .addresses
                    .iter()
                    .all(|address| valid_address(address))
            {
                return Err(invalid("invalid pinned node address"));
            }
        }
        if self.devices().next().is_none() {
            return Err(invalid("roster must list at least one device"));
        }
        if let Some(recovery) = &self.recovery {
            parse_signing_public_key(&recovery.signing_key)?;
            parse_history_key(&recovery.history_key)?;
        }
        Ok(())
    }
}

/// A chain that passed every rule of "Roster".
#[derive(Clone, Debug)]
pub(crate) struct VerifiedChain {
    pub group_id: String,
    pub docs: Vec<SignedDoc>,
    pub bodies: Vec<RosterBody>,
    pub head_hash: [u8; 32],
}

impl VerifiedChain {
    pub(crate) fn head(&self) -> &RosterBody {
        self.bodies.last().expect("a verified chain is never empty")
    }

    pub(crate) fn version(&self) -> u64 {
        self.head().version
    }

    pub(crate) fn head_hash_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.head_hash)
    }

    /// The fork rule: longer wins, then the smaller head hash.
    pub(crate) fn wins_over(&self, other: &VerifiedChain) -> bool {
        (self.docs.len(), std::cmp::Reverse(self.head_hash))
            > (other.docs.len(), std::cmp::Reverse(other.head_hash))
    }
}

pub(crate) fn verify_chain(docs: &[SignedDoc]) -> Result<VerifiedChain> {
    if docs.is_empty() || docs.len() > MAX_CHAIN {
        return Err(invalid("roster chain must hold 1-4096 documents"));
    }
    let mut bodies: Vec<RosterBody> = Vec::with_capacity(docs.len());
    let mut group_id = String::new();
    let mut previous_hash = [0u8; 32];
    for (index, doc) in docs.iter().enumerate() {
        let bytes = doc.body_bytes()?;
        let body: RosterBody =
            serde_json::from_slice(&bytes).map_err(|_| invalid("malformed roster document"))?;
        if body.doc_type != ROSTER_TYPE || body.v != 1 {
            return Err(invalid("not a mesh v1 roster document"));
        }
        if body.version != index as u64 + 1 {
            return Err(invalid("roster versions are not consecutive"));
        }
        body.validate_members()?;
        let key = match bodies.last() {
            None => {
                if body.group_id.is_some() || body.prev.is_some() {
                    return Err(invalid(
                        "genesis roster must not name a group or predecessor",
                    ));
                }
                let nonce = body
                    .nonce
                    .as_deref()
                    .and_then(|nonce| URL_SAFE_NO_PAD.decode(nonce).ok())
                    .filter(|nonce| nonce.len() == 16);
                if nonce.is_none() {
                    return Err(invalid("genesis roster needs a 16-byte nonce"));
                }
                group_id = group_id_for(&bytes);
                if body.signer == RECOVERY_SIGNER {
                    return Err(invalid("genesis roster must be signed by a device"));
                }
                body.signer_key(&body.signer)?
            }
            Some(previous) => {
                if body.group_id.as_deref() != Some(group_id.as_str()) {
                    return Err(invalid("roster document belongs to another group"));
                }
                if body.prev.as_deref() != Some(URL_SAFE_NO_PAD.encode(previous_hash).as_str()) {
                    return Err(invalid("roster document does not follow its predecessor"));
                }
                previous.signer_key(&body.signer)?
            }
        };
        doc.verify(ROSTER_DOMAIN, &bytes, &key)?;
        previous_hash = hash(&bytes);
        bodies.push(body);
    }
    Ok(VerifiedChain {
        group_id,
        docs: docs.to_vec(),
        bodies,
        head_hash: previous_hash,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeInfoBody {
    #[serde(rename = "type")]
    pub doc_type: String,
    pub v: u32,
    pub node_id: String,
    pub public_key: String,
    pub seq: u64,
    pub name: String,
    pub transport_protocol: String,
    pub transport_public_key: String,
    pub urls: Vec<String>,
    pub signed_at: u64,
}

pub(crate) const MAX_NODE_URLS: usize = 16;

/// Verifies a node info document against the node's roster key.
pub(crate) fn verify_node_info(doc: &SignedDoc, expected_key: &str) -> Result<NodeInfoBody> {
    let bytes = doc.body_bytes()?;
    let body: NodeInfoBody =
        serde_json::from_slice(&bytes).map_err(|_| invalid("malformed node info"))?;
    if body.doc_type != NODE_INFO_TYPE || body.v != 1 {
        return Err(invalid("not a mesh v1 node info document"));
    }
    if body.public_key != expected_key || body.urls.len() > MAX_NODE_URLS {
        return Err(invalid("node info does not match the roster"));
    }
    let key = parse_signing_public_key(&body.public_key)?;
    if body.node_id != member_id("node_", key.as_bytes()) {
        return Err(invalid("node info id does not match its key"));
    }
    doc.verify(NODE_INFO_DOMAIN, &bytes, &key)?;
    Ok(body)
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn device_key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    pub(crate) fn history_key(byte: u8) -> String {
        let key = crate::history_crypto::recipient_from_seed(&[byte; 32]);
        URL_SAFE_NO_PAD.encode(key.to_bytes())
    }

    pub(crate) fn device_member(key: &SigningKey, byte: u8, added_by: &str) -> Member {
        Member {
            id: member_id("dev_", key.verifying_key().as_bytes()),
            kind: MemberKind::Device,
            name: format!("device {byte}"),
            public_key: URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            history_key: Some(history_key(byte)),
            added_at: 1,
            added_by: added_by.to_owned(),
            addresses: Vec::new(),
            relay: None,
            relay_target: None,
        }
    }

    pub(crate) fn node_member(key: &VerifyingKey, added_by: &str) -> Member {
        Member {
            id: member_id("node_", key.as_bytes()),
            kind: MemberKind::Node,
            name: "node".to_owned(),
            public_key: URL_SAFE_NO_PAD.encode(key.as_bytes()),
            history_key: None,
            added_at: 1,
            added_by: added_by.to_owned(),
            addresses: Vec::new(),
            relay: None,
            relay_target: None,
        }
    }

    /// A genesis roster listing only the device `creator`.
    pub(crate) fn genesis(creator: &SigningKey, byte: u8) -> Vec<SignedDoc> {
        let member = device_member(creator, byte, "");
        let body = RosterBody {
            doc_type: ROSTER_TYPE.to_owned(),
            v: 1,
            group_id: None,
            version: 1,
            prev: None,
            nonce: Some(URL_SAFE_NO_PAD.encode([byte; 16])),
            name: "Home".to_owned(),
            signer: member.id.clone(),
            signed_at: 1,
            recovery: None,
            members: vec![Member {
                added_by: member.id.clone(),
                ..member
            }],
        };
        vec![SignedDoc::sign(
            ROSTER_DOMAIN,
            &serde_json::to_vec(&body).unwrap(),
            creator,
        )]
    }

    /// Appends a version with `members`, signed by `signer`.
    pub(crate) fn append(
        chain: &[SignedDoc],
        signer: &SigningKey,
        members: Vec<Member>,
    ) -> Vec<SignedDoc> {
        let verified = verify_chain(chain).unwrap();
        let head = verified.head();
        let body = RosterBody {
            doc_type: ROSTER_TYPE.to_owned(),
            v: 1,
            group_id: Some(verified.group_id.clone()),
            version: head.version + 1,
            prev: Some(verified.head_hash_b64()),
            nonce: None,
            name: head.name.clone(),
            signer: member_id("dev_", signer.verifying_key().as_bytes()),
            signed_at: head.signed_at + 1,
            recovery: head.recovery.clone(),
            members,
        };
        let mut next = chain.to_vec();
        next.push(SignedDoc::sign(
            ROSTER_DOMAIN,
            &serde_json::to_vec(&body).unwrap(),
            signer,
        ));
        next
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    /// tests/fixtures/mesh-v1.json, generated by TodeX_protocol
    /// (scripts/generate-mesh-v1-vectors.cjs): the TypeScript and Rust
    /// implementations agree on every format.
    #[test]
    fn cross_implementation_vectors() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/mesh-v1.json")).unwrap();
        let docs: Vec<SignedDoc> = serde_json::from_value(fixture["chain"].clone()).unwrap();
        let chain = verify_chain(&docs).unwrap();
        assert_eq!(chain.group_id, fixture["groupId"].as_str().unwrap());
        for (index, expected) in fixture["versions"].as_array().unwrap().iter().enumerate() {
            let prefix = verify_chain(&docs[..=index]).unwrap();
            assert_eq!(prefix.head_hash_b64(), expected["head"].as_str().unwrap());
            let ids: Vec<&str> = prefix
                .head()
                .members
                .iter()
                .map(|m| m.id.as_str())
                .collect();
            let expected: Vec<&str> = expected["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect();
            assert_eq!(ids, expected);
        }
        assert_eq!(chain.head().signer, RECOVERY_SIGNER);
        let node = chain
            .head()
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Node)
            .unwrap();
        assert_eq!(
            node.addresses,
            vec!["https://nas.example.net:7345".to_owned()]
        );
        assert_eq!(node.relay, Some(RelayRole::Preferred));
        assert!(node.relays() && !node.reachable_by_relay());
        let ticket = &fixture["relayTicket"];
        let parts: Vec<&str> = ticket["ticket"].as_str().unwrap().splitn(3, '.').collect();
        let device = parse_signing_public_key(ticket["devicePublicKey"].as_str().unwrap()).unwrap();
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        device
            .verify_strict(
                &crate::server::mesh_relay_ticket_message(
                    ticket["deviceId"].as_str().unwrap(),
                    ticket["relayNodeId"].as_str().unwrap(),
                    ticket["targetNodeId"].as_str().unwrap(),
                    ticket["expiresAt"].as_u64().unwrap(),
                ),
                &signature,
            )
            .unwrap();
        for (name, invalid) in fixture["invalidChains"].as_object().unwrap() {
            let docs: Vec<SignedDoc> = serde_json::from_value(invalid.clone()).unwrap();
            assert!(verify_chain(&docs).is_err(), "{name}");
        }
        let info: SignedDoc = serde_json::from_value(fixture["nodeInfo"]["doc"].clone()).unwrap();
        let node_key = fixture["nodeInfo"]["publicKey"].as_str().unwrap();
        let body = verify_node_info(&info, node_key).unwrap();
        assert_eq!(body.node_id, fixture["memberIds"]["node"].as_str().unwrap());
        let binding = &fixture["historyKeyBinding"];
        let device =
            parse_signing_public_key(binding["devicePublicKey"].as_str().unwrap()).unwrap();
        let history_key = parse_history_key(binding["historyKey"].as_str().unwrap()).unwrap();
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(binding["signature"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        device
            .verify_strict(
                &history_key_binding(binding["groupId"].as_str().unwrap(), &history_key),
                &signature,
            )
            .unwrap();
        let code = &fixture["joinCode"];
        assert_eq!(
            crate::mesh::normalize_join_code(code["code"].as_str().unwrap()),
            code["normalized"].as_str().unwrap()
        );
        let key = crate::mesh::join_code_key(code["code"].as_str().unwrap());
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(key.as_ref()), code["codeKey"].as_str().unwrap());
        let transcript_hash: [u8; 32] = (0..32)
            .map(|i| {
                u8::from_str_radix(
                    &code["transcriptHash"].as_str().unwrap()[i * 2..i * 2 + 2],
                    16,
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let node_public: [u8; 32] = URL_SAFE_NO_PAD
            .decode(code["nodePublicKey"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            crate::mesh::node_join_server_proof(&key, &transcript_hash, &node_public),
            code["serverProof"].as_str().unwrap()
        );
        let client_proof = URL_SAFE_NO_PAD
            .decode(code["clientProof"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            client_proof,
            crate::mesh::client_proof_for_tests(&key, &transcript_hash)
        );
    }

    #[test]
    fn a_genesis_chain_verifies_and_names_its_group() {
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let verified = verify_chain(&chain).unwrap();
        assert!(verified.group_id.starts_with("grp_"));
        assert_eq!(verified.version(), 1);
        assert_eq!(verified.head().devices().count(), 1);
    }

    #[test]
    fn only_members_of_the_previous_version_may_sign() {
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let head = verify_chain(&chain).unwrap().head().clone();
        let stranger = device_key(2);
        let mut members = head.members.clone();
        members.push(device_member(&stranger, 2, "x"));
        // The stranger lists itself and signs: refused.
        let forged = append(&chain, &stranger, members.clone());
        assert!(verify_chain(&forged).is_err());
        // The creator adds it: accepted, and then the newcomer may sign.
        let added = append(&chain, &creator, members);
        let verified = verify_chain(&added).unwrap();
        assert_eq!(verified.version(), 2);
        let without_creator = verified
            .head()
            .members
            .iter()
            .filter(|member| member.id != head.members[0].id)
            .cloned()
            .collect();
        let removed = append(&added, &stranger, without_creator);
        let verified = verify_chain(&removed).unwrap();
        // The removed creator cannot sign a later version.
        let comeback = append(&removed, &creator, verified.head().members.clone());
        assert!(verify_chain(&comeback).is_err());
    }

    #[test]
    fn tampering_with_any_document_breaks_the_chain() {
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let members = verify_chain(&chain).unwrap().head().members.clone();
        let mut chain = append(&chain, &creator, members);
        chain[0].sig = chain[1].sig.clone();
        assert!(verify_chain(&chain).is_err());
    }

    #[test]
    fn rosters_without_a_device_or_with_mismatched_ids_are_refused() {
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let node = device_key(9).verifying_key();
        let only_node = append(&chain, &creator, vec![node_member(&node, "x")]);
        assert!(verify_chain(&only_node).is_err());
        let mut wrong = device_member(&device_key(3), 3, "x");
        wrong.id = "dev_AAAAAAAAAAAAAAAA".to_owned();
        let head = verify_chain(&chain).unwrap().head().members.clone();
        let mismatched = append(&chain, &creator, [head, vec![wrong]].concat());
        assert!(verify_chain(&mismatched).is_err());
    }

    #[test]
    fn the_longer_chain_then_the_smaller_head_wins() {
        let creator = device_key(1);
        let chain = genesis(&creator, 1);
        let members = verify_chain(&chain).unwrap().head().members.clone();
        let a = verify_chain(&append(&chain, &creator, members.clone())).unwrap();
        let mut renamed = members.clone();
        renamed[0].name = "renamed".to_owned();
        let b = verify_chain(&append(&chain, &creator, renamed)).unwrap();
        assert_ne!(a.wins_over(&b), b.wins_over(&a));
        let short = verify_chain(&chain).unwrap();
        assert!(a.wins_over(&short) && !short.wins_over(&a));
    }
}
