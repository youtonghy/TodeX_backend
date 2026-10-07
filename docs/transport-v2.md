# TodeX transport v2

Status: normative. Implemented by `TodeX_backend` (`src/transport_crypto/`,
`src/server/ws/`, `src/server/sealed.rs`), `TodeX_protocol` (TypeScript,
`secureChannel.ts` / `secureTransport.ts`, used by desktop and web) and
`Todex_mobile` (Swift). All three pass the shared vectors in
`tests/fixtures/transport-v2.json` (a verbatim copy of the file
`TodeX_protocol/scripts/generate-transport-v2-vectors.cjs` generates; do not
edit it by hand).

Transport v2 replaces the `todex.crypto.v1` WebSocket frames and adds
encryption for REST.

## Goals

- REST and WebSocket share one key schedule and one AEAD construction.
- Every session key mixes in fresh server randomness, so replaying a
  handshake never reproduces a key or a nonce, even across daemon restarts.
- Non-loopback peers can only talk to the API through v2. Loopback peers may
  still use plaintext.
- Encryption is invisible to business code: handlers and UI see plain
  requests, responses and JSON messages.
- Historical E2E encryption is unaffected: transport never sees history keys.

## Primitives

- AEAD: XChaCha20-Poly1305 (24-byte nonce, 16-byte tag).
- KDF: HKDF-SHA256. Hash: SHA-256.
- Key agreement, unchanged from v1 and chosen by the server's
  `pairing_encryption` (`x25519` or `ml-kem-768`):
  - `x25519`: the client makes a fresh ephemeral key pair per handshake.
    `shared = X25519(client_secret, server_static_public)`; reject an all-zero
    result. `client_material = client_public` (32 bytes).
  - `ml-kem-768`: `(ciphertext, shared) = Encapsulate(server_static_public)`.
    `client_material = ciphertext` (1088 bytes).
- `server_static_public` is the public key pinned at pairing (the same bytes
  as the v1 pairing public key for that protocol).
- base64url without padding for every value in a header, query string or
  JSON field.
- `LP(x) = u32_be(len(x)) || x`. `u64_be(n)` is an 8-byte big-endian integer.
- Every key, shared secret and transcript is zeroized when dropped.

## Key schedule (shared by WebSocket and REST)

```
th   = SHA256(LP(label) || LP(protocol) || LP(device_id) ||
              LP(server_static_public) || LP(client_material) ||
              LP(client_nonce) || LP(server_nonce))
prk  = HKDF-Extract(salt = th, ikm = shared)
k_up   = HKDF-Expand(prk, label || "/up",   32)   // client -> server
k_down = HKDF-Expand(prk, label || "/down", 32)   // server -> client
```

- `protocol` is the ASCII protocol id (`x25519` | `ml-kem-768`).
- WebSocket: `label = "todex.transport.v2/ws"`, `device_id` = the device id in
  the signed upgrade credential (empty string when auth is disabled),
  `client_nonce` = 32 random bytes from the client, `server_nonce` = 32 random
  bytes from the server.
- REST: `label = "todex.transport.v2/rest"`, `device_id` = empty string (the
  inner request carries its own signature), `client_nonce` = 32 random bytes
  per request, `server_nonce` = empty (zero-length). Freshness comes from the
  per-request client material and the inner signature's single-use nonce.

## Sealed records

A message or stream is a sequence of records. Record `i` (starting at 0 per
direction and per key) is:

```
nonce_i = 16 zero bytes || u64_be(i)
aad_i   = th || direction || final
```

- `direction` is one byte: `0x02` up (client → server), `0x01` down.
- `final` is one byte: `0x01` for the last record of a REST stream, `0x00`
  otherwise. WebSocket frames always use `0x00`.
- Receivers require `i` to equal the next expected counter exactly; any
  mismatch, authentication failure, or exhausted counter is fatal for the
  connection/request.

## WebSocket

1. The client opens `/v2/ws` with the usual signed query plus
   `tv=2`, `enc=<protocol>`, `client_nonce=<b64url 32B>` and either
   `client_key=<b64url>` (x25519) or `ciphertext=<b64url>` (ml-kem-768).
2. After the upgrade the server sends exactly one **text** message:
   `{"type":"todex.transport.hello","version":2,"serverNonce":"<b64url 32B>"}`.
   The client must not send anything before it arrives.
3. Every later message in both directions is a **binary** message:
   `u64_be(i) || ciphertext`, where `ciphertext = AEAD(k, nonce_i, aad_i,
   utf8(json_message))`. The JSON messages are exactly the v1 plaintext
   protocol messages.
4. On any crypto failure the receiver closes with code `4400` and reason
   `transport crypto failure`. No internal detail is sent.
5. A text message after the hello, an unexpected `tv`, or a v1 `enc=` upgrade
   without `tv=2` is rejected: before upgrade with HTTP 426 and the error code
   `PROTOCOL_UPGRADE_REQUIRED`.

## REST tunnel

`POST /v2/sealed` carries one complete inner HTTP request.

Outer request headers:

- `Content-Type: application/vnd.todex.sealed`
- `X-Todex-Transport: 2`
- `X-Todex-Encryption: <protocol>`
- `X-Todex-Client-Key` (x25519) or `X-Todex-Kem-Ciphertext` (ml-kem-768)
- `X-Todex-Request-Nonce: <b64url 32B>`

Outer body: a record stream sealed with `k_up`. On the wire each record is
`u32_be(len(ciphertext)) || ciphertext`. Each record's plaintext is at most
65536 bytes. There is at least one record. Exactly the last record has
`final = 0x01`, and no bytes may follow it.

The concatenated plaintext is the inner request:

```
u32_be(len(head)) || head || body
head = UTF-8 JSON {"method": "GET", "path": "/v2/...", "query": "a=b" (optional,
       raw, without "?"), "headers": {"<lowercase name>": "<value>", ...}}
```

- `path` must start with `/` and must not be `/v2/sealed` (no nesting).
- `headers` carries `content-type`, `accept` and the device-auth credential
  headers. The server drops every other header, including hop-by-hop headers
  and any `x-todex-transport*` header.
- The inner request then runs through the normal router and middleware,
  including device auth (which signs the inner method, path, query and body)
  and per-route body limits.

Outer response: status `200`, `Content-Type: application/vnd.todex.sealed`,
body is a record stream sealed with `k_down` whose plaintext is:

```
u32_be(len(head)) || head || body
head = UTF-8 JSON {"status": 200, "headers": {"content-type": "...", ...}}
```

The server streams records as the inner body is produced. When the outer
request itself cannot be opened (bad header, bad material, AEAD failure,
truncation), the server answers a plain `400` with
`{"error":{"code":"TRANSPORT_CRYPTO_FAILED","message":"transport crypto failure"}}`
and no further detail.

## Enforcement

> Backend status: `/v2/transport-policy` already advertises
> `transportVersion: 2`; the route restrictions, the startup check and the
> removal of transport v1 land once every client has migrated.

- `/v2/transport-policy` adds `"transportVersion": 2`.
- A non-loopback listener with `pairing_encryption = "none"` is a startup
  error. Loopback listeners may keep `none`.
- For a non-loopback peer, only these routes are reachable directly:
  `/health`, `/v2/transport-policy`, `/v2/version`, `/v2/device-pairing/*`,
  `/v2/sealed`, and `/v2/ws` with `tv=2`. Everything else answers `426`
  `PROTOCOL_UPGRADE_REQUIRED`. Inner requests from the tunnel are treated as
  arriving through v2.
- Loopback peers may use plaintext WebSocket and direct REST, and may also use
  v2.

## Client rules

- If the profile pins an encryption protocol and key, the client always uses
  v2 for the WebSocket and the tunnel for every REST call, including loopback.
- If the profile has no pinned key and the host is not loopback, the client
  refuses to connect and asks the user to pair with encryption. It never
  falls back to plaintext because of a policy answer.
- If the server policy requires a different protocol than the pinned one, the
  client shows a re-pair error.
- Clients pick the size limit on the plaintext before sealing; a frame that
  is too large is rejected without consuming a counter.

## Device pairing v3 (commit, then reveal)

v2 let a man in the middle grind its own public key against a 40-bit code.
v3 makes the client commit first.

```
commit     = SHA256(LP("todex.device-pairing.v3/commit") || client_public || client_nonce)
transcript = "todex.device-pairing.v3/transcript\0" || request_id || 0x00 ||
             client_public || server_public || 0x00 || device_public || client_nonce
```

1. `POST /v2/device-pairing/create`
   `{clientCommitment, deviceName, devicePublicKey}` →
   `{requestId, serverPublicKey, expiresAt, pollIntervalMs}`.
   A body with `clientPublicKey` (v2) answers `426 PROTOCOL_UPGRADE_REQUIRED`.
2. `POST /v2/device-pairing/reveal` `{requestId, clientPublicKey, clientNonce}`.
   The server checks the commitment in constant time, derives the pairing
   material from the v3 transcript (same HKDF salt/info pattern as v2 with v3
   labels: `todex.device-pairing.v3/wrap-key`, `/poll-proof`, `/cancel-proof`),
   and only then shows the verification code. Answers `{"status":"pending"}`.
   A second reveal for the same request fails.
3. `poll` and `cancel` are unchanged apart from using v3 material.

The verification code is still the first 5 bytes of `SHA256(transcript)`,
formatted `XXXXX-XXXXX`.

## Test vectors

`TodeX_protocol/tests/fixtures/transport-v2.json` contains, for each
protocol: the server static key pair, the client material (and, for
ml-kem-768, the ciphertext with its expected shared secret, because not every
library supports deterministic encapsulation), nonces, `th`, `k_up`, `k_down`,
WebSocket frames for a few messages in both directions, a REST request and
response stream including a multi-record case, and failure cases (wrong
counter, flipped tag bit, truncated stream, extra trailing bytes). It also
contains a pairing v3 vector (commitment, transcript hash, code and the three
derived keys).

## Clarifications (normative)

1. Counters: the value `2^64 - 1` is never used. A sender or receiver that
   would need it treats the counter as exhausted (fatal).
2. Final record detection: a streaming receiver cannot know whether more bytes
   follow, so it tries to open each record with `final = 0x00` and, if that
   fails, with `final = 0x01`. The counter only advances on success. Any byte
   after a final record is fatal; end of stream without a final record is
   truncation.
3. The inner `head` JSON is at most 65536 bytes.
4. An empty plaintext REST stream is one empty final record (in practice the
   inner message is always at least 4 bytes).
5. The head JSON byte form is not normative. Vectors give the full plaintext;
   implementations seal that plaintext and compare. They do not re-serialize.
6. Pairing v3 `client_nonce` is 32 bytes. HKDF is as in v2:
   `salt = SHA256(transcript)`, `ikm = X25519 shared`, `info` = the v3 labels.
7. Hello: unknown fields are ignored. `type`, `version = 2` and a 32-byte
   `serverNonce` must match exactly.
8. WebSocket size limits apply to the binary frame; the plaintext limit is the
   frame limit minus 24 bytes (8-byte counter + 16-byte tag).
9. The `device_id` in the key schedule is the one in the signed upgrade
   credential; a client whose signer has a different id refuses to connect.
10. A tunnel response that is not `200` with the sealed content type is not
    authenticated and is reported as a plain API error (for example `400
    TRANSPORT_CRYPTO_FAILED` or `426 PROTOCOL_UPGRADE_REQUIRED`).

## Backend implementation notes

- Module layout: `transport_crypto::handshake` (key agreement and key
  schedule), `transport_crypto::channel` (`RecordCipher`, WebSocket frames),
  `transport_crypto::envelope` (REST record streams, inner head),
  `server::ws` (handshake negotiation, the plaintext / v1 / v2 frame codecs
  and the socket loops) and `server::sealed` (the tunnel). Business handlers
  only see JSON text and plain HTTP requests.
- Keys live inside the AEAD (wiped on drop); transcripts, shared secrets and
  derived keys are `Zeroizing`.
- WebSocket: protocol problems answer before the upgrade (`tv` other than
  `2` is `426 PROTOCOL_UPGRADE_REQUIRED`; a protocol other than the server's
  `pairing_encryption` is `403`). Malformed handshake material (bad base64,
  wrong length, an all-zero X25519 result) is a crypto failure: the server
  upgrades and closes with `4400` without sending the hello. Sealing or
  opening failures, a text frame after the hello and counter exhaustion all
  close with `4400 transport crypto failure`. The frame limit is 8 MiB, so
  the plaintext limit is 8 MiB − 24 bytes.
- REST tunnel:
  - The outer body limit is the record-stream size of a 32 MiB inner body
    plus a 64 KiB head (`4 + 65536 + 32 MiB` plaintext plus 20 bytes per
    record). Outer failures, including an oversized body or a protocol other
    than the server's `pairing_encryption`, answer `400` with the usual error
    envelope `{"code":"TRANSPORT_CRYPTO_FAILED","message":"transport crypto failure"}`.
  - Inner headers kept: `content-type`, `accept`, `x-todex-device-id`,
    `x-todex-auth-ts`, `x-todex-auth-nonce`, `x-todex-auth-sig` (names are
    matched case-insensitively). `Host` and `Origin` are taken from the outer
    request, because anonymous (`enable_auth = false`) deployments check them.
    The peer address (`ConnectInfo`) carries over; no other outer request
    extension does.
  - Every inner request carries the server-only `ArrivedViaTransportV2`
    request extension, which the enforcement layer treats as "arrived through
    v2".
  - The inner request runs through the router without the tunnel route, so
    nesting cannot be routed even apart from the path check.
  - Inner response headers are copied except hop-by-hop headers; repeated
    headers are joined with `, `. Records follow the inner body's chunks (the
    latest chunk is held back until the next one or the end, so exactly the
    last record is final). If the inner body fails mid-stream the outer body
    ends with an error and no final record, which clients report as
    truncation. The tunnel response is never gzip-compressed.
- Device pairing v3 is implemented in `src/device_pairing.rs`; the
  approval credential wrap is unchanged from v2 (XChaCha20-Poly1305 under the
  wrap key, AAD = the full v3 transcript). The verification code reaches the
  local approver (TUI) only after `reveal`. A reveal that does not open the
  commitment discards the request; a second reveal answers `409 CONFLICT`.
  `poll` and `cancel` before `reveal` answer `401`.

## Transport v1 during the migration

Until the enforcement step lands, `/v2/ws` still accepts the v1
`enc=<protocol>&client_key|ciphertext=...` upgrade without `tv`. Its keys
depend only on the client material, so the server remembers used handshakes
for twice the device-auth clock window (600 s) and refuses a repeat; entries
age out, and a full registry (65,536 entries) fails new handshakes with
`RESOURCE_EXHAUSTED` until entries expire instead of permanently. A v1 frame
that does not decrypt closes the connection with `4400`.
