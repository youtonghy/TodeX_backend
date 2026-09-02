# TodeX 后端服务 (`todex-agentd`)

<p align="center">
  <strong>统一的 TodeX 2.0 AI 编程 Agent 后端守护进程与编排引擎。</strong>
</p>

<p align="center">
  <a href="README.md">English</a> •
  <a href="README.zh-CN.md">简体中文</a>
</p>

---

## 概述

`todex-agentd` 是 TodeX 生态的核心后端服务。基于 Rust、Tokio 和 Axum 构建，它为多种 AI 编程助手（包括 **Codex app-server**、**Agent Client Protocol (ACP 2.0)**、**Pi RPC** 和 **Claude Code stream-json**）提供统一、安全且持久化的编排与控制接口。

在 TodeX 2.0 中，所有对外交互均收敛至 `/v2` 控制面（REST 接口与统一的 `/v2/ws` WebSocket）。基于会话目录（Conversation Folder）的持久化设计构成了消息历史、事件日志、状态快照与原生 Provider 会话的核心存储机制。

---

## 核心特性

- **多 Agent 统一编排**：
  - **Codex**：原生 JSON-RPC app-server 驱动（支持 `start`、`turn`、`status`、`stop`、`attach`、`replay`、`interrupt` 等控制）。
  - **ACP (Agent Client Protocol 2.0)**：支持 `config.toml` 中预配置的受控 profile，防止任意命令注入。
  - **Pi**：原生 RPC 驱动，支持命令目录发现（`get_commands`）、动态模型查询与交互式工具审批。
  - **Claude Code**：基于 `stream-json` 协议直接对接 Claude CLI。
- **会话目录持久化体系**：
  - 会话数据独立保存于 `$DATA_DIR/conversations/<uuid>/` 目录下：
    - `manifest.json`：存储元数据、选定 Provider Profile、工作区路径及时间戳。
    - `events.jsonl`：仅追加的事件日志，支持按序列号断点续传。
    - `snapshot.json`：压缩后的快速会话状态快照。
    - `provider-state.json`：原生 Agent 引擎状态，支持重启后无缝 resume 会话。
  - 乐观并发控制（并发发起变更 turn 时返回 `409 Conflict`，避免未知的盲目排队）。
- **只读能力目录（Capability Catalogs）**：
  - 实时读取原生已安装 Agent 的 Skills、MCP Servers、斜杠命令与模型目录。
  - 应用“项目级优先于用户级”的配置优先级规则，绝不擅自篡改本地 Provider 配置文件。
  - 支持后端按 `resourceId` 注入 Skill 内容（前端无需上传完整 Skill 文本）。
- **统一多路复用 WebSocket (`/v2/ws`)**：
  - 单一 WebSocket 连接承载会话事件流、实时推理流、交互式权限审批、本地终端 PTY 会话以及运行控制指令。
  - 具备心跳保活检测、基于序列号的断线重连（`afterSequence`）及 UTF-8 帧大小保护。
- **后量子传输加密**：
  - 支持端到端传输层加密：
    - **明文** (`none`)
    - **X25519-ChaCha20Poly1305** (`x25519`)
    - **ML-KEM-768** (`ml-kem-768`，NIST 后量子密码学标准)
  - 密钥交换参数通过 TUI 配对二维码无缝传输。
- **安全与沙箱边界**：
  - Fail-Closed Bearer Token 认证（未授权直接拦截并返回 `401 Unauthorized`）。
  - 租户隔离（`tenant_id`），严格校验所有会话读取、订阅与变更入口。
  - 工作区根目录约束（`workspace_root`），强力防御未经授权的路径穿越。
  - 净化的子进程环境，只继承基础系统变量，避免泄露主机敏感凭据。
- **交互式 TUI 与守护进程管理**：
  - 基于 Ratatui 构建的交互式 TUI（`cargo run -- tui`）：查看运行状态、实时日志、启停守护进程，并自动探测局域网 IP 生成移动端配对二维码。
  - 基于 PID 文件管理的后台守护进程模式（`start`、`stop`、`restart`、`status`）。

---

## 系统架构

```
                      +---------------------------------------+
                      |   TodeX Desktop / TodeX Mobile App    |
                      +-------------------+-------------------+
                                          |
                        HTTP /v2/*        |   WebSocket /v2/ws
                       (Auth / REST)      |   (事件流, 交互, PTY)
                                          v
+---------------------------------------------------------------------------------+
|                                  todex-agentd                                   |
|                                                                                 |
|  +---------------------+  +----------------------+  +------------------------+  |
|  |     认证与安全      |  |       传输加密       |  |       工作区存储       |  |
|  | (Bearer / 租户隔离) |  | (X25519 / ML-KEM)    |  |  (沙箱 workspace_root) |  |
|  +---------------------+  +----------------------+  +------------------------+  |
|                                                                                 |
|  +---------------------------------------------------------------------------+  |
|  |                           会话引擎 / 调度中心                             |  |
|  |          (Manifests, 事件日志, 快照, Turn 乐观并发控制)                   |  |
|  +---------------------------------------------------------------------------+  |
|                                                                                 |
|  +---------------------------------------------------------------------------+  |
|  |                             Provider 适配器                               |  |
|  |  +----------------+  +----------------+  +--------------+  +-----------+  |  |
|  |  | Codex Gateway  |  |   ACP 2.0      |  |    Pi RPC    |  |Claude Code|  |  |
|  |  +----------------+  +----------------+  +--------------+  +-----------+  |  |
|  +---------------------------------------------------------------------------+  |
+---------------------------------------------------------------------------------+
                                          |
                      +-------------------+-------------------+
                      |       原生 Agent CLI 进程与子工具     |
                      |    (codex, acp profile, pi, claude)   |
                      +---------------------------------------+
```

---

## 快速开始

### 前置要求

- Rust 工具链（推荐 MSRV 1.80+）
- 本机已安装并完成登录认证的 AI Agent CLI（如 `codex`、`pi`、`claude` 等）

### 1. 编译构建

```bash
cargo build --release
```

### 2. 启动服务

#### 方式 A：交互式 TUI 控制台（推荐本地开发使用）

```bash
cargo run -- tui
```

在 TUI 控制台中可以直观启动/停止后台守护进程、查看实时日志，并显示移动端配对二维码。退出 TUI 不会终止后台守护进程。

#### 方式 B：前台运行服务

```bash
cargo run -- serve --host 127.0.0.1 --port 7345
```

#### 方式 C：后台守护进程模式

```bash
# 启动后台守护进程
cargo run -- daemon start

# 查看状态
cargo run -- daemon status

# 重启守护进程
cargo run -- daemon restart

# 停止守护进程
cargo run -- daemon stop
```

---

## 配置说明

配置优先级依次为：
1. **命令行参数**
2. **环境变量**
3. **配置文件** (`$TODEX_AGENTD_DATA_DIR/config.toml`)
4. **内置默认值**

### 常用配置参数

| 配置项 | 命令行参数 | 环境变量 | 默认值 | 描述 |
| :--- | :--- | :--- | :--- | :--- |
| **监听主机** | `--host` | `TODEX_AGENTD_HOST` | `127.0.0.1` | 服务绑定的网络接口地址。 |
| **监听端口** | `--port` | `TODEX_AGENTD_PORT` | `7345` | HTTP 及 WebSocket 监听端口。 |
| **数据目录** | `--data-dir` | `TODEX_AGENTD_DATA_DIR` | `~/.todex-agent` | 存储配置文件、会话目录与日志的位置。 |
| **工作区根目录** | `--workspace-root` | `TODEX_AGENTD_WORKSPACE_ROOT` | `~/projects` | 允许客户端访问的工作区根沙箱路径。 |
| **默认 Agent** | — | `TODEX_AGENTD_DEFAULT_AGENT` | `codex` | 默认 Provider（`codex`、`acp`、`pi`、`claude-code`）。 |
| **Codex 可执行文件** | — | `TODEX_AGENTD_CODEX_BIN` | `codex` | Codex CLI 路径或命令名称。 |
| **Claude 可执行文件**| — | `TODEX_AGENTD_CLAUDE_BIN` | `claude` | Claude Code CLI 路径或命令名称。 |
| **Pi 可执行文件**    | — | `TODEX_AGENTD_PI_BIN` | `pi` | Pi CLI 路径或命令名称。 |
| **启用认证** | — | `TODEX_AGENTD_ENABLE_AUTH` | `true` | 是否启用 Fail-closed Bearer 认证。 |
| **认证 Token** | — | `TODEX_AGENTD_AUTH_TOKEN` | *无* | Bearer Token 密钥。 |
| **配对加密方式** | — | `TODEX_AGENTD_PAIRING_ENCRYPTION` | `ml-kem-768` | 配对加密算法（`none`、`x25519`、`ml-kem-768`）。 |

### `config.toml` 配置示例

文件位于 `~/.todex-agent/config.toml`：

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
> 原生监听器禁止配置 `enable_tls = true`，以避免“误以为已经安全”的安全假设。在生产或远程暴露环境中，请使用受信任的反向代理（如 Nginx、Caddy、Cloudflare Tunnel）来终结 TLS。

---

## API 与 WebSocket 接口概览

### HTTP 接口 (`/v2`)

- `GET /health`：服务健康检查。
- `GET /v2/version`：获取服务端版本号、工作区根目录与支持能力。
- `GET /v2/workspaces`：获取当前租户已缓存的工作区列表。
- `PUT /v2/workspaces`：合并当前租户的工作区缓存，并返回统一的工作区 ID。
- `GET|PUT /v2/workspaces/{workspaceId}/trust`：读取或显式修改按租户隔离的执行信任；新工作区默认不信任。
- `DELETE /v2/workspaces/{workspaceId}`：撤销信任、取消活动 turn，并从当前租户的目录中移除工作区，但保留其对话历史。
- `GET /v2/workspace/entries?workspace=...&query=...`：为 `@` 提及提供工作区文件与目录补全。
- `GET /v2/workspace/directories?path=...`：目录树浏览查询。
- `GET /v2/workspace/file?path=...`：读取沙箱内指定文件内容。
- `GET /v2/browser/fetch?url=...`：代理获取网页内容。
- `GET /v2/providers`：获取已启用的 Agent Provider 列表与可用状态。
- `GET /v2/providers/models?provider=...&workspace=...`：动态获取指定 Provider 支持的模型列表。
- `GET /v2/providers/commands?provider=...&workspace=...`：查询斜杠命令与扩展。
- `GET /v2/conversations`：获取持久化的会话列表。
- `POST /v2/conversations`：创建新会话目录并锁定指定 Agent Provider。
- `GET /v2/conversations/{id}`：获取指定会话的元数据清单。
- `GET /v2/conversations/{id}/events?afterSequence=0&limit=200`：分页拉取事件日志。
- `POST /v2/conversations/{id}/prompt`：发送 prompt（支持文本、类型化内容、模型覆盖、推理强度及 Skill resourceId）；本地文件只能来自受信任工作区。
- `POST /v2/conversations/{id}/cancel`：取消当前正在执行的 turn。
- `POST /v2/conversations/{id}/permissions/{permissionId}`：响应交互式权限审批请求。

### WebSocket 统一接口 (`/v2/ws`)

统一的 `/v2/ws` 承载：
1. 实时订阅会话事件流（`conversation.subscribe`）。
2. 发送 prompt 与取消执行指令。
3. 响应权限与工具调用审批。
4. 交互式 PTY 终端会话（`terminal.open`、`terminal.input`、`terminal.resize`、`terminal.close`）。
5. 本地 Codex 引擎控制。

---

## 开发与验证

提交代码前请执行以下标准验证套件：

```bash
# 检查编译
cargo check

# 检查代码格式
cargo fmt --all --check

# 执行 Clippy 静态检查
cargo clippy --locked --all-targets --all-features

# 运行单元测试
cargo test

# 运行真实 Codex 端到端测试（需要本机有已登录的 codex CLI）
TODEX_REAL_E2E=1 cargo test --test e2e_real_codex -- --ignored --test-threads=1
```

---

## 相关仓库

- **[TodeX Desktop](../TodeX_desktop)**：基于 Electron、React 19 与 HeroUI Pro 的 macOS 桌面端客户端。
- **[TodeX App](../TodeX_app)**：基于 React Native 与 Expo SDK 57 的移动端应用。

---

## 开源协议

本项目采用 MIT 许可证 - 详情参见 [LICENSE](LICENSE) 文件。
