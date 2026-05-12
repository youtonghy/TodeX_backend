# TodeX Backend API 调用文档

本文档基于当前代码实现整理。当前控制面已经切换为 Codex 原生 app-server 通道，WebSocket 只暴露 `codex.local.*` 和保留的 `codex.*` 网关兼容消息；旧的本地终端 workspace/window/pane 控制请求已从协议中移除。

## 基本信息

- 服务名称：`todex-agentd`
- 默认监听地址：`127.0.0.1:7345`
- 默认 HTTP Base URL：`http://127.0.0.1:7345`
- 默认 WebSocket URL：`ws://127.0.0.1:7345/v1/ws`
- 可选传输加密：`x25519` 或 `ml-kem-768`，由 TUI 配对二维码和 `/v1/pairing` 暴露的服务端公钥协商
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
| 默认 agent 名称 | 无 | `TODEX_AGENTD_DEFAULT_AGENT` | `codex` |
| 是否开启认证 | 无 | `TODEX_AGENTD_ENABLE_AUTH` | `true` |
| Bearer token | 无 | `TODEX_AGENTD_AUTH_TOKEN` | 无 |

当前 HTTP 层没有实现 TLS 终止；生产环境不应直接暴露明文端口到公网。`TODEX_AGENTD_AUTH_TOKEN` 是 WebSocket Bearer token 来源。所有 WebSocket 业务消息都必须在握手时提供 `Authorization: Bearer <TODEX_AGENTD_AUTH_TOKEN>`。如果服务端没有配置 token，WebSocket 握手仍可建立，但业务消息会返回 `UNAUTHENTICATED`。

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
GET /v1/version
```

响应字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | Cargo 包名 |
| `version` | string | Cargo 包版本 |
| `data_dir` | string | 当前数据目录 |
| `workspace_root` | string | 当前 workspace 根目录 |

### App 配对信息

```http
GET /v1/pairing
Authorization: Bearer <TODEX_AGENTD_AUTH_TOKEN>
```

返回后端地址、Bearer token、当前首选加密方式和可用传输加密公钥。TUI 中的配对二维码编码短配对链接、Auth token 和首选加密方式；App 扫描后自动调用此接口拉取完整公钥并一次性导入连接配置。

响应字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `kind` | string | 固定为 `todex-pairing` |
| `version` | number | 配对负载版本，当前为 `1` |
| `serverUrl` | string | App 使用的 HTTP Base URL |
| `wsUrl` | string | App 使用的 WebSocket URL |
| `authToken` | string/null | 当前 Bearer token |
| `preferredEncryption` | string | 当前 TUI 设置的首选加密方式：`none`、`x25519` 或 `ml-kem-768` |
| `protocols[].id` | string | `x25519` 或 `ml-kem-768` |
| `protocols[].publicKey` | string | base64url 编码的服务端公钥 |

## WebSocket 协议

客户端发送文本帧，内容必须是 JSON。二进制帧会被忽略。默认仍支持明文 JSON；如果 WebSocket URL 带上加密握手参数，业务 JSON 会被包装在 `todex.crypto.v1` 加密帧中。

连接示例：

```bash
websocat -H "Authorization: Bearer ${TODEX_AGENTD_AUTH_TOKEN}" ws://127.0.0.1:7345/v1/ws
```

可选加密握手：

- X25519：客户端从配对信息读取服务端 X25519 公钥，连接 `ws://.../v1/ws?enc=x25519&client_key=<base64url-client-public-key>`。
- ML-KEM-768：客户端从配对信息读取服务端 ML-KEM-768 公钥，连接 `ws://.../v1/ws?enc=ml-kem-768&ciphertext=<base64url-kem-ciphertext>`。
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

## codex.local 请求

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

```json
{
  "id": "local-request-1",
  "type": "codex.local.request",
  "payload": {
    "codexSessionId": "cdxs_local_1",
    "tenantId": "local",
    "method": "turn/start",
    "params": {}
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
| `UNAUTHENTICATED` | 未提供有效 Bearer token，或服务端未配置 token。 |
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
