# TodeX Backend (`todex-agentd`)

<p align="center">
  <strong>Unified backend daemon and orchestration engine for TodeX 2.0 AI coding agents.</strong>
</p>

<p align="center">
  <a href="README.md">English</a> •
  <a href="README.zh-CN.md">简体中文</a>
</p>

---

## Overview

`todex-agentd` is the core backend service of the TodeX ecosystem. Built with Rust, Tokio, and Axum, it orchestrates multiple AI coding assistants—including **Codex app-server**, **Agent Client Protocol (ACP 2.0)**, **Pi RPC**, and **Claude Code stream-json**—behind a unified, secure, and persistent API.

In TodeX 2.0, all interactions are consolidated under the `/v2` surface (REST endpoints and a unified `/v2/ws` WebSocket). Conversation folders serve as the single source of truth for message histories, journals, snapshots, and native provider sessions.

---

## Key Features

- **Multi-Agent Orchestration**:
  - **Codex**: Native JSON-RPC app-server integration (`start`, `turn`, `status`, `stop`, `attach`, `replay`, `interrupt`).
  - **ACP (Agent Client Protocol 2.0)**: Supports pre-configured profiles defined in `config.toml` with strict boundary constraints.
  - **Pi**: Native RPC integration with command discovery (`get_commands`), dynamic models, and interactive tool approval handling.
  - **Claude Code**: Stream-JSON driver integrating directly with the Claude CLI.
- **Conversation Folder Persistence**:
  - Structured storage under `$DATA_DIR/conversations/<uuid>/`:
    - `manifest.json`: Metadata, active provider profile, workspace path, and timestamps.
    - `events.jsonl`: Append-only event journal enabling resilient resumption.
    - `snapshot.json`: Compact conversation state snapshots.
    - `provider-state.json`: Native agent engine state for turn resumption across restarts.
  - Optimistic turn concurrency protection (returns `409 Conflict` on concurrent mutations; no arbitrary prompt queueing).
- **Read-Only Capability Catalogs**:
  - Real-time introspection of native Skills, MCP servers, Slash Commands, and Model catalogs directly from installed providers.
  - Applies project-over-user precedence hierarchy without mutating local provider configurations.
  - Native Skill injection into agent prompts via `resourceId` (no client file uploads required).
- **Unified Multiplexed WebSocket (`/v2/ws`)**:
  - Single connection handling conversation streams, turn events, interactive permission requests, local terminal/PTY sessions, and runtime controls.
  - Heartbeat detection, sequence-based reconnection (`afterSequence`), and UTF-8 frame length enforcement.
- **Post-Quantum Transport Encryption**:
  - Configurable end-to-end transport layer encryption:
    - **Plaintext** (`none`)
    - **X25519-ChaCha20Poly1305** (`x25519`)
    - **ML-KEM-768** (`ml-kem-768`, NIST Post-Quantum standard)
  - Key exchange parameters are seamlessly exchanged via TUI pairing QR codes.
- **Security & Sandboxing**:
  - Fail-closed Bearer token authentication (unauthorized requests are rejected with `401 Unauthorized`).
  - Tenant isolation (`tenant_id`) enforced across all conversation queries, event journals, and subscriptions.
  - Workspace root boundary enforcement (`workspace_root`) restricting client access to authorized filesystem scopes.
  - Sanitized subprocess environments preventing leak of administrative environment variables.
- **Interactive TUI & Daemon Management**:
  - Interactive Terminal UI (`cargo run -- tui`) built with Ratatui to monitor status, inspect logs, control daemon lifecycle, and generate pairing QR codes with automatic LAN IP resolution.
  - Background daemon management (`start`, `stop`, `restart`, `status`) backed by a persistent PID file.

---

## Architecture

```
                      +---------------------------------------+
                      |   TodeX Desktop / TodeX Mobile App    |
                      +-------------------+-------------------+
                                          |
                        HTTP /v2/*        |   WebSocket /v2/ws
                       (Auth / REST)      |   (Events, Streams, PTY)
                                          v
+---------------------------------------------------------------------------------+
|                                  todex-agentd                                   |
|                                                                                 |
|  +---------------------+  +----------------------+  +------------------------+  |
|  |   Auth & Security   |  |   Transport Crypto   |  |     Workspace Store    |  |
|  | (Bearer / Tenants)  |  | (X25519 / ML-KEM)    |  | (Sandbox Root Bounds)  |  |
|  +---------------------+  +----------------------+  +------------------------+  |
|                                                                                 |
|  +---------------------------------------------------------------------------+  |
|  |                          Conversation Engine / Hub                        |  |
|  |     (Manifests, Events Journal, Snapshots, Turn Concurrency Control)      |  |
|  +---------------------------------------------------------------------------+  |
|                                                                                 |
|  +---------------------------------------------------------------------------+  |
|  |                             Provider Drivers                              |  |
|  |  +----------------+  +----------------+  +--------------+  +-----------+  |  |
|  |  | Codex Gateway  |  |   ACP 2.0      |  |    Pi RPC    |  |Claude Code|  |  |
|  |  +----------------+  +----------------+  +--------------+  +-----------+  |  |
|  +---------------------------------------------------------------------------+  |
+---------------------------------------------------------------------------------+
                                          |
                      +-------------------+-------------------+
                      | Native Agent CLI Processes / Subtools |
                      |    (codex, acp profile, pi, claude)   |
                      +---------------------------------------+
```

---

## Quick Start

### Prerequisites

- Rust toolchain (MSRV 1.80+ recommended)
- At least one AI agent CLI installed and authenticated on the machine (`codex`, `pi`, `claude`, etc.)

### 1. Build the Binary

```bash
cargo build --release
```

### 2. Running the Server

#### Option A: Interactive TUI Controller (Recommended for local dev)

```bash
cargo run -- tui
```

The TUI allows starting and stopping the background daemon, viewing live server logs, and displaying QR codes for mobile client pairing. Quitting the TUI keeps the daemon running in the background.

#### Option B: Foreground Server

```bash
cargo run -- serve --host 127.0.0.1 --port 7345
```

#### Option C: Background Daemon Mode

```bash
# Start detached daemon
cargo run -- daemon start

# Check status
cargo run -- daemon status

# Restart daemon
cargo run -- daemon restart

# Stop daemon
cargo run -- daemon stop
```

---

## Configuration

Configuration values are resolved using the following precedence:
1. **Command-Line Arguments**
2. **Environment Variables**
3. **Configuration File** (`$TODEX_AGENTD_DATA_DIR/config.toml`)
4. **Built-in Defaults**

### Configuration Options

| Option | CLI Flag | Environment Variable | Default Value | Description |
| :--- | :--- | :--- | :--- | :--- |
| **Host** | `--host` | `TODEX_AGENTD_HOST` | `127.0.0.1` | Binding network interface address. |
| **Port** | `--port` | `TODEX_AGENTD_PORT` | `7345` | TCP port for HTTP and WebSocket. |
| **Data Dir** | `--data-dir` | `TODEX_AGENTD_DATA_DIR` | `~/.todex-agent` | Storage directory for configs, manifests, and logs. |
| **Workspace Root** | `--workspace-root` | `TODEX_AGENTD_WORKSPACE_ROOT` | `~/projects` | Root boundary for authorized project directories. |
| **Default Agent** | — | `TODEX_AGENTD_DEFAULT_AGENT` | `codex` | Default provider (`codex`, `acp`, `pi`, `claude-code`). |
| **Codex Binary** | — | `TODEX_AGENTD_CODEX_BIN` | `codex` | Path or executable name for Codex CLI. |
| **Claude Binary** | — | `TODEX_AGENTD_CLAUDE_BIN` | `claude` | Path or executable name for Claude Code CLI. |
| **Pi Binary** | — | `TODEX_AGENTD_PI_BIN` | `pi` | Path or executable name for Pi CLI. |
| **Enable Auth** | — | `TODEX_AGENTD_ENABLE_AUTH` | `true` | Enables fail-closed Bearer authentication. |
| **Auth Token** | — | `TODEX_AGENTD_AUTH_TOKEN` | *None* | Bearer token secret. |
| **Pairing Encryption** | — | `TODEX_AGENTD_PAIRING_ENCRYPTION` | `ml-kem-768` | Pairing encryption algorithm (`none`, `x25519`, `ml-kem-768`). |

### Example `config.toml`

Located at `~/.todex-agent/config.toml`:

```toml
host = "127.0.0.1"
port = 7345
pairing_encryption = "ml-kem-768"
data_dir = "~/.todex-agent"
workspace_root = "~/projects"

[agent]
default_agent = "codex"
codex_bin = "codex"
claude_bin = "claude"
pi_bin = "pi"

[agent.acp_profiles.default]
command = "mcp-server"
args = ["--stdio"]

[security]
enable_auth = true
enable_tls = false
auth_token = "your-secure-secret-token"
```

> [!NOTE]
> `enable_tls = true` is intentionally blocked on the native listener to prevent false security assumptions. For remote or production access, terminate TLS using a trusted reverse proxy (e.g., Nginx, Caddy, Cloudflare Tunnel).

---

## API & WebSocket Reference

### HTTP Endpoints (`/v2`)

- `GET /health`: Health status probe.
- `GET /v2/version`: Returns daemon version, workspace root, and capabilities.
- `GET /v2/workspaces`: List cached workspaces for current tenant.
- `PUT /v2/workspaces`: Merge workspace caches, return canonical workspace IDs, and automatically trust undecided directories within the backend workspace boundary.
- `GET|PUT /v2/workspaces/{workspaceId}/trust`: Read or explicitly change owner-scoped execution trust. New workspaces are untrusted by default.
- `DELETE /v2/workspaces/{workspaceId}`: Revoke trust, cancel active turns, and remove one workspace from the current tenant's catalog without deleting its conversations.
- `GET /v2/workspace/entries?workspace=...&query=...`: Workspace file and folder suggestions for `@` picker.
- `GET /v2/workspace/directories?path=...`: Directory tree explorer.
- `GET /v2/workspace/file?path=...`: Read file contents within sandbox root.
- `GET /v2/browser/fetch?url=...`: Proxy web resource fetching.
- `GET /v2/providers`: List available agent providers and their active states.
- `GET /v2/providers/models?provider=...&workspace=...`: Discover supported models for a provider.
- `GET /v2/providers/commands?provider=...&workspace=...`: Query slash commands and extensions.
- `GET /v2/conversations`: List persisted conversations for tenant.
- `POST /v2/conversations`: Create a new conversation folder with selected agent provider.
- `GET /v2/conversations/{id}`: Fetch conversation manifest and details.
- `GET /v2/conversations/{id}/events?afterSequence=0&limit=200`: Paginated event journal query.
- `POST /v2/conversations/{id}/prompt`: Dispatch a prompt turn with text, typed content, model, reasoning effort, and skill resource IDs. Local files remain confined to the trusted workspace.
- `POST /v2/conversations/{id}/cancel`: Cancel active running turn.
- `POST /v2/conversations/{id}/permissions/{permissionId}`: Resolve interactive approval request.

### WebSocket Endpoint (`/v2/ws`)

The single endpoint `/v2/ws` handles:
1. Subscription to real-time conversation event journals (`conversation.subscribe`).
2. Dispatching prompt turns and cancellations.
3. Interactive permission decisions.
4. Interactive PTY terminal sessions (`terminal.open`, `terminal.input`, `terminal.resize`, `terminal.close`).
5. Local Codex engine process control.

---

## Development & Verification

Run the standard check suite before committing:

```bash
# Check compilation
cargo check

# Check formatting
cargo fmt --all --check

# Run linter
cargo clippy --locked --all-targets --all-features

# Run unit tests
cargo test

# Read-only Codex/Pi installation, login, and RPC discovery
cargo run -- doctor providers --provider codex,pi --format json

# Run the billable Codex/Pi smoke explicitly
TODEX_REAL_E2E=1 TODEX_REAL_ALLOW_BILLABLE=1 TODEX_REAL_PROVIDERS=codex,pi \
  cargo test --test e2e_real_codex real_v2_provider_http_ws_roundtrip \
  -- --ignored --test-threads=1
```

---

## Related Repositories

- **[TodeX Desktop](../TodeX_desktop)**: macOS desktop client built with Electron, React 19, and HeroUI Pro.
- **[TodeX App](../TodeX_app)**: Mobile client built with React Native and Expo SDK 57.

---

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
