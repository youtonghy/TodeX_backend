# TodeX Backend / TodeX 后端

`todex-agentd` 是 TodeX 的后端服务，负责提供 HTTP 健康检查、版本信息、工作区文件检索，以及 WebSocket 协议网关，并通过 `codex app-server --listen stdio://` 管理本地 Codex 会话。

`todex-agentd` is the backend service for TodeX. It provides HTTP health/version/workspace-entry endpoints, a WebSocket protocol gateway, and local Codex session management through `codex app-server --listen stdio://`.

## 功能 / Features

- HTTP 接口：`/health`、`/v1/version`、`/v1/workspace/entries`
- WebSocket 接口：`/v1/ws`
- 本地 Codex 会话管理：`start`、`status`、`stop`、`turn`、`attach`、`replay`、`interrupt`
- 工作区文件检索：为前端 `@` 引用提供目录和文件建议
- 认证与配置：支持 Bearer token、环境变量、`config.toml`
- 交互式 TUI：可启动、停止服务，并保存监听地址

- HTTP endpoints: `/health`, `/v1/version`, `/v1/workspace/entries`
- WebSocket endpoint: `/v1/ws`
- Local Codex session control: `start`, `status`, `stop`, `turn`, `attach`, `replay`, `interrupt`
- Workspace file lookup for the frontend `@` picker
- Auth and config via Bearer token, environment variables, and `config.toml`
- Interactive TUI for starting/stopping the service and saving host/port

## 快速开始 / Quick Start

### 1. 安装依赖 / Install

```bash
cargo build
```

### 2. 启动服务 / Run the server

```bash
cargo run -- serve
```

### 3. 或者使用 TUI / Or use the TUI

```bash
cargo run -- tui
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
- `TODEX_AGENTD_DEFAULT_AGENT`
- `TODEX_AGENTD_ENABLE_AUTH`
- `TODEX_AGENTD_ENABLE_TLS`
- `TODEX_AGENTD_AUTH_TOKEN`

## 使用方式 / How to Use

1. 先启动后端服务。
2. 让前端客户端连接到 `http://127.0.0.1:7345` 或你自己的地址。
3. 在设置里填写 `Auth token` 和 `Tenant id`。
4. 通过 `/v1/workspace/entries` 为 `@` 引用提供文件建议。
5. 通过 WebSocket `/v1/ws` 收发协议事件。

1. Start the backend service first.
2. Point the frontend client to `http://127.0.0.1:7345` or your own host.
3. Fill in `Auth token` and `Tenant id` in the client settings.
4. Use `/v1/workspace/entries` to power `@` file suggestions.
5. Exchange protocol events over WebSocket `/v1/ws`.

## 常用检查 / Common Checks

```bash
cargo check
cargo fmt --check
cargo clippy
curl http://127.0.0.1:7345/health
curl http://127.0.0.1:7345/v1/version
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

