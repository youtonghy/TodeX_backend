# TodeX Backend API 调用文档

本文档基于当前代码实现整理。TodeX 2.0 的主控制面是 `/v2/conversations` 与 `/v2/ws`，统一支持 ACP、Codex、Pi、Claude Code 和 Grok Build。所有 `/v1/*` 接口已移除：旧 `/v1/ws` 的终端、本地 Codex 控制、Cloud Code、MCP 和事件流能力已并入 `/v2/ws`，旧 HTTP 资源接口已迁至 `/v2/*`。

## 基本信息

- 服务名称：`todex-agentd`
- 默认监听地址：`127.0.0.1:7345`
- 默认 HTTP Base URL：`http://127.0.0.1:7345`
- 默认 WebSocket URL：`ws://127.0.0.1:7345/v2/ws`
- 可选传输加密：`x25519` 或 `ml-kem-768`，由 TUI 配对二维码携带的服务端公钥协商
- 数据格式：JSON
- 字符编码：UTF-8

启动示例：

```bash
cargo run -- serve --host 127.0.0.1 --port 7345
```

移动端真机扫码时不要使用 `127.0.0.1` 作为监听地址；它只指向手机自身。请在可信局域网内用 `--host 0.0.0.0` 启动 TUI/服务后重新生成二维码，二维码会尽量使用后端机器的局域网 IP。

配置来源优先级：

1. 命令行参数
2. 环境变量
3. `$TODEX_AGENTD_DATA_DIR/config.toml`
4. 内置默认值

常用配置项：

| 配置 | 命令行参数 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| 监听主机 | `--host` | `TODEX_AGENTD_HOST` | `127.0.0.1` |
| 监听端口 | `--port` | `TODEX_AGENTD_PORT` | `7345` |
| 数据目录 | `--data-dir` | `TODEX_AGENTD_DATA_DIR` | `~/.todex-agent` |
| Workspace 根目录 | `--workspace-root` | `TODEX_AGENTD_WORKSPACE_ROOT` | `~/projects` |
| Codex 可执行文件 | 无 | `TODEX_AGENTD_CODEX_BIN` | `codex` |
| Claude Code 可执行文件 | 无 | `TODEX_AGENTD_CLAUDE_BIN` | `claude` |
| Pi 可执行文件 | 无 | `TODEX_AGENTD_PI_BIN` | `pi` |
| Grok Build 可执行文件 | 无 | `TODEX_AGENTD_GROK_BIN` | `grok` |
| Grok Build 认证方法 | 无 | `TODEX_AGENTD_GROK_AUTH_METHOD` | 上游默认方法 |
| Grok Build 环境白名单 | 无 | `TODEX_AGENTD_GROK_ENV_ALLOWLIST` | Grok 配置变量与 `XAI_API_KEY` |
| 默认 agent 名称 | 无 | `TODEX_AGENTD_DEFAULT_AGENT` | `codex` |
| 是否开启认证 | 无 | `TODEX_AGENTD_ENABLE_AUTH` | `true` |
| Bearer token | 无 | `TODEX_AGENTD_AUTH_TOKEN` | 无 |

当前 HTTP 层没有实现 TLS 终止，配置 `enable_tls = true` 时服务会拒绝启动，避免产生“已经启用 TLS”的错误安全假设。生产环境应在可信反向代理终止 TLS，且不应直接暴露明文端口。v2 HTTP 和 WebSocket 都使用 `Authorization: Bearer <TODEX_AGENTD_AUTH_TOKEN>`；conversation 持久化 owner tenant，所有读取、订阅与变更入口都会校验 tenant。

认证策略是 fail-closed 的：一旦配置了 token，匿名 `/v2/ws` 握手直接被拒绝（401），不存在“先连上再限制命令”的匿名模式；未配置 token 的本地部署才会以本地信任模式接受匿名连接。

## v2 Conversation API

Provider 标识为 `acp`、`codex`、`pi`、`claude-code`、`grok-build`（创建请求也接受 `grok` 与 `grok_build` 别名）。未指定时使用 `[agent].default_agent`，默认是 `codex`。ACP 必须使用后端 `config.toml` 中预配置的 `providerProfile`；客户端不能提交任意 command、args 或 env。

Provider 子进程只继承运行所需的基础系统环境；ACP 额外使用管理员在 profile 中明确配置的 env。Codex、Pi、Claude Code 和 Grok Build 应先由运行 daemon 的同一系统用户完成原生登录。Grok Build 也可通过白名单传入 `XAI_API_KEY`；daemon 不会启动浏览器/OIDC 交互认证。Grok 的工具授权、提问、计划审批和 MCP elicitation 会转换为 TodeX `permission.requested`，客户端按服务器提供的 `optionId` 和 `kind` 回复。Pi RPC 当前没有覆盖所有工具调用的通用审批接口，因此首期以 `--approve` 启动，`permissions` capability 为 `false`；extension UI 请求仍会转成 TodeX permission 事件，但不能把它等同于逐工具审批。

```http
GET /v2/providers
GET /v2/providers/models?provider=codex&workspace=/home/user/projects/demo
GET /v2/conversations
POST /v2/conversations
GET /v2/conversations/{conversationId}
GET /v2/conversations/{conversationId}/events?afterSequence=0&limit=200
POST /v2/conversations/{conversationId}/prompt
POST /v2/conversations/{conversationId}/cancel
POST /v2/conversations/{conversationId}/permissions/{permissionId}
```

模型目录中的 `contextWindow` 来自 Provider 原生模型元数据；Provider 未公开该值时省略。客户端应结合实时 usage 事件显示上下文占用，不得用静态模型表猜测窗口大小。

`/v2/providers/models` 会实时向指定 Agent 查询模型目录，返回 `source` 与 `fetchedAt`。每个模型包含 `supportedReasoningEfforts`，并可通过 `defaultReasoningEffort` 声明后端当前默认强度。Codex 使用 app-server `model/list`，Pi 使用 RPC `get_available_models` 与 `get_state`，Claude Code 在配置了 `ANTHROPIC_BASE_URL` 时读取 `/v1/models`，Grok Build 从 ACP initialize 的原生模型状态读取。查询失败时客户端应保留上一次成功目录，并展示可恢复错误。

`GET /v2/providers/commands?provider=pi&workspace=/path` 会实时读取 Agent 命令目录。Pi 使用 RPC `get_commands` 返回扩展、Prompt Template 和 Skill；Codex 返回与本机 CLI 版本同步的 TUI 命令适配目录。命令描述包含 `invocation`，客户端应据此选择原生 RPC、桌面动作或 Provider prompt，不要把所有 `/` 输入都当作普通 prompt。

创建对话：

```json
{
  "provider": "codex",
  "workspace": "/home/user/projects/demo",
  "title": "Review backend",
  "providerProfile": null
}
```

发送 prompt：

```json
{
  "text": "检查认证边界",
  "model": null,
  "reasoningEffort": null,
  "skills": [
    { "resourceId": "skill_abc", "name": "review" }
  ]
}
```

`skills` 可选。Backend 按 `resourceId` 读取 Skill 正文并注入到 Provider prompt；客户端不要拼接完整 Skill 文件。只带 Skill、不带 `text` 时允许发送。注入成功后会发出 `skill.injected` 事件；用户可见的 `message.created` 仍是原始输入。

每个 conversation 同时只允许一个 mutating turn；并发 prompt 返回 `409 CONFLICT`，不会排队。daemon 重启会把未完成 turn 标记为 `interrupted`，不会通过重放 prompt 猜测恢复。原生会话 ID 由 `provider-state.json` 保存，Provider 支持时下一 turn 使用原生 resume。

### v2 WebSocket

客户端命令 envelope：

```json
{
  "id": "request-1",
  "type": "conversation.subscribe",
  "payload": {
    "conversationId": "00000000-0000-4000-8000-000000000000",
    "afterSequence": 0,
    "limit": 500
  }
}
```

支持 `conversation.subscribe`、`conversation.create`、`conversation.prompt`、`conversation.cancel`、`conversation.stop`、`conversation.permission.respond`、`mcp.list`、`mcp.refresh`、`mcp.call` 和 `server.ping`。服务端返回 `server.result`、`server.error` 与按 conversation 隔离的 `conversation.event`。订阅会先 replay，再接续实时 sequence。

MCP 真实调用只走 Backend：客户端只发送 `resourceId`、`toolName` 和 `arguments`。Catalog JSON 不含 command、URL 或凭据。调用前必须通过权限 broker，默认拒绝；仅 `allow_once` / `allow_always` 会放行。

### 只读 Skill/MCP Catalog

```http
GET /v2/catalog/skills?provider=claude-code&workspace=/home/user/projects/demo
GET /v2/catalog/skills/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
GET /v2/catalog/mcp?provider=claude-code&workspace=/home/user/projects/demo
```

Catalog 只读取 Provider 的用户级和项目级原生配置，项目级同名资源优先。Skill 正文只能通过后端生成的 `resourceId` 读取；MCP 响应仅返回名称、来源、scope、transport、active 状态以及可选的 tools/authStatus/error，不返回 command、args、env、URL 或凭据。后端不提供安装、启停、删除或改写接口。实际 MCP 调用使用 `mcp.call`，由 daemon 按内部配置执行。

### Conversation Folder 与旧数据迁移

```text
$DATA_DIR/conversations/<uuid-v4>/
  manifest.json
  events.jsonl
  snapshot.json
  provider-state.json
```

`events.jsonl` 是规范事件日志，sequence 从 1 连续递增。启动时会复制迁移旧 `$DATA_DIR/codex_gateway/sessions`；旧文件不修改，迁移可重复执行，并会去除 approval response 和常见 secret 字段。

## HTTP 接口

### 健康检查

```http
GET /health
```

响应：

```text
ok
```

### 版本与运行配置

```http
GET /v2/version
```

该端点与 `/health` 一样不需要认证，供 daemon 自检和客户端连接卡片轮询。

响应字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | Cargo 包名 |
| `version` | string | Cargo 包版本 |
| `data_dir` | string | 当前数据目录 |
| `workspace_root` | string | 当前 workspace 根目录 |

## Workspace 缓存同步

工作区清单由后端持久化到 `$TODEX_AGENTD_DATA_DIR/workspaces.json`。手机和桌面端的本地存储只作为离线缓存；连接成功后会拉取当前身份的后端快照。工作区 ID 由后端根据规范化路径稳定生成，并与对话 manifest 的 `workspaceId` 共用，从而让不同设备恢复同一工作区内的对话。

后端会把 `workspace_root` 作为移动端可用工作区的权限边界。`PUT /v2/workspaces`、`/v2/workspace/entries`、本地 Codex 启动和本地终端启动都会拒绝 `workspace_root` 之外的目录；目录必须存在且是目录。

```http
GET /v2/workspaces
PUT /v2/workspaces
DELETE /v2/workspaces/{workspaceId}
```

`GET` 响应：

```json
{
  "workspaces": [
    {
      "id": "ws_1f45e78a20d5a33556417b12",
      "name": "demo",
      "path": "/home/user/projects/demo",
      "sessionId": "cdxs_demo",
      "tenantId": "local",
      "threadId": "",
      "model": "gpt-5.5",
      "reasoningEffort": "medium",
      "approvalPolicy": "on-request",
      "sandboxMode": "workspace-write",
      "serviceTier": null,
      "localAdapterState": "idle",
      "createdAt": 1700000000000,
      "updatedAt": 1700000000000
    }
  ],
  "updatedAt": 1700000001000
}
```

`PUT` 请求体使用同样的 `workspaces` 数组。后端会校验 `name`、`path`、路径存在性和根目录边界，按当前认证身份和规范化路径合并记录，并返回后端生成的稳定 ID。它不会接受客户端伪造的租户，也不会持久化设备本地的 `threadId` 和 `localAdapterState`。删除使用显式 `DELETE`，不会删除该工作区已有的对话历史。

### Workspace 目录浏览

```http
GET /v2/workspace/directories
GET /v2/workspace/directories?path=/home/user/projects/demo
```

响应：

```json
{
  "root": "/home/user/projects",
  "current": "/home/user/projects",
  "parent": null,
  "entries": [
    {
      "name": "demo",
      "path": "/home/user/projects/demo",
      "kind": "directory"
    }
  ]
}
```

`path` 为空时从当前 `workspace_root` 开始。返回值只包含可进入的子目录，会跳过隐藏目录、文件，以及 canonical path 落在 `workspace_root` 之外的目录或符号链接。

### 文件 `@` 引用建议

```http
GET /v2/workspace/entries?cwd=/home/user/projects/demo&query=routes&limit=40
```

响应返回 `entries` 数组（`name`、`path`、`kind`）。`query` 为相对路径片段，支持递归匹配（跳过 `node_modules`、`target`、`.git` 等大目录）；以 `/` 结尾时按目录直接列出。

### 文件预览

```http
GET /v2/workspace/file?path=/home/user/projects/demo/README.md
```

路径必须是 `workspace_root` 内的绝对路径且指向文件，超过 1 MiB 拒绝预览。响应包含 `name`、`path`、`mimeType`、`sizeBytes`，文本类型附带 `text` 内容。

### 浏览器代理

```http
POST /v2/browser/fetch
{"url": "https://example.com/page"}
```

仅允许 `http`/`https` URL；拒绝云元数据地址和非本后端的 loopback 目标。响应返回 `url`、`status`、`contentType`、`body`（≤2 MiB）。

## WebSocket 协议

客户端发送文本帧，内容必须是 JSON。二进制帧会被忽略。默认仍支持明文 JSON；如果 WebSocket URL 带上加密握手参数，业务 JSON 会被包装在 `todex.crypto.v1` 加密帧中。

连接示例：

```bash
websocat -H "Authorization: Bearer ${TODEX_AGENTD_AUTH_TOKEN}" ws://127.0.0.1:7345/v2/ws
```

无法设置 header 的客户端（Electron 原生 WebSocket、浏览器）可使用查询参数：`ws://127.0.0.1:7345/v2/ws?access_token=<url-encoded-token>`。服务端会先做 percent-decode 再比对，因此含 `&`、`=` 等保留字符的 token 也能通过该路径认证。注意查询参数可能进入反向代理日志，生产环境优先使用 header。

TUI 配对二维码携带后端地址、当前首选加密方式和服务端公钥。仅 loopback 监听时二维码携带 Bearer token；非 loopback 监听会省略长期 token，需通过独立可信通道录入。客户端每次连接必须生成新的 X25519 client key 或 ML-KEM ciphertext，服务端会拒绝当前进程生命周期内重复使用的握手材料。进程内最多登记 65,536 份已使用握手材料；达到上限后新加密握手会失败关闭，需要重启 daemon 清空登记表。

- X25519：客户端从配对信息读取服务端 X25519 公钥，连接 `ws://.../v2/ws?enc=x25519&client_key=<base64url-client-public-key>`。
- ML-KEM-768：客户端从配对信息读取服务端 ML-KEM-768 公钥，连接 `ws://.../v2/ws?enc=ml-kem-768&ciphertext=<base64url-kem-ciphertext>`。

兼容端点已移除：`/v1/ws` 与 `/v1/*` HTTP 不再注册，访问返回 404。旧客户端必须升级。
- 双方用 HKDF-SHA256 派生 32 字节会话密钥，并用 XChaCha20-Poly1305 加密每个业务文本帧。

消息 envelope：

```json
{
  "id": "req-1",
  "type": "codex.local.status",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local"
  }
}
```

本地 token 认证当前映射到租户 `local`。请求 payload 中的 `tenantId` 必须与认证上下文匹配。鉴权结果会写入 `$TODEX_AGENTD_DATA_DIR/audit/audit.jsonl`，并广播 `codex.audit` 事件。

### Codex 原生控制范围

本地 Codex 控制通过 `codex app-server --listen stdio://` 执行。后端把 app-server 的 newline-delimited JSON 请求、响应和通知映射为 typed WebSocket 事件，并通过 `CodexGatewayStore` 提供 cursor、replay、attach 和恢复能力。

不再支持旧的本地终端控制请求：`create_workspace`、`list_workspaces`、`attach_workspace`、`stop_workspace`、`create_window`、`list_windows`、`stop_window`、`agent_message`、`terminal_input`、`resize_pane`、`interrupt_pane`。这些消息不属于当前 `ClientMessageKind`，会在 JSON 解析阶段失败。

### 本地 adapter 生命周期

`codexSessionId` 是本地 adapter 的所有权边界。每个 session 最多拥有一个 `codex app-server` child process，mutating command 串行执行。

| 状态 | 含义 |
| --- | --- |
| `idle` | adapter 模型存在但尚未启动进程。 |
| `starting` | 正在启动进程或等待初始化完成。 |
| `ready` | 进程可用，命令通道空闲。 |
| `busy` | 有一个 in-flight mutating command。 |
| `waiting_for_approval` | 当前 command 正等待 approval/server-request 响应。 |
| `stopping` | 正在停止或清理进程。 |
| `stopped` | 已停止且不拥有进程。 |
| `failed` | 启动、运行或协议错误后进入失败状态。 |

并发 mutating command 默认拒绝，不排队。

## 统一 WebSocket 命令面

`/v2/ws` 是唯一的 WebSocket 端点，同时承载两类命令，消息 envelope 一致（`{id, type, payload}`）：

1. **v2 原生命令**：`conversation.*`、`server.ping`、`session.resume`，以 `server.result` / `server.error` envelope 应答。
2. **本地控制命令**（原 `/v1/ws` 能力）：`terminal.*`、`codex.local.*`、`codex.gateway.control`、`codex.mcp.*`、`codex.cloudTask.*`。应答与事件通过按连接隔离的 ServerEvent 流返回（见「事件」一节），连接只会收到自己触碰过的 Codex session / 终端的事件。

单帧上限 8 MiB（聊天附件以 base64 data URL 传输，无出站分片）。服务端每 30 秒发送 WebSocket Ping，90 秒无入站帧即关闭连接。

### `session.resume`（断线恢复）

连接建立后发送，携带客户端持久化的 Codex session cursor，服务端为仍存在的 session 授予事件可见性并重放 cursor 之后的事件（每 session 最多 80 条，最多 12 个 session）。替代已删除的 transport hello 握手：

```json
{
  "id": "resume-1",
  "type": "session.resume",
  "payload": { "sessionCursors": { "cdxs_local_1": 100 } }
}
```

## codex.local 请求

以下命令在 `/v2/ws` 上发送，属于本地 Codex 控制面。

### `codex.local.start`

启动本地 Codex app-server。

```json
{
  "id": "local-start-1",
  "type": "codex.local.start",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "cwd": "/home/user/projects/demo",
    "model": "gpt-5.5",
    "approvalPolicy": "on-request",
    "sandboxMode": "workspace-write",
    "configOverrides": {}
  }
}
```

成功事件包括 `codex.control.starting` 和 `codex.control.ready`。失败事件为 `codex.control.error`。

### `codex.local.status`

查询 live adapter 状态；如果没有 live handle，会从持久化事件中恢复最近状态。

```json
{
  "id": "local-status-1",
  "type": "codex.local.status",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local"
  }
}
```

### `codex.local.stop`

停止并清理本地 adapter。

```json
{
  "id": "local-stop-1",
  "type": "codex.local.stop",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "force": false
  }
}
```

### `codex.local.turn`

向 app-server 发送 `turn/start`。

```json
{
  "id": "local-turn-1",
  "type": "codex.local.turn",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "threadId": "thread_1",
    "input": [{ "type": "text", "text": "检查当前项目" }],
    "collaborationMode": {
      "mode": "default",
      "settings": {
        "model": "gpt-5.5",
        "developerInstructions": null
      }
    }
  }
}
```

### `codex.local.input`

向正在运行的 turn 追加 input。

```json
{
  "id": "local-input-1",
  "type": "codex.local.input",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "threadId": "thread_1",
    "turnId": "turn_1",
    "input": [{ "type": "text", "text": "继续" }]
  }
}
```

### `codex.local.steer`

向 app-server 发送 `turn/steer`。

```json
{
  "id": "local-steer-1",
  "type": "codex.local.steer",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "threadId": "thread_1",
    "turnId": "turn_1",
    "expectedTurnId": "turn_1",
    "input": [{ "type": "text", "text": "改用只读检查" }]
  }
}
```

### `codex.local.interrupt`

中断指定 thread 当前 turn。

```json
{
  "id": "local-interrupt-1",
  "type": "codex.local.interrupt",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "threadId": "thread_1",
    "turnId": "turn_1"
  }
}
```

### `codex.local.approval.respond`

响应 app-server 发出的 approval/server-request。

```json
{
  "id": "local-approval-1",
  "type": "codex.local.approval.respond",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "requestId": "approval_1",
    "responseType": "codex.approval.commandExecution.respond",
    "response": { "decision": "accepted" }
  }
}
```

### `codex.local.request`

发送通用 app-server JSON-RPC 方法。用于当前 typed wrapper 尚未覆盖但属于本地 app-server 的方法。

`thread/start` 必须使用与生产适配器一致的 canonical 最小参数（字符串 `approvalPolicy` / `sandbox`）；旧版 CLI 的 granular approval map 与 permission profile 已被移除，会被 app-server 以 `-32600` 拒绝：

```json
{
  "id": "local-request-1",
  "type": "codex.local.request",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "method": "thread/start",
    "params": {
      "cwd": "/home/user/projects/demo",
      "approvalPolicy": "on-request",
      "sandbox": "workspace-write"
    }
  }
}
```

### `codex.local.replay`

按 cursor 重放指定 session 的持久化 Codex 事件。

```json
{
  "id": "local-replay-1",
  "type": "codex.local.replay",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "afterCursor": 100,
    "limit": 200
  }
}
```

### `codex.local.attach`

附加到已有 session，并重放最近事件。

```json
{
  "id": "local-attach-1",
  "type": "codex.local.attach",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "afterCursor": 100,
    "replayLimit": 200
  }
}
```

### `codex.local.snapshot`

返回 display-only snapshot。当前实现不读取终端缓冲区，也不从外部屏幕文本推断状态；`authoritative` 固定为 `false`，`text` 当前为空字符串。

```json
{
  "id": "local-snapshot-1",
  "type": "codex.local.snapshot",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "maxBytes": 65536
  }
}
```

### `codex.local.unsupported`

客户端显式记录某个本地不支持的操作。

```json
{
  "id": "local-unsupported-1",
  "type": "codex.local.unsupported",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "operation": "codex.cloudTask.create",
    "reason": "cloud task is not local Codex control"
  }
}
```

## 事件

所有服务端事件使用统一格式：

```json
{
  "time": "2026-05-09T00:00:00Z",
  "event_id": "evt_...",
  "type": "codex.control.ready",
  "workspace_id": null,
  "window_id": null,
  "pane_id": null,
  "payload": {
    "cursor": 1,
    "codex_session_id": "cdxs_local_1",
    "data": {}
  }
}
```

常见事件：

| 事件 | 说明 |
| --- | --- |
| `codex.audit` | 鉴权/租户决策审计。 |
| `codex.control.starting` | 本地 adapter 开始启动。 |
| `codex.control.ready` | app-server 初始化完成。 |
| `codex.control.status` | 状态查询结果。 |
| `codex.control.stopping` | adapter 开始停止。 |
| `codex.control.stopped` | adapter 已停止。 |
| `codex.control.request.accepted` | 本地请求已进入 adapter。 |
| `codex.control.request.rejected` | 请求因鉴权、状态或协议原因被拒绝。 |
| `codex.control.error` | 本地 Codex 控制错误。 |
| `codex.local.snapshot` | 非权威显示快照。 |
| `codex.item.*` | app-server item/stream 通知映射。 |
| `codex.plan.*` | app-server plan 通知映射。 |
| `codex.approval.*` | app-server approval/server-request 映射。 |
| `error` | 通用后端错误事件。 |

## 错误码

| code | 说明 |
| --- | --- |
| `INVALID_REQUEST` | JSON 格式、字段或消息类型不符合当前协议。 |
| `UNAUTHENTICATED` | 配置了 token 但未提供有效 Bearer header 或 `access_token` 查询参数。 |
| `UNAUTHORIZED` | tenant 与认证上下文不匹配。 |
| `UNSUPPORTED` | 请求能力不在当前后端支持范围。 |
| `EVENT_STREAM_LAGGED` | WebSocket 事件接收端落后。 |
| `EVENT_STREAM_CLOSED` | 事件流已关闭。 |
| `SERIALIZATION_FAILED` | JSON 序列化失败。 |
| `IO_ERROR` | 文件或进程 I/O 错误。 |
| `INTERNAL_ERROR` | 未分类内部错误。 |

本地 Codex typed error payload 使用：

| code | 说明 |
| --- | --- |
| `MISSING_BINARY` | 找不到 configured Codex binary。 |
| `PERMISSION_DENIED` | 启动或访问权限不足。 |
| `INVALID_CWD` | `codex.local.start.cwd` 不存在或不是目录。 |
| `STARTUP_TIMEOUT` | app-server 初始化超时。 |
| `MALFORMED_EVENT` | app-server 输出无法解析为预期事件。 |
| `UNSUPPORTED_ACTION` | 当前状态或方法不支持该操作。 |
| `UNSUPPORTED_LOCAL` | 操作不属于本地 Codex 控制范围。 |
| `SESSION_BUSY` | 同一 session 正在执行其他 mutating command。 |
| `ADAPTER_CRASH` | child process 或结构化通道异常退出。 |
