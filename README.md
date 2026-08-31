# TodeX Backend / TodeX 后端

`todex-agentd` 是 TodeX 2.0 后端。它以 conversation folder 为持久化核心，统一驱动 ACP、Codex、Pi 和 Claude Code。全部对外接口都在 `/v2`（HTTP 资源 + 统一 `/v2/ws` WebSocket）；`/v1/*` 已删除。

`todex-agentd` is the TodeX 2.0 backend. Conversation folders are the persistence core for ACP, Codex, Pi, and Claude Code. The whole API surface now lives under `/v2` (HTTP resources plus the unified `/v2/ws` WebSocket); `/v1/*` has been removed.

## 功能 / Features

- HTTP 接口：`/health`、`/v2/version`、`/v2/workspaces`、`/v2/workspace/entries`、`/v2/workspace/directories`、`/v2/workspace/file`、`/v2/browser/fetch`
- v2 对话接口：`/v2/providers`、`/v2/conversations`、`/v2/ws`
- Provider：ACP 配置 profile、Codex app-server、Pi RPC、Claude Code stream-json
- 只读 Catalog：读取各 Provider 原生 Skill/MCP 配置，应用 project-over-user 优先级，不安装、不切换、不改写
- Conversation folder：`$DATA_DIR/conversations/<uuid>/` 下保存 manifest、事件日志、快照与原生 Provider 状态
- WebSocket 接口：统一 `/v2/ws`（conversation 命令 + 终端/本地 Codex/Cloud Code/MCP 控制命令），可选 X25519 或 ML-KEM-768 传输加密，支持 header 与 `access_token` 查询参数认证
- 本地 Codex 会话管理：`start`、`status`、`stop`、`turn`、`attach`、`replay`、`interrupt`
- 工作区权限边界：后端 `workspace_root` 限制移动端只能创建和使用该根目录内的工作区
- 工作区文件检索：为前端 `@` 引用提供目录和文件建议
- 认证与配置：支持 Bearer token、环境变量、`config.toml`
- 后台进程模式：可作为独立 daemon 持续运行，并通过 pidfile 管理状态
- 交互式 TUI：作为控制器启动、停止 daemon，保存监听地址，并显示 App 配对二维码

- HTTP endpoints: `/health`, `/v2/version`, `/v2/workspaces`, `/v2/workspace/entries`, `/v2/workspace/directories`, `/v2/workspace/file`, `/v2/browser/fetch`
- v2 conversation endpoints: `/v2/providers`, `/v2/conversations`, and `/v2/ws`
- ACP profiles, Codex app-server, Pi RPC, and Claude Code stream-json drivers
- Read-only native Skill/MCP catalogs with project-over-user precedence
- Per-conversation manifests, journals, snapshots, and native provider state under `$DATA_DIR/conversations/<uuid>/`
- WebSocket endpoint: the unified `/v2/ws` (conversation commands plus terminal / local Codex / Cloud Code / MCP control), with optional X25519 or ML-KEM-768 transport encryption and header or `access_token` query auth
- Local Codex session control: `start`, `status`, `stop`, `turn`, `attach`, `replay`, `interrupt`
- Workspace boundary enforcement: backend `workspace_root` restricts mobile-created and mobile-used workspaces to that root
- Workspace file lookup for the frontend `@` picker
- Auth and config via Bearer token, environment variables, and `config.toml`
- Persistent daemon mode managed through a pidfile
- Interactive TUI as a controller for starting/stopping the daemon, saving host/port, and showing the app pairing QR

## 快速开始 / Quick Start

### 1. 安装依赖 / Install

```bash
cargo build
```

### 2. 启动服务 / Run the server

后台 daemon：

Persistent daemon:

```bash
cargo run -- daemon start
```

前台运行：

Foreground server:

```bash
cargo run -- serve
```

### 3. 或者使用 TUI 控制 daemon / Or control the daemon with the TUI

```bash
cargo run -- tui
```

在 TUI 中启动后，退出 TUI 不会停止 daemon；TUI 只是控制器。停止服务可在 TUI 中执行 Stop，或运行：

After the TUI starts the daemon, quitting the TUI leaves it running. Stop it from the TUI or run:

```bash
cargo run -- daemon stop
```

默认监听 `127.0.0.1:7345`，数据目录是 `~/.todex-agent`，默认 workspace 根目录是 `~/projects`。

The default listen address is `127.0.0.1:7345`, the default data directory is `~/.todex-agent`, and the default workspace root is `~/projects`.

## 配置 / Configuration

配置优先级：

1. 命令行参数
2. 环境变量
3. `$TODEX_AGENTD_DATA_DIR/config.toml`
4. 内置默认值

Priority order:

1. CLI arguments
2. Environment variables
3. `$TODEX_AGENTD_DATA_DIR/config.toml`
4. Built-in defaults

常用环境变量 / Common env vars:

- `TODEX_AGENTD_HOST`
- `TODEX_AGENTD_PORT`
- `TODEX_AGENTD_DATA_DIR`
- `TODEX_AGENTD_WORKSPACE_ROOT`
- `TODEX_AGENTD_CODEX_BIN`
- `TODEX_AGENTD_CLAUDE_BIN`
- `TODEX_AGENTD_PI_BIN`
- `TODEX_AGENTD_DEFAULT_AGENT`
- `TODEX_AGENTD_ENABLE_AUTH`
- `TODEX_AGENTD_ENABLE_TLS`
- `TODEX_AGENTD_AUTH_TOKEN`

## 使用方式 / How to Use

1. 先启动后端 daemon，或用 `serve` 前台运行。
2. 让前端客户端连接到 `http://127.0.0.1:7345` 或你自己的地址。
3. 在设置里填写 `Auth token` 和 `Tenant id`，或从 TUI 扫描配对二维码导入地址和加密公钥。仅 loopback 二维码携带 token；非 loopback 二维码会省略长期 token。
4. 通过 `/v2/workspace/entries` 为 `@` 引用提供文件建议。
5. 通过 WebSocket `/v2/ws` 收发协议事件（conversation 命令与终端/本地 Codex 控制命令同一条连接）。

1. Start the backend daemon first, or run `serve` in the foreground.
2. Point the frontend client to `http://127.0.0.1:7345` or your own host.
3. Fill in `Auth token` and `Tenant id` in the client settings, or scan the TUI pairing QR. Loopback QR payloads include the token; non-loopback QR payloads intentionally omit the long-lived token.
4. Use `/v2/workspace/entries` to power `@` file suggestions.
5. Use `/v2/ws` for everything WebSocket: conversation commands and terminal / local Codex control share one connection.

## 常用检查 / Common Checks

```bash
cargo check
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features
curl http://127.0.0.1:7345/health
curl http://127.0.0.1:7345/v2/version
```

## 真实 E2E / Real E2E

```bash
TODEX_REAL_E2E=1 cargo test --test e2e_real_codex -- --ignored --test-threads=1
```

需要本机可执行 `codex`，并且已经登录可用。

`codex` must be available on the machine and already signed in.

## 相关文档 / Related Docs

- `docs/API.md`
- `docs/BUILD_RUN.md`
