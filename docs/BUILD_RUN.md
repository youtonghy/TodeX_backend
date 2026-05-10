# TodeX Backend 运行与编译文档

这份文档说明如何在当前仓库里启动、编译和做基础校验。

## 项目概览

- 包名：`todex-agentd`
- 非交互运行入口：`cargo run -- serve`
- 交互式 TUI 启动入口：`cargo run -- tui`
- 默认监听：`127.0.0.1:7345`
- 默认 WebSocket：`ws://127.0.0.1:7345/v1/ws`
- 默认数据目录：`~/.todex-agent`
- 默认 workspace 根目录：`~/projects`
- 本地 Codex 控制：`codex app-server --listen stdio://`

## 环境要求

- Rust 工具链
- `cargo`
- `codex` 可执行文件在 `PATH` 中，或通过 `TODEX_AGENTD_CODEX_BIN` 指定路径

建议先检查版本：

```bash
rustc --version
cargo --version
codex --version
```

## 配置方式

配置优先级：

1. 命令行参数
2. 环境变量
3. `$TODEX_AGENTD_DATA_DIR/config.toml`
4. 默认值

常用环境变量：

| 变量 | 说明 |
| --- | --- |
| `TODEX_AGENTD_HOST` | 监听地址 |
| `TODEX_AGENTD_PORT` | 监听端口 |
| `TODEX_AGENTD_DATA_DIR` | 数据目录 |
| `TODEX_AGENTD_WORKSPACE_ROOT` | workspace 根目录 |
| `TODEX_AGENTD_CODEX_BIN` | `codex` 命令路径 |
| `TODEX_AGENTD_DEFAULT_AGENT` | 默认 agent 名称 |
| `TODEX_AGENTD_ENABLE_AUTH` | 是否开启认证 |
| `TODEX_AGENTD_ENABLE_TLS` | 是否开启 TLS |
| `TODEX_AGENTD_AUTH_TOKEN` | WebSocket Bearer token |

配置文件示例：

```toml
host = "127.0.0.1"
port = 7345
data_dir = "/home/user/.todex-agent"
workspace_root = "/home/user/projects"

[agent]
default_agent = "codex"
codex_bin = "codex"

[security]
enable_auth = true
enable_tls = false
auth_token = "replace-me"
```

## 如何编译

### 开发编译

```bash
cargo build
```

### 发布编译

```bash
cargo build --release
```

### 只做语法和依赖检查

```bash
cargo check
```

### 格式检查

```bash
cargo fmt --check
```

### 基础静态检查

```bash
cargo clippy
```

### 真实 Codex E2E

真实 E2E 会启动 `todex-agentd` 子进程，并通过 WebSocket 驱动真实 `codex app-server --listen stdio://`。默认不会运行，必须显式开启：

```bash
TODEX_REAL_E2E=1 cargo test --test e2e_real_codex -- --ignored --test-threads=1
```

前置条件：

- `codex` 已安装并可执行，或设置 `TODEX_REAL_CODEX_BIN=/absolute/path/to/codex`
- 默认使用临时 workspace；如需指定真实任务目录，设置 `TODEX_REAL_WORKSPACE=/absolute/path/to/workspace`
- 默认从当前 `CODEX_HOME`/`~/.codex` 复制登录凭据到临时 `CODEX_HOME`，并移除 MCP/marketplace 配置，避免本机 MCP 启动状态影响后端控制链路测试；如需完全指定 Codex home，设置 `TODEX_REAL_CODEX_HOME=/absolute/path/to/codex-home`
- Codex 已登录，且当前环境能连接模型
- 允许测试消耗少量模型调用额度

测试覆盖 HTTP `/health`、`/v1/version`、WebSocket `/v1/ws`、认证失败、租户不匹配、旧协议拒绝、本地 Codex start/status/stop、真实 turn、Plan 模式、approval 响应、replay/attach/snapshot、并行多 session 和同 session busy rejection。

## 如何运行

### 默认运行

```bash
cargo run -- serve
```

### 使用 TUI 管理启动

```bash
cargo run -- tui
```

TUI 默认进入未启动状态，可以在界面里查看当前监听地址、数据目录、workspace 根目录、运行状态、运行时长和活跃 Codex adapter 数量。常用快捷键：

| 按键 | 作用 |
| --- | --- |
| `s` | 启动或停止服务 |
| `r` | 重启服务 |
| `h` | 修改监听 IP |
| `p` | 修改监听端口 |
| `w` | 保存监听 IP 和端口到 `$TODEX_AGENTD_DATA_DIR/config.toml` |
| `q` / `Esc` | 退出 TUI，并停止由 TUI 启动的服务 |

也可以用方向键选择操作项，然后按 Enter 执行。

### 指定监听地址和端口

```bash
cargo run -- serve --host 127.0.0.1 --port 7345
cargo run -- tui --host 127.0.0.1 --port 7345
```

### 指定数据目录和 workspace 根目录

```bash
cargo run -- serve \
  --data-dir ~/.todex-agent \
  --workspace-root ~/projects
```

### 使用环境变量运行

```bash
export TODEX_AGENTD_HOST=127.0.0.1
export TODEX_AGENTD_PORT=7345
export TODEX_AGENTD_DATA_DIR="$HOME/.todex-agent"
export TODEX_AGENTD_WORKSPACE_ROOT="$HOME/projects"
export TODEX_AGENTD_AUTH_TOKEN="replace-me"
cargo run -- serve
```

## 启动后检查

服务启动后会打印一条监听日志，类似：

```text
todex-agentd listening
```

可以用以下接口确认服务正常：

```bash
curl http://127.0.0.1:7345/health
curl http://127.0.0.1:7345/v1/version
```

## WebSocket 连接验证

```bash
websocat -H "Authorization: Bearer ${TODEX_AGENTD_AUTH_TOKEN}" ws://127.0.0.1:7345/v1/ws
```

发送状态查询消息：

```json
{
  "id": "status-1",
  "type": "codex.local.status",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local"
  }
}
```

## 常见错误

### 绑定端口失败

原因通常是端口被占用。换一个端口即可：

```bash
cargo run -- serve --port 7346
```

### `codex` 不存在

把 `TODEX_AGENTD_CODEX_BIN` 指向正确路径，或确认 `codex` 已在 `PATH` 中。

### WebSocket 消息返回 `UNAUTHENTICATED`

确认服务端设置了 `TODEX_AGENTD_AUTH_TOKEN`，并且 WebSocket 握手带上：

```http
Authorization: Bearer <TODEX_AGENTD_AUTH_TOKEN>
```

### `codex.local.start` 返回 `INVALID_CWD`

`payload.cwd` 必须是本机已存在目录。

## 推荐最小流程

```bash
cargo check
cargo run -- serve
```

然后用另一个终端验证：

```bash
curl http://127.0.0.1:7345/health
```
