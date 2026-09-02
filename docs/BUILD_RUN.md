# TodeX Backend 运行与编译文档

这份文档说明如何在当前仓库里启动、编译和做基础校验。

## 项目概览

- 包名：`todex-agentd`
- 非交互运行入口：`cargo run -- serve`
- 后台 daemon 控制入口：`cargo run -- daemon start|stop|restart|status`
- 交互式 TUI 启动入口：`cargo run -- tui`
- 默认监听：`127.0.0.1:7345`
- 默认 WebSocket：`ws://127.0.0.1:7345/v2/ws`
- 默认数据目录：`~/.todex-agent`
- 默认 workspace 根目录：`~/projects`
- 首期 Provider：ACP、Codex CLI、Pi、Claude Code

客户端启动（Backend 先于客户端）：

```bash
# Backend
cd TodeX_backend && cargo run -- tui
# 或非交互：cargo run -- serve --host 127.0.0.1 --port 7345

# Desktop（三栏）
cd TodeX_desktop && pnpm run dev

# App（移动端堆叠导航）
cd TodeX_app && pnpm start
```

## 环境要求

- Rust 工具链
- `cargo`
- 至少安装要使用的 Provider CLI，并放在 `PATH` 中或通过对应环境变量指定路径
- Codex、Pi 和 Claude Code 使用 daemon 所属系统用户的原生配置与登录状态

建议先检查版本：

```bash
rustc --version
cargo --version
codex --version
pi --version
claude --version
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
| `TODEX_AGENTD_CLAUDE_BIN` | `claude` 命令路径 |
| `TODEX_AGENTD_PI_BIN` | `pi` 命令路径 |
| `TODEX_AGENTD_DEFAULT_AGENT` | 默认 agent 名称 |
| `TODEX_AGENTD_ENABLE_AUTH` | 是否开启认证 |
| `TODEX_AGENTD_ENABLE_TLS` | 是否开启 TLS |
| `TODEX_AGENTD_AUTH_TOKEN` | WebSocket Bearer token |

本机临时测试可设置 `TODEX_AGENTD_ENABLE_AUTH=false`；此时后端不会生成或要求 Auth token。生产环境应保持认证开启。

配置文件示例：

```toml
host = "127.0.0.1"
port = 7345
data_dir = "/home/user/.todex-agent"
workspace_root = "/home/user/projects"

[agent]
default_agent = "codex"
codex_bin = "codex"
claude_bin = "claude"
pi_bin = "pi"

[agent.acp_profiles.example]
command = "example-acp-agent"
args = []

[security]
enable_auth = true
enable_tls = false
auth_token = "replace-me"

[tui]
language = "zh-CN" # 也可使用 "en"；可在 TUI 中按 l 切换并持久化
```

`enable_tls = true` 会直接拒绝启动，因为当前 binary 没有证书/私钥配置入口。需要 TLS 时应由可信反向代理终止 TLS；不要把明文监听端口直接暴露到公网。

TUI 默认不捕获鼠标，终端中的文本可以直接拖选复制。按 `c` 打开“凭据与复制”，可完整查看并复制 Auth Token 与当前加密方式的公钥；私钥不会显示或复制。日志使用 `PageUp`、`PageDown`、`Home`、`End` 滚动。daemon 启动会先检查端口占用，并等待最多 30 秒完成迁移和初始化后再报告超时。

Provider 子进程会清空 daemon 的其余环境，只继承基础系统路径、用户目录、locale、代理和 SSH agent 等运行环境。ACP profile 中的 `env` 会显式传入，但名称以 `TODEX_AGENTD_` 开头的变量会被拒绝。Codex、Pi 和 Claude Code 因此应优先使用各自保存在用户目录中的原生登录配置。Pi 首期使用 RPC `--approve`，因为其 RPC 协议没有通用的逐工具审批接口；只应在可信 workspace 与可信 Pi 配置下启用。

## Conversation 数据目录

每个 v2 对话使用服务端生成的 UUID v4 目录：

```text
$TODEX_AGENTD_DATA_DIR/conversations/<conversation-id>/
  manifest.json
  events.jsonl
  snapshot.json
  provider-state.json
```

旧 `$DATA_DIR/codex_gateway/sessions` 会在启动时复制迁移到该结构，源文件保持不变；迁移映射保存在 `$DATA_DIR/migrations/codex-gateway-v1.json`。

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

- `codex` 已安装并可执行，或设置 `TODEX_REAL_CODEX_BIN=/absolute/path/to/codex`。当前验证版本：`codex-cli 0.145.0`（更早版本的 granular `approvalPolicy` 参数已不再被 app-server 接受）
- 默认使用临时 workspace；如需指定真实任务目录，设置 `TODEX_REAL_WORKSPACE=/absolute/path/to/workspace`
- 默认从当前 `CODEX_HOME`/`~/.codex` 复制登录凭据到临时 `CODEX_HOME`，并移除 MCP/marketplace 配置，避免本机 MCP 启动状态影响后端控制链路测试；如需完全指定 Codex home，设置 `TODEX_REAL_CODEX_HOME=/absolute/path/to/codex-home`
- Codex 已登录，且当前环境能连接模型
- 允许测试消耗少量模型调用额度
- 测试环境必须允许监听 127.0.0.1 随机端口、启动 Provider 子进程并读取其状态（不支持无监听能力的沙箱环境）
- 默认模型预期取自安装 CLI 的 `config/read` 实际值；需要钉住版本时设置 `TODEX_REAL_CODEX_MODEL=<model>`

`thread/start` 一律使用与生产适配器一致的 canonical 最小参数（`cwd` + 字符串 `approvalPolicy` + 字符串 `sandbox`）；granular approval map 与 permission profile 属于已被 CLI 移除的旧 schema，不再进入真实 E2E。

测试覆盖 HTTP `/health`、`/v2/version`、WebSocket `/v2/ws`（含统一命令面上的终端与本地 Codex 控制、`session.resume` 断线恢复）、认证矩阵（匿名/错 token 拒绝、header 与 URL 编码 query token 成功）、租户不匹配、旧协议拒绝、本地 Codex start/status/stop、真实 turn、Plan 模式、approval 响应、replay/attach/snapshot、并行多 session 和同 session busy rejection，以及 `/v1/*` 的 404 回归。

### 真实 Provider E2E

ACP、Pi 和 Claude Code 的 v2 round-trip 测试默认忽略，不会在普通 CI 中启动真实 CLI。测试会检查 `/v2/providers`，创建 conversation，通过 `/v2/ws` 订阅，再调用 `/v2/conversations/{id}/prompt` 并等待事件。先用 daemon 用户完成登录：

```bash
TODEX_REAL_E2E=1 TODEX_REAL_PROVIDERS=pi,claude-code \
  cargo test --test e2e_real_codex real_v2_provider_http_ws_roundtrip -- --ignored --nocapture
```

ACP 需要在 `config.toml` 配置受信任的 `[agent.acp_profiles.<name>]`，并把该 provider/profile 配置为可用后再加入 `TODEX_REAL_PROVIDERS`。集成测试也支持用临时配置注入 profile：`TODEX_REAL_ACP_COMMAND=/absolute/path/to/acp-server TODEX_REAL_ACP_PROFILE=real TODEX_REAL_ACP_ARGS='arg1\u001farg2'`。测试不会接受客户端传入任意 command、args 或 env。真实测试会消耗模型额度，只应在隔离 workspace 和专用账号运行。

已验证的 ACP adapter 是 `pi-acp`。它要求 Node.js 22+、Pi 0.80.4+，并复用 Pi 的模型配置：

```bash
npm install -g pi-acp
TODEX_REAL_E2E=1 TODEX_REAL_PROVIDERS=acp \
TODEX_REAL_ACP_COMMAND="$(command -v pi-acp)" TODEX_REAL_ACP_PROFILE=real \
  cargo test --test e2e_real_codex real_v2_provider_http_ws_roundtrip -- --ignored --nocapture
```

## 如何运行

### 生产部署

生产环境建议让 `todex-agentd` 作为单独系统用户运行，数据目录和 workspace 根目录使用该用户可读写的绝对路径。服务只监听 loopback，反向代理负责 HTTPS/WSS 和访问控制。

systemd 示例（`/etc/systemd/system/todex-agentd.service`）：

```ini
[Unit]
Description=TodeX Agent daemon
After=network-online.target

[Service]
User=todex
Group=todex
WorkingDirectory=/var/lib/todex
EnvironmentFile=/etc/todex-agentd.env
ExecStart=/usr/local/bin/todex-agentd serve --host 127.0.0.1 --port 7345 --data-dir /var/lib/todex/data --workspace-root /srv/todex/workspaces
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/var/lib/todex /srv/todex/workspaces

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now todex-agentd
sudo systemctl status todex-agentd
```

macOS 可使用 launchd 的 `ProgramArguments` 指向同一 `serve` 命令，并把 `TODEX_AGENTD_AUTH_TOKEN` 放在 root-only 的 plist 或环境文件中；Windows 则使用 NSSM 或 Windows Service wrapper，服务账户必须拥有数据目录、workspace 和各 Provider CLI 的原生登录配置。不要把 token 写入命令行参数或提交到仓库。

反向代理至少需要转发 `/health` 与 `/v2/*`，启用 WebSocket upgrade，设置合理的 request/body timeout，并只允许 HTTPS 来源。代理到 daemon 的 upstream 仍使用 `http://127.0.0.1:7345`；移动端使用代理公开的 HTTPS/WSS 地址重新生成配对二维码。注意 `access_token` 查询参数可能进入代理访问日志：生产环境优先使用 `Authorization` header 认证。

生产检查清单：

- `TODEX_AGENTD_ENABLE_AUTH=true`，token 使用随机高熵值，并定期轮换。
- `enable_tls=false` 仅表示由代理终止 TLS；禁止直接暴露 7345 明文端口。
- daemon 用户与登录 Provider 的用户一致，且 workspace 目录最小授权。
- 日志、`daemon.json`、`audit/` 和配置文件设置为仅 daemon 用户可读。
- 监控 `/health`、进程重启次数、数据目录剩余空间和 Provider CLI 退出率。
- 备份前停止写入或使用文件系统快照，备份后做一次临时目录恢复和事件 replay 校验。

升级前先停止 daemon，复制整个数据目录到带时间戳的备份目录，再升级 binary 并启动。启动迁移是幂等的：旧 `codex_gateway/sessions` 只读复制到 `conversations/`，不会删除或覆盖源文件。升级失败时停止新 binary，恢复备份目录和旧 binary；不要手工编辑 `events.jsonl` 或 `provider-state.json`。

### 默认运行

后台 daemon 运行：

```bash
cargo run -- daemon start
cargo run -- daemon status
cargo run -- daemon stop
```

daemon 启动后会写入 `~/.todex-agent/daemon.json`，日志写入 `~/.todex-agent/logs/todex-agentd-daemon.log`。

前台运行：

```bash
cargo run -- serve
```

### 使用 TUI 管理启动

```bash
cargo run -- tui
```

TUI 是 daemon 控制器，不再承载核心服务进程。可以在界面里查看当前监听地址、数据目录、workspace 根目录、daemon pid 和运行时长。常用快捷键：

| 按键 | 作用 |
| --- | --- |
| `s` | 启动或停止 daemon |
| `r` | 重启 daemon |
| `h` | 修改监听 IP |
| `p` | 修改监听端口 |
| `w` | 打开 workspace 根目录选择器，并保存到 `$TODEX_AGENTD_DATA_DIR/config.toml` |
| `q` / `Esc` | 退出 TUI；如果 daemon 已启动，它会继续在后台运行 |

也可以用方向键选择操作项，然后按 Enter 执行。workspace 根目录选择器中，Enter/Right 进入选中的子目录，Left/Backspace 返回上级，Space 把当前目录保存为移动端最高层目录；如果 daemon 正在运行，TUI 会用新目录重启 daemon。

### 指定监听地址和端口

```bash
cargo run -- serve --host 127.0.0.1 --port 7345
cargo run -- tui --host 127.0.0.1 --port 7345
```

真机扫码配对需要手机能访问后端监听地址。推荐在可信局域网内用：

```bash
cargo run -- tui --host 0.0.0.0 --port 7345
```

此时服务监听所有网卡，TUI 配对二维码会尽量写入当前局域网 IP，而不是不可访问的 `0.0.0.0`。

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
curl http://127.0.0.1:7345/v2/version
```

## WebSocket 连接验证

```bash
websocat -H "Authorization: Bearer ${TODEX_AGENTD_AUTH_TOKEN}" ws://127.0.0.1:7345/v2/ws
```

无法设置 header 的客户端可使用查询参数：`ws://127.0.0.1:7345/v2/ws?access_token=<url-encoded-token>`。

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

## 从 /v1 迁移到 /v2（已完成）

本次迁移删除了全部 `/v1/*` 接口（HTTP 与 `/v1/ws`）。要点：

- 旧 `/v1/ws` 的终端、本地 Codex 控制、Cloud Code、MCP 命令与事件流已并入 `/v2/ws`，同一连接可同时使用 `conversation.*` 与 `terminal.*` / `codex.*` 命令。
- 断线恢复不再使用 transport hello/chunk/ack 封装：连接建立后发送 `session.resume`，携带客户端持久化的 Codex session cursor，由服务端重放。
- WebSocket 消息上限统一为 8 MiB（聊天附件 base64 传输需要）。
- `/v2/version` 与 `/health` 一样免认证，供 daemon 自检使用。

部署顺序：

1. 本版本 Backend 已删除 `/v1/*`，Backend 与 Desktop/TodeX_app 必须同步升级发布；旧客户端访问本版本 Backend 会在 `/v1/*` 上得到 404。
2. 无法同步发布时的过渡方案：先部署同时提供 `/v1` 与 `/v2` 的过渡版本，客户端切换完成并通过 E2E 后，再部署本版本。

回滚说明：本版本 Backend 与客户端需一起回滚（只回滚 Backend 会让新客户端缺少 `/v2` 资源接口与统一 `/v2/ws` 命令面；只回滚客户端会让旧客户端撞上已删除的 `/v1`）。数据格式未变（conversation folder、workspace 缓存、认证 token 均沿用），回滚不涉及数据迁移。

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
