# TodeX Backend API 调用文档

本文档基于当前代码实现整理。TodeX 2.0 的主控制面是 `/v2/conversations` 与 `/v2/ws`，统一支持 ACP、Codex、Pi、Claude Code 和 Grok Build。所有 `/v1/*` 接口已移除：旧 `/v1/ws` 的终端、本地 Codex 控制、Cloud Code、MCP 和事件流能力已并入 `/v2/ws`，旧 HTTP 资源接口已迁至 `/v2/*`。

## 基本信息

- 服务名称：`todex-agentd`
- 默认监听地址：`127.0.0.1:7345`
- 默认 HTTP Base URL：`http://127.0.0.1:7345`
- 默认 WebSocket URL：`ws://127.0.0.1:7345/v2/ws`
- 外部 API（API key 鉴权，默认关闭）：`http://127.0.0.1:7346/api/v1`，见 [外部 API（API Key）](#外部-apiapi-key)
- 传输加密：transport v2（[transport-v2.md](transport-v2.md)），`x25519` 或 `ml-kem-768`，服务端公钥由设备配对交付（验证码认证）；WebSocket 用 `tv=2` 握手，REST 走 `POST /v2/sealed` 隧道
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

`data_dir` 采用两阶段读取：命令行或环境变量显式指定时直接作为最终目录；否则先读取默认目录的 `config.toml`，允许其中的 `data_dir` 重定向一次，再合并最终目录中的配置。相对路径以声明它的配置文件目录为基准。最终配置再次指向第三个目录会拒绝启动，避免循环或含混的配置来源。

常用配置项：

| 配置 | 命令行参数 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| 监听主机 | `--host` | `TODEX_AGENTD_HOST` | `127.0.0.1` |
| 监听端口 | `--port` | `TODEX_AGENTD_PORT` | `7345` |
| 数据目录 | `--data-dir` | `TODEX_AGENTD_DATA_DIR` | `~/.todex-agent` |
| Workspace 根目录 | `--workspace-root`（可重复指定多个） | `TODEX_AGENTD_WORKSPACE_ROOT`、`TODEX_AGENTD_WORKSPACE_ROOTS`（按平台路径分隔符分隔多个） | `~/projects` |
| Codex 可执行文件 | 无 | `TODEX_AGENTD_CODEX_BIN` | `codex` |
| Claude Code 可执行文件 | 无 | `TODEX_AGENTD_CLAUDE_BIN` | `claude` |
| Pi 可执行文件 | 无 | `TODEX_AGENTD_PI_BIN` | `pi` |
| Grok Build 可执行文件 | 无 | `TODEX_AGENTD_GROK_BIN` | `grok` |
| Grok Build 认证方法 | 无 | `TODEX_AGENTD_GROK_AUTH_METHOD` | 上游默认方法 |
| Grok Build 环境白名单 | 无 | `TODEX_AGENTD_GROK_ENV_ALLOWLIST` | Grok 配置变量与 `XAI_API_KEY` |
| Devin 可执行文件 | 无 | `TODEX_AGENTD_DEVIN_BIN` | `devin` |
| Devin 认证方法 | 无 | `TODEX_AGENTD_DEVIN_AUTH_METHOD` | 上游默认方法 |
| Devin API key 环境变量 | 无 | `TODEX_AGENTD_DEVIN_API_KEY_ENV` | 无（回退到 CLI 登录凭据） |
| Devin CLI 凭据回退 | 无 | `TODEX_AGENTD_DEVIN_CLI_CREDENTIALS` | `true` |
| Devin 环境白名单 | 无 | `TODEX_AGENTD_DEVIN_ENV_ALLOWLIST` | `DEVIN_API_KEY`、`DEVIN_MODEL`、`WINDSURF_API_KEY` 等 |
| OpenCode 可执行文件 | 无 | `TODEX_AGENTD_OPENCODE_BIN` | `opencode` |
| OpenCode 环境白名单 | 无 | `TODEX_AGENTD_OPENCODE_ENV_ALLOWLIST` | `OPENCODE_CONFIG*`、`OPENCODE_AUTH_CONTENT`、`OPENCODE_API_KEY` 等 |
| Antigravity 可执行文件 | 无 | `TODEX_AGENTD_ANTIGRAVITY_BIN` | `agy` |
| Antigravity 环境白名单 | 无 | `TODEX_AGENTD_ANTIGRAVITY_ENV_ALLOWLIST` | `GEMINI_API_KEY`、`GOOGLE_GEMINI_BASE_URL`、`GOOGLE_CLOUD_PROJECT`、`GOOGLE_APPLICATION_CREDENTIALS` |
| DeepSeek Harness 可执行文件 | 无 | `TODEX_AGENTD_DSH_BIN`（`[agent] dsh_bin`） | `dsh` |
| DeepSeek Harness 环境白名单 | 无 | `TODEX_AGENTD_DSH_ENV_ALLOWLIST`（`[agent] dsh_env_allowlist`） | `DSH_HOME`、`DEEPSEEK_API_KEY`、`DEEPSEEK_BASE_URL`、`DEEPSEEK_SEARCH_BASE_URL` |
| omp 可执行文件 | 无 | `TODEX_AGENTD_OMP_BIN`（`[agent] omp_bin`） | `omp` |
| omp 环境白名单 | 无 | `TODEX_AGENTD_OMP_ENV_ALLOWLIST`（`[agent] omp_env_allowlist`） | 空 |
| OpenSSH 客户端 | 无 | `TODEX_AGENTD_SSH_BIN`（`[agent] ssh_bin`） | `ssh`（`ssh-add` 取同目录或 PATH） |
| 默认 agent 名称 | 无 | `TODEX_AGENTD_DEFAULT_AGENT` | `codex` |
| 是否开启认证 | 无 | `TODEX_AGENTD_ENABLE_AUTH` | `true` |
| 历史保留天数 | `--history-retention-days` | `TODEX_AGENTD_HISTORY_RETENTION_DAYS` | 关闭 |

当前 HTTP 层没有实现 TLS 终止，配置 `enable_tls = true` 时服务会拒绝启动，避免产生“已经启用 TLS”的错误安全假设。生产环境应在可信反向代理终止 TLS，且不应直接暴露明文端口。v2 HTTP 和 WebSocket 都使用设备签名认证：每个请求携带 `x-todex-device-id`、`x-todex-auth-ts`、`x-todex-auth-nonce`、`x-todex-auth-sig` 四个 header（无法设置 header 的客户端使用等价 query 参数 `device_id`、`auth_ts`、`auth_nonce`、`auth_sig`），签名覆盖方法、路径、canonical query、时间戳、nonce 与 body 哈希，见 [设备验证](device-verification.md)。签名用 Ed25519 严格校验（`verify_strict`）。时间戳与服务端时钟相差超过 300 秒，或早于 daemon 本次启动时刻（重启会清空 nonce 缓存，启动栅栏阻止旧凭证重放），返回 401 `{"code":"AUTH_TIMESTAMP_REJECTED","message":"...","serverTime":<unix 秒>}`（`serverTime` 与 `code`/`message` 并列在顶层，仅此错误带该字段）；REST 客户端据此按后端记录 `offset = serverTime − 本地时间`，用新 nonce 重签并重试一次，WebSocket 只靠退避重连。其他认证失败仍是 `UNAUTHENTICATED`。nonce 在窗口内一次性有效，每台设备窗口内最多 8192 个，超出只对该设备返回 429 `RATE_LIMITED`（`Retry-After: 1`）；全局 65536 条作为最后防线（429 `RESOURCE_EXHAUSTED`）。WebSocket 升级使用同样的检查。conversation 持久化 owner tenant，所有读取、订阅与变更入口都会校验 tenant。

认证策略是 fail-closed 的：`enable_auth = true` 时匿名 `/v2/ws` 握手直接被拒绝（401），不存在“先连上再限制命令”的匿名模式；`enable_auth = false` 的本地部署才会以本地信任模式接受匿名连接。关闭认证时监听地址必须是回环地址（`127.0.0.1`、`::1`），否则 daemon 拒绝启动（TUI 也拒绝保存这种监听地址）；匿名请求（HTTP 与 `/v2/ws` 升级）的 `Host` 必须是 `localhost` 或回环 IP（可带端口），`Origin` 若存在须为回环主机的 `http(s)` 源或桌面端固定的 `todex-desktop://app`（桌面端把自身 `loadFile` 页面的 `null` / `file://` Origin 改写为它），否则返回 403 `UNAUTHORIZED`；`Origin: null` 与 `file://` 一律拒绝，因为任意网页都能用 sandbox iframe 产生 `null` Origin，用于防御 DNS rebinding 与跨站 WebSocket 劫持。开启认证时不做此检查。认证中间件先校验凭证（header 或 query 齐全、时间戳在窗口内、设备已登记）再读取请求体，缺少或无效凭证的请求在读取请求体前即返回 401。两个监听（设备与 `/api/v1`）都只说 HTTP/1.1（含 WebSocket 升级），请求头须在连接建立或上一个响应之后 10 秒内收完，否则连接被关闭；每个监听同时最多服务 1024 个连接，更多的连接在内核队列中等待。客户端可另带 `x-todex-content-sha256`（请求体 SHA-256 的 base64url、无填充，即签名覆盖的那个值）：带了它时先按该值验签并占用 nonce，再读请求体并比对，签名或请求体不符都返回 401 `UNAUTHENTICATED`，签名错误的请求不会读入请求体；值不是 32 字节的 base64url 时同样返回 401。不带该头时（旧客户端），签名覆盖请求体，因此验签前必须先读入请求体：GET/HEAD（含 `/v2/ws` 升级）带请求体时返回 `400 INVALID_REQUEST`；请求体超过 32 MiB 返回 `413 PAYLOAD_TOO_LARGE`；所有尚未验签的请求体合计最多缓冲 256 MiB（按 `Content-Length` 计，长度未知按 32 MiB 计），等待额度超过 10 秒返回 `503 TRANSPORT_BUSY`（`Retry-After: 1`，请求未执行）；请求体须在 60 秒内收完，否则返回 `408 REQUEST_TIMEOUT`。没有可配置的共享凭据：设备只能通过配对流程登记（见 `POST /v2/device-pairing/*`）。

非回环对端只能经 transport v2 访问 API（见 [transport-v2.md](transport-v2.md)）：直接可达的只有 `/health`、`/v2/transport-policy`、`/v2/version`、`/v2/device-pairing/*`、`/v2/sealed` 和带 `tv=2` 的 `/v2/ws`，其余请求（包括未知路径）返回 426 `PROTOCOL_UPGRADE_REQUIRED`；经 `/v2/sealed` 隧道的内层请求视为已走 v2。浏览器页面跨域读取 daemon 响应（CORS）只对三类 `Origin` 放行：桌面端的 `todex-desktop://app`、任意回环主机的 `http(s)://` 页面（本机 web 客户端与开发服务器），以及 `security.cors_origins`（或 `TODEX_AGENTD_CORS_ORIGINS`，逗号分隔，按字符串精确匹配，如 `https://todex.example.com`）列出的 origin；回环监听时 Private Network Access 预检同样只对这些 origin 放行。其他页面的预检得不到允许头，因此无法发起带签名的请求或配对；直连 daemon 的自建 web 客户端需把站点 origin 加入 `cors_origins`（经 web 服务端代理的托管模式不受影响）。回环对端仍可使用明文 REST 与 WebSocket，但带 `Forwarded`、`X-Forwarded-For` 或 `X-Real-IP` 的请求视为经反向代理转发（同机代理会让远端客户端显示为回环），按非回环对端处理；`enable_auth = false` 时这类请求一律返回 403。`security.trusted_proxies`（或 `TODEX_AGENTD_TRUSTED_PROXIES`，逗号分隔的 IP）列出的代理转发的请求，以 `X-Forwarded-For` 最右一项作为客户端地址，用于限流与配对审批显示；不在列表中的代理不采信该头。监听非回环地址时 `pairing_encryption` 不能为 `none`，否则 daemon 拒绝启动（TUI 也拒绝保存这种组合）。

## v2 Conversation API

Provider 标识为 `acp`、`codex`、`pi`、`claude-code`、`grok-build`、`devin`、`opencode`、`antigravity`、`deepseek-harness`、`omp`（创建请求也接受 `grok` / `grok_build`、`devin-cli` / `devin_cli`、`open-code` / `open_code`、`agy` / `antigravity-cli`、`dsh` / `deepseek_harness` 与 `oh-my-pi` / `oh_my_pi` 别名）。未指定时使用 `[agent].default_agent`，默认是 `codex`。ACP 必须使用后端 `config.toml` 中预配置的 `providerProfile`；客户端不能提交任意 command、args 或 env。ACP profile 的每个对话默认对应一个常驻 ACP 进程（空闲超过 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）后回收，daemon 级上限 32 个）；profile 设 `resident = false` 时恢复为每个 turn 启动一个进程。代理在 `initialize` 声明 `sessionCapabilities.resume` 时，新进程以不回放历史的 `session/resume` 恢复会话，否则使用 `session/load`。对所有常驻 ACP 进程（ACP profile、Grok Build、Devin、OpenCode、DeepSeek Harness、omp）：被取消的 turn 若在 3 秒内收到 `session/prompt` 的 `stopReason: "cancelled"` 应答且没有未决请求与 live control，进程保留给下一 turn，否则丢弃；`session/set_config_option`（model / effort / mode）只在会话当前报告的值与请求不同时下发。ACP profile 的 fork 与图片能力来自同一次 `initialize` 探测，按 profile 缓存 5 分钟。

Provider 子进程只继承运行所需的基础系统环境；ACP 额外使用管理员在 profile 中明确配置的 env。Codex、Pi、Claude Code 和 Grok Build 应先由运行 daemon 的同一系统用户完成原生登录。Grok Build 也可通过白名单传入 `XAI_API_KEY`；daemon 不会启动浏览器/OIDC 交互认证。Grok 的工具授权、提问、计划审批和 MCP elicitation 会转换为 TodeX `permission.requested`，客户端按服务器提供的 `optionId` 和 `kind` 回复。Codex 为 MCP 工具调用（含其自带 Computer Use 的 `cua_repl`）发起的审批 elicitation（`_meta.codex_approval_kind: "mcp_tool_call"`）以 `kind: "tool"` 发出，details 保留原始请求并补充 `tool_name` 与 `input`；选项为 `allow_once`、`reject_once`，Codex 在 `_meta.persist` 中提供 `session` 时另有 `allow_always`（回复 `_meta: { persist: "session" }`，本 Codex 会话内同一工具不再询问，app-server 重启后失效；不提供会写入用户 `config.toml` 的 `always`）。其他 elicitation 仍以 `kind: "elicitation"` 表单发出。Claude Code 的 `AskUserQuestion` 以 `kind: "user_input"` 的 `permission.requested` 发出（问题 id 为 `q0`、`q1`…，携带 `options`、`multiSelect` 与 `isOther: true`），客户端以 `answer` 回复 `{ "answers": { "q0": { "answers": ["..."] } } }`，后端按问题原文回填到 Claude 的 `updatedInput.answers`（多选以 `, ` 连接，可填自定义文本）；选择 `reject_once`（Skip）则拒绝该工具。Claude 的其他工具授权（`can_use_tool`）以 `kind: "tool"` 发出，details 保留原始请求，并把 `input.command` 与 `decision_reason` 提升为与 Codex 一致的顶层 `command` / `reason`；`decision_reason_type: "safetyCheck"` 表示 Claude Code 内置安全检查（如对可能为空的变量执行 `rm`），即使 `bypassPermissions`（完全访问）也会询问。选项为 `allow_once`、`reject_once`；能为该请求构造会话规则时另有 `allow_always`（“Allow for session”），回复后 TodeX 在 `can_use_tool` 应答中附带 `updatedPermissions: [{ type: "addRules", rules: [<规则>], behavior: "allow", destination: "session" }]`，规则只存在于该 Claude 进程内，不写任何 settings 文件，进程重启后失效。规则由 TodeX 自行构造，不回传 Claude 的 `permission_suggestions`（其 Bash 建议会写入 `.claude/settings.local.json`，文件建议会把整个会话切到 `acceptEdits`）：Bash 为完整命令的精确规则，仅限不含 `;`、`&`、`|`、`<`、`>`、`` ` ``、`$`、`*` 与换行的单条命令（Claude 对复合命令、重定向逐段检查，整条命令的规则不会命中，因此不提供该选项；`*` 在 Bash 规则里是通配符，结尾的 `:*` 是前缀匹配，会让规则放行其他命令）；Edit/Write/MultiEdit 为该文件的 `Edit(//<绝对路径>)`（相对路径按会话工作区解析；同一规则也覆盖 Write），NotebookEdit 为 `NotebookEdit(//<绝对路径>)`，路径含 `.`/`..` 段或通配字符时不提供；WebFetch 为 `domain:<host>`；`mcp__` 工具为不带内容的工具级规则；其他工具不提供。Claude 的工具事件以 `block.category: "tool"`、`block.id` 为 `tool_use` id 发出，同一调用的开始、参数、进度与结果合并为一张卡片。Pi 只有全自动执行模式：以 `--approve` 启动，工具与项目扩展按 daemon 用户权限运行。Pi 没有通用逐工具审批或 OS sandbox，因此 `permissions` capability 为 `false`；extension UI 请求仍可转成交互事件，但不能把它等同于工具授权。

Devin 通过 `devin acp` 接入，每个对话对应一个常驻 ACP 进程（空闲超过 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）后回收；CLI 声明 `sessionCapabilities.resume` 时以 `session/resume` 恢复，否则 `session/load`）。`devin acp` 自身有意不读取本地 CLI 登录态，daemon 会在每个 ACP 进程启动时执行 `authenticate`：优先以 `devin_api_key_env` 指定的环境变量值作为 `_meta.api_key` 无头认证（不会写入日志），未配置时默认回退读取 `devin auth login` 写入的 `~/.local/share/devin/credentials.toml` 中的 `windsurf_api_key`（`TODEX_AGENTD_DEVIN_CLI_CREDENTIALS=false` 可禁用），再否则按其声明的 `devin-browser` 方法走一次浏览器授权。权限模式只提供两档（Devin 没有“自动审批”中间档）：`ask`（手动审批）→`accept-edits`、`full-access`（完全访问）→`bypass`，plan 工作模式→`plan`；存量 `auto` 选择会降级为 `ask` 而不是报错。会话模式每轮都重新下发，未指定权限模式的轮次（如 `/api/v1` 的 run）按 `ask`（`accept-edits`）处理，不会沿用常驻进程上一轮 `full-access` 留下的 `bypass`。注意 Devin 原生的 `ask` 会话模式是只读问答，并不等价于“逐项审批”，因此未直接暴露。模型目录与 slash 命令通过真实 ACP 会话探测（探测会话随后调用 `session/delete` 清理，结果按工作区缓存约 5 分钟，避免客户端刷新反复触发认证）。Devin 支持会话内切换模型（`configure` live control）；reasoning effort 经 `configOptions.thought_level` 下发——Devin 只对当前模型暴露该选项且各模型档位集合不同，模型目录探测会逐个模型读取其 `supportedReasoningEfforts`（在同一 ACP 进程内并行开 8 个探测会话分摊，95 个模型约 4 秒；整轮上限 15 秒，超时未探到的模型档位留空，该结果只缓存约 1 分钟，之后的查询再补齐），不暴露该选项的模型（如 `claude-opus-4-6`）视为不支持调整思考强度。

OpenCode 通过 `opencode acp` 接入，每个对话对应一个常驻 ACP 进程（空闲超过 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）后回收，daemon 级上限 32 个）。OpenCode 自己管理登录态（`opencode auth login` 写入其本地凭据存储），其声明的 `opencode-login` authMethod 仅指向该终端流程，daemon 因此不发送 ACP `authenticate`；未登录时模型调用错误原样上浮。权限模式提供 `ask`/`auto`/`full-access`：`ask` 把每个 `session/request_permission` 转成 TodeX `permission.requested` 逐条询问；`auto`/`full-access` 由 daemon 客户端侧自动应答（都优先 `allow_once`：`allow_always` 会被常驻会话记住，之后的 `ask` 轮次将不再询问；只提供 `allow_always` 的请求在 `auto` 下交给用户，`full-access` 下才选它），自动批准同样记录 `permission.requested`/`permission.resolved`（`autoApproved: true`）事件。OpenCode 侧 `opencode.json` 中的 `permission` 配置（或白名单转发的 `OPENCODE_PERMISSION`）可让其原生策略先行放行。plan 工作模式经 `configOptions.mode` 映射到原生 `plan`，implement 显式重置为 `build`，避免上一轮 plan 残留。模型经 `configOptions.model` 选择（`provider/model` 形式），会话内支持 `configure` live control 切换；reasoning effort 经 `configOptions.effort` 下发——OpenCode 只在当前模型有 variant 时暴露该选项，模型目录探测会逐个模型读取其 `supportedReasoningEfforts`。OpenCode 声明 `loadSession` 与 `sessionCapabilities.resume`/`fork`，daemon 恢复对话时优先使用不重放历史的 `session/resume`（不支持时回退 `session/load`），会话分叉经 `session/fork` 完成；`session/list` 暂无消费方，未接入。Usage 来自 `usage_update`（`used`/`size`/`cost`）与 prompt 响应 `result.usage`（含 `cachedReadTokens`）。模型目录与 slash 命令（`available_commands_update`）通过同一次真实 ACP 会话探测获得，探测会话随后调用 `session/close` 清理；结果按工作区缓存约 5 分钟（有模型未应答时约 1 分钟），同时到达的模型与命令查询共用一次探测，发现总时限为 20 秒。OpenCode 的 skills 目录（`.opencode/skills`、`~/.config/opencode/skills`）进入 catalog；MCP 由 OpenCode 自己的 `opencode.json` 管理，不参与 TodeX catalog 解析。

Antigravity 通过 `agy` 的 print 模式接入（`--input-format stream-json --output-format stream-json`），没有 ACP：每个 turn 启动一个进程，发送一条 `{"event":"user"}` 后关闭 stdin，让 `agy` 自行落盘会话历史再退出（10 秒内未退出则停止）。`init` 返回的 `conversation_id` 立即写入 `nativeSessionId`，下一 turn 以 `--conversation` 续接；记录的会话已不存在时 `agy` 会新开会话，daemon 发出 `providerMethod: "session/recreated"`。登录态由 `agy` 自己管理（在终端运行 `agy` 完成 Google 登录），也可经环境白名单传入 `GEMINI_API_KEY`。headless `agy` 无法请求审批（需要审批的工具被拒绝，hook 回答 `allow` 也不能放行），因此 `ask`/`full-access` 的 implement turn 以 `--dangerously-skip-permissions` 运行，由 TodeX 写入 `~/.gemini/config/hooks.json` 的全局 `PreToolUse` hook（`todex-approval`，运行同目录的 `todex-agy-hook.sh`/`.cmd`，再执行 `todex-agentd agy-hook`）逐个工具询问 daemon 的 `/internal/agent-mcp/agy-permission`（仅回环、会话 token）：`ask` 下 TodeX 自有 MCP 工具与读取类工具直接放行——其中带文件路径的读取（`view_file`、`list_dir`、`grep_search`、`find_by_name`）仅当每个路径参数规范化（解析符号链接、逐组件比较）后都位于会话工作区、本会话的图片暂存目录、`agy` 自带文档（`~/.gemini/antigravity-cli/builtin`）或本会话的 artifact 目录内时才直接放行，否则（含无法识别路径）与编辑一样询问（Plan turn 中同样询问而非拒绝）；编辑、命令、联网/浏览器、其他 MCP 与未知工具发出 `kind: "tool"` 的 `permission.requested`（选项为允许一次、本会话总是允许该工具、拒绝；`details` 带 `tool_name`、`input`、`command`）；`full-access` 全部放行；`auto` 使用 `agy` 自己的 Auto 审核（按账号开放的预览功能）：以 `--mode auto` 启动、不带 `--dangerously-skip-permissions`，并在 `agy` 进程环境中设置 `TODEX_AGY_NATIVE_AUTO=1`（只有值恰为 `1` 才算；其他启动方式会显式移除该变量，各 Provider 的环境白名单也不会转发任何 `TODEX_` 前缀的变量），hook 据此（且仅当本 turn 记录的权限模式确为 `auto` 时）对非读取类工具不作回答，由 `agy` 自行判断——它判定安全的直接运行，其余在 headless 下被拒绝（不弹 `permission.requested`）；工作区内的读取与 TodeX 自有 MCP 工具仍由 hook 放行，工作区外（或无法识别路径）的读取同样交给 `agy` 的 Auto 审核。hook 进程自身环境未标记 Auto 时，daemon 的“不作回答”一律按拒绝输出。此前保存的 Antigravity `auto` 选择（例如看板定时任务、旧版客户端）升级后同样使用 `agy` 的 Auto（不可用时按 `ask`），不再像旧版那样直接按 `ask` 运行。`agy` 在不支持 Auto 的账号上只在 stderr 警告并退回默认模式，因此 daemon 先在临时目录（不在工作区内，结果按账号）运行 `agy --mode auto -p=/help` 探测（约 10 秒，最长 30 秒；结果缓存 30 分钟，并发的 auto turn 共用一次探测；探测失败按不可用处理并缓存 2 分钟后重试）。探测期间取消 turn 会立即结束，不启动 `agy`、不发送提示。不可用时该 turn 按 `ask` 运行（包括 TodeX 自有 MCP 工具 `todex_ssh`/`todex_desktop`：有副作用的调用逐次审批）并发出 `providerMethod: "auto_mode_unavailable"`——`turn.started` 的 `effectivePermissions` 仍记录请求时的 `auto`，本 turn 实际按 `ask` 运行以该事件为准；若探测结果已过期、`agy` 在运行中报告不可用（daemon 在 stderr 输出到达时逐段扫描该警告，之后再多的输出也不会掩盖它），turn 结束后同样发出该事件并清除缓存，下一 turn 重新探测；无法判定（无运行中的 turn、超时、daemon 不可达）一律显式拒绝（空回答会交回 `agy` 自身权限，而 `--dangerously-skip-permissions` 已关闭它）。hook 或其所需环境无法安装时，`ask`（及按 `ask` 运行的 `auto`）turn 保留 `agy` 自带审批（需要审批的工具被拒绝）并发出 `providerMethod: "approval_bridge_unavailable"`。Plan turn 始终用 `--mode plan` 保留自带审批，只允许读取。TodeX 的 `todex_ssh`/`todex_desktop` 以静态条目写入 `~/.gemini/config/mcp_config.json`（`todex-agentd agent-mcp-bridge --route <route>`），每个 turn 经 `agy` 进程环境变量 `TODEX_AGENT_MCP_ENDPOINT`/`TODEX_AGENT_MCP_TOKEN`/`TODEX_AGENT_MCP_ROUTES` 指向本会话；在 TodeX 之外启动时，hook 脚本不输出任何内容并以 0 退出（`agy` 将其视为没有意见，按自身权限规则处理，与未安装 hook 相同），MCP 条目不提供工具。这些条目在每个 Antigravity turn 前按需更新（只改 TodeX 自己的键，保留其他内容，JSONC 注释不保留，文件改为仅属主可读写），`DELETE /v2/providers/antigravity/integration` 将其移除（有进行中的 Antigravity turn 时返回 `409`；下一个 turn 会重新写入）。模型目录来自 `agy models`：同一模型的 `-low`/`-medium`/`-high` 变体合并为一项，所选思考强度决定启动的变体 ID。图片输入按模型声明（`imageInputMode: "model"`；`gpt-oss` 模型为 `false`，其余为 `true`）：`agy` 的 stream-json 输入只接受文本块，因此工作区内的图片直接给出路径，粘贴的图片写入 `<data_dir>/antigravity-attachments/<conversationId>/`（保留供后续 turn 再次查看）并以 `--add-dir` 加入该目录，提示末尾逐张追加 `Attached image: <路径>`，由模型用 `view_file` 打开。暂不支持分叉与原生压缩。turn 因额度失败时，daemon 读取 `agy -p /usage` 并为当前模型所属额度组发出 `status: "rejected"` 的 `quota.updated`，交给额度续写流程。

Claude Code 通过 `claude -p --input-format stream-json --output-format stream-json` 接入，每个对话对应一个常驻进程（空闲超过 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）后回收，daemon 级上限 16 个）：只有首个 turn 启动进程并完成 `initialize` 握手，后续 turn 向同一进程的 stdin 写入新的 `user` 帧，turn 仍以 `session_state_changed: idle` 结束：Claude 只在它自己认为的输入队列与后台工作都清空后才发这一帧，因此某个它仍记作未完成的任务（子代理重启、它已经回答过的通知）会让 turn 一直挂着。daemon 因此给这帧设了上限：某个 `result` 到达后，30 秒内（仍有后台任务时为 10 分钟，与 Claude Agent SDK 自己的 `runEndCeilingMs` 一致）没有新输出也没有 `idle`，就以该 `result` 正常结束 turn，并在结束前记一条 `provider.event` `{ "provider": "claude-code", "providerMethod": "idle_timeout", "metadata": { "taskIds": [...], "tasks": [...] }, "turnId" }`；进程保留，Claude 之后自行开始的工作照常成为 provider 发起的 turn。任何新帧都会重新计时，`session_state_changed: requires_action`（等待用户操作）不计时。收到 `idle` 后本 turn 仍没有可用的 `result`（例如 `result` 行超过 4 MiB 被丢弃）时同样有上限：30 秒内既无新帧也无新的 `running`，用户 turn 以 `turn.failed`（`code: "PROVIDER_UNAVAILABLE"`，文案注明本 turn 丢弃的超长行数）结束，provider 发起的 turn 以 `stopReason: "no_work"` 正常结束。排在 Claude 自行开始的调用之后、Claude 一直没有回放的用户 prompt，按同样的 30 秒窗口最多重发 3 次，仍未取走则该 turn 失败（`Claude Code never took the queued prompt; please retry`）；发出 prompt 后出现过超长行时不重发而直接失败，因为被丢弃的可能正是它的回放，重发会让同一 prompt 出现两次。turn 以 `idle`（或不报告会话状态的旧版 CLI 以无后台任务的 `result`）结束时，主线程上仍没有结果的工具调用各补一条 `tool.completed`（`isError: true`，`result` 为说明文字，本 turn 有超长行时注明行数）；以上述 30 秒/10 分钟上限、失败或取消结束的 turn 不补，子代理内的工具调用也不补。这类结果若之后才到达，记在读到它的那个 turn 中，显示为单独的工具卡。turn 内每个 `session_state_changed` 也记一条 `provider.event`（`providerMethod: "session_state_changed"`，`metadata.state` 为 `idle`/`running`/`requires_action`），便于从会话记录诊断卡住的 turn。模型、推理强度、权限模式、注入的 agent MCP 配置、可执行文件、工作区、Claude 配置目录或 `settings.json` 内容变化时，daemon 在该 turn 前停止旧进程并以 `--resume` 重启；进程退出或协议出错后的下一个 turn 同样重启。两个 turn 之间 daemon 读取但不记录 stdout：`control_request`（如 `can_use_tool`）一律以 `TodeX session is idle` 错误应答，不由 Claude 自行开始的模型输出则停止该进程。`initialize` 握手期间读到的 `control_request` 以 `TodeX session is starting` 错误应答；turn 内 `can_use_tool` 以外的控制请求以 `control request is not supported by TodeX` 错误应答，因此每个控制请求都恰好得到一次应答。Claude 在 `can_use_tool` 等待期间被中断时会发 `control_cancel_request`（同一 `request_id`）：daemon 把对应的 `permission.requested` 以 `permission.resolved`（`outcome: "cancelled"`）关闭，且不再应答该请求。TodeX 不注册 SDK hook 回调（`initialize` 传 `hooks: null`），Claude 不会发 `hook_callback`；各 `settings.json` 层中配置的命令型 hooks 仍由 Claude Code 照常执行（PreToolUse hook 与 `permissions.deny` 在完全访问下同样可以拦截工具）。模型以后台方式运行的 Bash、`ScheduleWakeup` 与 `CronCreate` 在 turn 以 `idle` 结束后仍可能继续：会话还有未完成的后台任务、未到点的 wakeup 或 cron 任务时，进程不按 keepalive 回收，最长保留到上一个 turn 结束后 6 小时（wakeup 保留到其到点后 2 分钟）。这类工作完成或到点时 Claude 会自行开始一次调用（`session_state_changed: running`），daemon 为它开一个 **provider 发起的 turn**：不写 `message.created`，`turn.started` 额外带 `trigger: "provider"` 与 `triggerDetail`（`{ kind: "background_task", taskIds, taskType?, summary? }`、`{ kind: "wakeup" }`、`{ kind: "cron" }` 或 `{ kind: "unknown" }`），模型、推理强度与权限设置沿用上一个 turn（`requestedPermissions`、`effectivePermissions` 照常记录），`clientRequestId` 为 `null`；之后与普通 turn 一样记录输出、权限请求并以 `turn.completed` 等结束，可被取消，结束后照常派发追加队列。用户 turn 的 `turn.started` 不带 `trigger`。turn 以 `idle` 结束时若仍有后台任务，结束前先记一条 `provider.event` `{ "provider": "claude-code", "providerMethod": "background_tasks_pending", "metadata": { "taskIds": [...], "tasks": [{ "taskId", "description"?, "taskType"?, "toolUseId"?, "startedAt"? }] }, "turnId" }`，客户端据此在 turn 结束后显示“等待后台任务”，直到下一个 `turn.started`。进程在后台任务完成并交回之前被停止时（`reason`：`settings_changed` 启动设置变化后重启、`evicted` 让出进程名额、`stopped` 会话停止/删除或 daemon 关闭、`expired` 超过 6 小时保留、`not_adopted` 未能开出 provider 发起的 turn、`process_ended` 进程退出、`turn_ended` 所在 turn 失败或取消），daemon 记一条 `provider.event` `{ "provider": "claude-code", "providerMethod": "background_tasks_ended", "metadata": { "taskIds": [...], "reason" } }`（有 turn 在运行时带其 `turnId`）；daemon 未能正常关闭时，重启恢复会为最近 7 小时内更新、最后一次 `background_tasks_pending` 之后再无 turn 的 Claude Code 会话补记一条 `reason: "daemon_restarted"`、`taskIds` 为空的同名事件。客户端收到它即结束等待。会话已删除或只读、工作区目录已不可用，或 10 秒内未能开出该 turn 时，daemon 停止该进程（行为同之前：这次工作丢失，下一个 turn 以 `--resume` 重启）；若用户 prompt 恰好先于该 turn 到达，Claude 的这次工作与对 prompt 的回答都记在该用户 turn 中。取消 turn 时立即返回 `turn.cancelled`，随后发送 stream-json `interrupt` 控制请求；进程在 5 秒内回到 `idle` 则保留，否则停止。达到上限时停止空闲最久的进程（没有后台工作的进程优先）；16 个进程都在运行 turn 时，新对话的 turn 改用单次进程（结束后停止），不会返回容量错误。删除对话、删除工作区、过期清理和 daemon 关闭都会停止对应进程。Claude Code 的产品权限模式为 `ask`（`default`）、`accept-edits`（`acceptEdits`：自动放行工作区内的文件编辑，其他操作仍经 `can_use_tool` 询问）、`auto`（默认）与 `full-access`（`bypassPermissions`）；`workMode: "plan"` 时一律以 `plan` 启动。`accept-edits` 只有 Claude Code 声明，其他 Provider 收到时按不支持的模式拒绝。daemon 以 `--permission-mode` 启动 Claude Code，并核对它实际运行的模式：`initialize` 应答的 `current_permission_mode`、每次调用的 `system/init` 帧，以及 turn 内改变模式的 `system/status` 帧（`permissionMode`）。Claude Code 不接受请求的模式时不报错，而是静默换成别的模式：设置层 `permissions.disableBypassPermissionsMode: "disable"` 会把 `bypassPermissions` 换成设置里的 `defaultMode`；`permissions.disableAutoMode: "disable"`，或模型/账号不提供 auto 模式（例如经非第一方 API 使用 Haiku），会把 `auto` 换成 `default` 或设置里的 `defaultMode`，有时 `init` 仍报告 `auto`、随后才由 `status` 帧改掉。`initialize` 报告的模式与请求的 `bypassPermissions` 或 `auto` 不一致时（或 turn 内才发现 `auto` 被换掉、且此前没有查过），daemon 发一次 `get_settings` 控制请求，在各设置层（`sources[].source`，原样使用 Claude 的名字：`userSettings`、`projectSettings`、`localSettings`、`policySettings`、`flagSettings`）中找出设置了对应 `disable` 的层（`policySettings` 优先，否则取优先级最高的层）；结论保存在进程上，同一进程的后续 turn（包括预热进程上的首个 turn）沿用。查询失败或超时（5 秒）只是不知道来源，不影响启动。

- **完全访问被拒**（请求 `full-access`，Claude Code 报告的不是 `bypassPermissions`）：不降级。先发出 `provider.event` `{ "provider": "claude-code", "providerMethod": "bypass_mode_disabled", "metadata": { "requestedMode": "full-access", "reportedMode": "<Claude 报告的模式>", "settingsSource": "<设置层或 null>", "reason": "<与错误文案相同>" }, "turnId" }`，再以 `PROVIDER_UNAVAILABLE` 使 turn 失败。已知来源时文案为 `Claude Code settings ({source}) disable full access (permissions.disableBypassPermissionsMode). Turn that setting off in CLI settings to use full access.`，否则为 `Claude Code refused full access (bypassPermissions); reported '{actual}'. Check permissions.disableBypassPermissionsMode in your Claude Code settings or managed policy.`。`initialize` 已报告被拒时 prompt 不会发给模型。turn 内 `status` 帧把模式改成 `plan` 以外的其他模式同样按此处理（`plan` 随 EnterPlanMode/ExitPlanMode 进出，不算被拒）。
- **auto 被换掉**（请求 `auto`，Claude Code 报告 `default` 或 `acceptEdits`）：turn 不失败。这两种模式对未自行放行的操作仍经 `can_use_tool` 询问，不比 auto 宽松：需要授权的工具发出 TodeX 的 `permission.requested`，TodeX 自有 MCP 工具按 `ask` 逐次审批。每个 turn 只发一次 `provider.event` `{ "provider": "claude-code", "providerMethod": "auto_mode_unavailable", "metadata": { "requestedMode": "auto", "activeMode": "ask" | "accept-edits", "model": "<init 报告的模型或 null>", "reason", "cause": "settings" | "unavailable", "settingsSource": "<设置层或 null>" }, "turnId" }`（与 Antigravity 的同名事件形状一致；`cause` 为 `settings` 表示找到了禁用 auto 的设置层，此时 `reason` 为 `Claude Code settings ({source}) disable auto mode (permissions.disableAutoMode)`，否则为 `Claude Code did not enable auto mode for this model or account`；`turn.started` 的 `effectivePermissions` 仍记录请求的 `auto`，以该事件为准）。换成 `plan`、`dontAsk` 等其他模式时（`plan` 会把实现 turn 变成只做计划，`dontAsk` 不询问就拒绝所有未预先放行的工具），turn 以 `PROVIDER_UNAVAILABLE` 失败，已知来源时文案为 `Claude Code settings ({source}) disable auto mode (permissions.disableAutoMode) and it fell back to '{actual}'. …`。

其他不一致仍以 `PROVIDER_UNAVAILABLE`（`Claude Code did not activate requested permission mode …`）使 turn 失败。

omp（oh-my-pi，Pi 的分支）通过 `omp acp --approval-mode always-ask` 接入，每个对话对应一个常驻 ACP 进程（空闲超过 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）后回收，daemon 级上限 32 个）。omp 的 RPC 模式已与 Pi 分叉（没有 `--session-id`、`agent_settled`、`get_commands` 等），因此不复用 Pi 驱动。omp 自己管理凭据（`omp` 中 `/login` 写入 `~/.omp/agent/agent.db`，或 `models.yml`），其 `agent` authMethod 只是指向这些本地凭据，daemon 不发送 `authenticate`；没有可用模型时会话的 prompt 以 omp 的 “No model selected” 错误上浮，`doctor providers --provider omp` 报 `MODEL_UNAVAILABLE` 并提示先运行 `omp` 登录。`always-ask` 让 omp 对每次写文件、执行命令与 MCP 工具调用（omp 把 MCP 工具归为 write 级）都请求批准（读操作直接放行）：`bash` 经 `session/request_permission`，其余工具（`write`、`edit`、`task`、MCP 工具等）经 ACP 表单 elicitation（`elicitation/create`，一个必填的字符串 `value`，选项为 `Approve` 或 `Approve Once`、`Approve <tool> for this session` 与 `Deny`），daemon 因此在 `initialize` 中声明 `clientCapabilities.elicitation.form`。两种请求都转成同样的 `permission.requested`，由 TodeX 权限模式决定：`ask` 逐条询问（elicitation 的三个选项分别对应 `allow_once`、`allow_always`（本会话）与 `reject_once`；omp 记住文件或目录的授权选项不提供）；`auto`/`full-access` 由 daemon 客户端侧自动应答（都优先 `allow_once`/`Approve`，原因同 OpenCode），并记录 `autoApproved: true`；取消回答 `{"action":"cancel"}`。不是批准格式的表单 elicitation（例如 omp `ask` 工具的问题）在任何模式下都以 `kind: "elicitation"` 交给用户回答，非 form 模式的 elicitation 直接回复 `decline`。omp 的 ACP 服务在一次 prompt 返回后不再转发任何消息，后台任务完成后 omp 自行发起的汇报回合 TodeX 看不到，因此 daemon 以 `--config $TODEX_AGENTD_DATA_DIR/providers/omp/todex.config.yml` 启动 omp（每次启动前重写，0600），关闭 `async.enabled`、`bash.autoBackground.enabled` 与 `eval.autoBackground.enabled`：`task` 子 agent 与长命令都在当前 turn 内完成。omp 的高危命令与 `tools.restrictedFolders` 提示同样经此路径。plan 工作模式经 `configOptions.mode` 映射为 `plan`，implement 显式重置为 `default`（omp 关闭 `plan.enabled` 时 `plan` 不可用，设置失败原样上浮）。模型经 `configOptions.model` 选择（`provider/model` 形式，只列出 omp 持有凭据的模型；经 ACP 切换模型不会写回 omp 的配置），思考强度经 `configOptions.thinking` 下发（档位随模型变化，含 `off` 与 `auto`），模型目录探测逐个模型读取其 `supportedReasoningEfforts`；会话内支持 `configure` live control 切换。mid-turn 插话经 omp 的 `_session/steering` 扩展方法注入当前 turn，仅在 omp 答复 `outcome: "injected"` 时确认；omp 的 follow-up 队列不开放。omp 声明 `loadSession` 与 `sessionCapabilities.resume`/`fork`，恢复对话优先使用不回放历史的 `session/resume`，会话分叉经 `session/fork`。Usage 来自 `usage_update` 与 prompt 响应 `result.usage`。daemon 启动 omp 时注入 `PI_NO_TITLE=1`（跳过 omp 自动生成标题的额外模型调用）与 `NO_COLOR=1`，并且不转发为 Pi 设置的 `PI_CODING_AGENT_DIR`（omp 读取同名变量，会因此指向 Pi 的目录）；确需为 omp 指定该变量时把它加入 `TODEX_AGENTD_OMP_ENV_ALLOWLIST`。默认白名单为空：omp 的凭据在其 `agent.db` 中，环境变量里的 provider key 只有列入白名单才会转发。skills 目录（`.omp/skills`、`~/.omp/agent/skills`）进入 catalog；MCP、插件、账户、原生历史与 omp 的 `task` 子 agent 暂不接入 TodeX 管理。模型目录与 slash 命令通过同一次真实 ACP 会话探测获得，按工作区缓存约 5 分钟，发现总时限 30 秒。

DeepSeek Harness 通过 `dsh --profile acp` 接入（`npm install -g @deepseek-ai/dsh`，需要 daemon 的 PATH 中有 Node.js），每个对话对应一个常驻 ACP 进程（空闲超过 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）后回收，daemon 级上限 32 个）。daemon 以 `--patch $TODEX_AGENTD_DATA_DIR/providers/deepseek-harness/todex.patch.yml` 启动它：该覆盖层由 daemon 在每次启动前重写，禁用 dsh-base 默认开启的 `session-log-deepseek` 与 `plugin-package-inventory-deepseek` 插件，即 TodeX 启动的 dsh 不会随官方 DeepSeek API 请求上传会话日志与已启用插件清单；用户自行运行的 `dsh web` 等不受影响。dsh 不声明 authMethods，daemon 不发送 `authenticate`；凭据由 dsh 自行解析，自 0.2.1-alpha.3 起顺序为 `$DSH_HOME/.credentials.yaml` 的 `refs`、继承的环境变量、启动目录（即工作区）的 `.env`、`$DSH_HOME/.env`——已保存的凭据优先于环境变量。默认环境白名单转发 `DSH_HOME`、`DEEPSEEK_API_KEY`、`DEEPSEEK_BASE_URL`、`DEEPSEEK_SEARCH_BASE_URL`；未配置凭据时模型调用错误原样上浮为 `turn.failed`。dsh 进程仍以工作区为启动目录（其沙箱以启动目录为工作区根）；工作区 `.env` 设置了 dsh 只允许由启动环境设置的变量（如 `NODE_OPTIONS`、`PATH`、代理变量）时 dsh 拒绝启动，错误连同 dsh 的 stderr 原样上浮。账户、插件与 MCP 由 TodeX 写入 `acp` profile 的用户补丁层 `$DSH_HOME/profiles/acp/cordis.patch.yml`（见下文 `/v2/agent-providers`、`/v2/providers/deepseek-harness/plugins` 与 `/v2/catalog/mcp`），API Key 写入 `.credentials.yaml` 的 `refs`；dsh 只在启动时读取补丁层，因此 TodeX 改写 dsh 配置后，常驻进程在下一个 turn 开始前（turn 之间，从不打断运行中的 turn）重启并以 `session/resume` 接回同一会话。这些写入遵守 dsh 的跨进程写锁（profile 的 `package.json.lock`、`.credentials.yaml.lock`），与同时运行的 `dsh web`、`dsh plugin` 不会交错；家目录层 `$DSH_HOME/cordis.patch.yml` 覆盖同一行时，TodeX 拒绝写入并返回 `409 CONFLICT`。权限模式提供 `ask`/`auto`/`full-access`：`ask` 把每个 `session/request_permission` 转成 `permission.requested`；`auto`/`full-access` 由 daemon 客户端侧自动应答（dsh 只提供一次性的 `allow_once`/`reject_once`），并记录 `autoApproved: true`。dsh 的 ACP 不提供 modes，因此不支持 plan 工作模式。模型经 `configOptions.model` 选择：dsh 按 provider 路由分组列出模型，值是不透明的路由键（当前为 JSON 数组字符串，如 `["deepseek-official","deepseek-v4-pro"]`），daemon 原样作为模型 `id` 返回并回传，分组 id 作为 `family`；未指定模型时使用 dsh `acp` profile 的默认路由。reasoning effort 经 `configOptions.reasoning_effort` 下发，只对声明推理能力的模型存在，模型目录探测逐个模型读取其 `supportedReasoningEfforts`（dsh 表示“provider 默认”的空值不作为档位列出）；会话内可用 `configure` live control 切换，先写 `model` 再写 `reasoning_effort`。dsh 声明 `sessionCapabilities.resume` 而没有 `loadSession`，新进程恢复对话一律使用不回放历史的 `session/resume`；记录的会话已被删除时新开会话并发出 `providerMethod: "session/recreated"`。dsh 只在一条消息提交后才发送 `agent_message_chunk`/`agent_thought_chunk`，因此回复按完整消息分段出现而非逐字流式；工具事件统一为 `kind: "other"`，标题为工具名。Usage 来自 `usage_update`。图片输入按其 `initialize` 的 `promptCapabilities.image` 判断（取决于 `acp` 行的默认路由；dsh 0.2.1-rc.2 自带的默认 `deepseek-v4-flash` 不在其模型目录中，被视为纯文本，因此默认不提供图片输入）。ACP `initialize` 的 `agentInfo.version` 固定为 `0.0.1`，dsh 的真实版本以 `dsh -V` 为准。不支持分叉、原生压缩、mid-turn 插话（`steer` 返回 unsupported）、原生队列与 slash 命令；原生历史（`$DSH_HOME` 下压缩且版本化的会话日志）不接入。模型目录探测结果按工作区缓存约 5 分钟，发现总时限 30 秒。

会话分叉（`conversation.fork`）只在 Provider 真实支持时通过 `capabilities.controlActions` 的 `fork` 项暴露：Codex 走 app-server `thread/fork`，Pi 走 RPC `clone`，Grok Build 走 `_x.ai/session/fork`，OpenCode 与 omp 走 ACP `session/fork`；Claude Code 由 daemon 复制 `~/.claude` 下的会话 transcript 并改写 `sessionId`（等价于 `claude --resume --fork-session`，不消耗额外汇合）。通用 ACP profile 与 Devin 的分叉能力按已安装 agent 动态探测——daemon 在其 `initialize` 响应中检查 `agentCapabilities.sessionCapabilities.fork`，结果缓存约 5 分钟；声明该能力的 agent 自动开放分叉（Devin CLI 目前未声明，升级后无需改动即可启用），未声明时 `fork` 不出现在 `controlActions` 中，`conversation.fork` 直接返回 unsupported 错误。

`capabilities.managedMcp` 表示 daemon 能否把自己的 agent 工具（SSH、桌面 MCP 服务器）注入该 Provider：Codex 经 `config` 覆盖、Claude Code 经 `--mcp-config` 文件、Pi 经 `-e` 扩展脚本与进程环境变量注入、ACP profile / Grok Build / Devin / OpenCode / DeepSeek Harness / omp 经 ACP `mcpServers` 注入、Antigravity 经全局 `mcp_config.json` 静态条目加进程环境变量注入，均为 `true`。`nativeMcp` 则表示 Provider 是否自行加载用户配置的 MCP 服务器。各 Provider 的这类静态能力在后端各驱动的 `ProviderProfile`（`src/provider/profile.rs`）中统一声明。

```http
GET /v2/providers
GET /v2/providers/versions
POST /v2/providers/{provider}/upgrade
POST /v2/providers/{provider}/install
GET /v2/providers/upgrades/{operationId}
GET /v2/providers/{claude-code|codex|pi|deepseek-harness}/settings
PATCH /v2/providers/{claude-code|codex|pi}/settings
PUT /v2/providers/{claude-code|codex|pi|deepseek-harness}/settings
GET /v2/providers/pi/plugins
PUT /v2/providers/pi/plugins/state
POST /v2/providers/pi/plugins/operations
GET /v2/providers/pi/plugins/operations/{operationId}
GET /v2/providers/pi/plugin-config/{plugin}
PATCH /v2/providers/pi/plugin-config/{plugin}
GET /v2/providers/pi/mcp
PATCH /v2/providers/pi/mcp
GET /v2/providers/deepseek-harness/plugins
PUT /v2/providers/deepseek-harness/plugins/state
PUT /v2/providers/deepseek-harness/plugins/bundles
POST /v2/providers/deepseek-harness/plugins/operations
GET /v2/providers/deepseek-harness/plugins/operations/{operationId}
GET /v2/providers/deepseek-harness/plugin-config/{plugin}
PATCH /v2/providers/deepseek-harness/plugin-config/{plugin}
GET /v2/providers/models?provider=codex&workspace=/home/user/projects/demo
GET /v2/providers/commands?conversationId={conversationId}
GET /v2/agent-providers?agent={codex|claude-code|grok-build|pi|opencode|deepseek-harness}
GET /v2/agent-providers/{agent}/live
PUT /v2/agent-providers/{agent}/{id}
DELETE /v2/agent-providers/{agent}/{id}
POST /v2/agent-providers/{agent}/{id}/activate
POST /v2/agent-providers/{agent}/import-live
GET /v2/agent-providers/{agent}/export
POST /v2/agent-providers/{agent}/import
GET /v2/agent-providers/{agent}/{id}/models
POST /v2/agent-providers/{agent}/{id}/models
GET /v2/conversations
POST /v2/conversations
GET /v2/conversations/{conversationId}
PATCH /v2/conversations/{conversationId}
DELETE /v2/conversations/{conversationId}
GET /v2/history/storage
POST /v2/history/cleanup
GET /v2/history/cleanup-policy
PUT /v2/history/cleanup-policy
POST /v2/history/cleanup-policy/run
GET /v2/conversations/{conversationId}/events?afterSequence=0&limit=200&detail=full
GET /v2/conversations/{conversationId}/events?beforeSequence=500&limit=200&detail=summary
POST /v2/conversations/{conversationId}/prompt
POST /v2/conversations/{conversationId}/cancel
POST /v2/conversations/{conversationId}/runtime/stop
POST /v2/conversations/{conversationId}/warm
POST /v2/conversations/{conversationId}/permissions/{permissionId}
```

模型目录中的 `contextWindow` 来自 Provider 原生模型元数据；Provider 未公开该值时省略。客户端应结合实时 usage 事件显示上下文占用，不得用静态模型表猜测窗口大小。

`/v2/providers/models` 会实时向指定 Agent 查询模型目录，返回 `source` 与 `fetchedAt`。每个模型包含 `supportedReasoningEfforts`，并可通过 `defaultReasoningEffort` 声明后端当前默认强度。Codex 使用 app-server `model/list`，Pi 使用 RPC `get_available_models` 与 `get_state`，Claude Code 在配置了 `ANTHROPIC_BASE_URL` 时读取 `/v1/models`（与 Claude Code 一致：`ANTHROPIC_AUTH_TOKEN` 作 `Authorization: Bearer` 并优先于以 `x-api-key` 发送的 `ANTHROPIC_API_KEY`；非回环地址必须为 https，否则跳过 HTTP 发现并返回内置别名；单次请求 10 秒超时，不跟随重定向），Grok Build 从 ACP initialize 的原生模型状态读取，Antigravity 解析 `agy models`，DeepSeek Harness 读取 ACP 会话的 `configOptions`。需要启动 CLI 或访问网关的目录（Codex、Claude Code、Pi、Grok Build、Antigravity 的模型与命令）按工作区缓存约 60 秒，Devin、OpenCode、DeepSeek Harness、omp 约 5 分钟；同一目录的并发查询只触发一次探测，经 TodeX 切换 Agent Provider 或检测到 CLI 可执行文件变化（升级）时立即失效，因此 `fetchedAt` 可能早于本次请求。查询失败不缓存，客户端应保留上一次成功目录，并展示可恢复错误。

`GET /v2/providers/versions` 在 daemon 所在主机读取配置的九个内建 CLI，并从各自官方发布源查询最新版（Codex、Claude Code、OpenCode、DeepSeek Harness 与 omp（`@oh-my-pi/pi-coding-agent`，预编译二进制与之同版本）查询 npm registry，Pi 查询 `pi.dev`，Devin 与 Antigravity 读取各自的自更新 manifest，Grok Build 执行 `grok update --check --json`）。配置的 binary 在 PATH 与官方安装器使用的用户目录（`~/.local/bin`、`~/.grok/bin`、`~/.opencode/bin`、`~/.pi/agent/bin`、`~/.npm-global/bin`，Unix）中都找不到时返回 `status: "notInstalled"`、`installed: false`，不带 `error`，`installSupported` 表示可一键安装。结果按 CLI 分项缓存：整行（含本地 `--version`）约 30 秒，远端最新版成功时 15 分钟、失败时 2 分钟；`?refresh=true` 跳过最新版缓存重新查询（刷新按钮使用）。每个 CLI 的本地版本与最新版并行查询，同一 CLI 的并发查询共用一次探测，单个查询失败不会隐藏其他 CLI。顶层 `checkedAt` 是各 CLI 中最早一次最新版查询的时间。ACP profile 作为外部管理项列出，不执行任意 profile 命令，也不提供升级。随容器镜像预装的 CLI（解析出的 binary 或其符号链接目标位于 `TODEX_AGENTD_IMAGE_CLI_DIR` / `agent.image_cli_dirs` 下）带 `managedBy: "image"`，`upgradeSupported` 与 `installSupported` 均为 `false`，由更新镜像来升级；其他 CLI 不带该字段。`POST /v2/providers/{provider}/upgrade` 仅接受 `codex`、`pi`、`claude-code`、`grok-build`、`devin`、`opencode`、`antigravity`、`deepseek-harness`、`omp` 固定标识（OpenCode 执行 `opencode upgrade`，omp 执行 `omp update`，Antigravity 执行 `agy update`，DeepSeek Harness 没有自更新命令，执行与安装相同的 `npm install --global @deepseek-ai/dsh@latest`），返回异步 operation；客户端使用 operation 查询接口轮询。升级命令不经过 shell、不读取工作区，并在升级后重新调用同一个配置 binary 验证版本。每个 CLI 有自己的锁：安装或升级在整个运行期间独占该 CLI 的锁，不同 CLI 的安装与升级可以同时进行，同一 CLI 同时只有一个（再次发起返回 `409 CONFLICT`）。该 CLI 有运行中的 Agent turn（或 Codex 有本地 adapter）时返回 `409 CONFLICT`；该 CLI 的 turn 启动、Provider 发现或版本探测正在进行时，升级最多等待 15 秒，仍未结束则返回 `409 CONFLICT`。升级期间该 CLI 的新 turn、fork 与压缩等待升级结束后用新版本启动，它的 Provider 发现与预热返回忙，其他 CLI 不受影响；版本查询对该 CLI 返回上一次结果（没有时按 operation 推断），不会运行正在被替换的 binary，其他 CLI 照常探测。操作结束只使该 CLI 的缓存失效。响应的 `activeOperations` 列出全部运行中的 operation（按开始时间），`activeOperation` 为其中最早的一个，供只跟踪一个操作的旧客户端使用。CLI 升级在 loopback 部署中也必须携带有效设备签名，以免网页通过跨域请求改变宿主机工具链；已认证的尝试及最终结果会写入审计日志，初始审计记录无法落盘时不会启动升级。

`POST /v2/providers/{provider}/install` 与升级共用 operation、该 CLI 的锁与审计（operation 带 `action: "install" | "upgrade"`，审计事件带 `action`），CLI 已安装时返回 `409 CONFLICT`。目标是镜像管理的 CLI（`managedBy: "image"`）时，安装与升级都返回 `409 CLI_MANAGED_BY_IMAGE`（记录 deny 审计），不执行任何命令。安装执行各厂商文档中的固定安装脚本（`curl -fsSL <官方 URL> | sh/bash`，URL 为常量，不含请求内容；下载及其重定向只允许 HTTPS）：Codex `chatgpt.com/codex/install.sh`（`CODEX_NON_INTERACTIVE=1`）、Pi `pi.dev/install.sh`（需要主机已有 Node.js ≥ 22）、Claude Code `claude.ai/install.sh`、Grok Build `x.ai/cli/install.sh`、Devin `cli.devin.ai/install.sh`、OpenCode `opencode.ai/install`、Antigravity `antigravity.google/cli/install.sh`、omp `omp.sh/install`（以 `sh -s -- --binary` 运行，即使主机有 bun 也安装预编译的 `~/.local/bin/omp`，而不是 `bun install -g`）。DeepSeek Harness 只经 npm 发布，执行固定的 `npm install --global --no-fund --no-audit @deepseek-ai/dsh@latest`（不经 shell，需要 daemon 的 PATH 中有 `npm`；没有时 `installSupported`/`upgradeSupported` 为 `false`），以 npm 的退出状态与之后配置 binary 能否输出版本判定成功。脚本在无控制终端的新会话中运行，交互提示取默认值；部分脚本会按厂商默认行为在 shell rc 中加入 PATH。安装成功与否以脚本结束后配置 binary 能否输出版本为准（Devin 脚本末尾的交互式 `devin setup` 在无终端时失败不影响结果）。Windows 暂不提供一键安装（`installSupported: false`）。

`/v2/providers/{provider}/settings` 读写 daemon 主机上 CLI 的用户配置文件，供 CLI 管理页的设定面板使用：`claude-code` 为 `$CLAUDE_CONFIG_DIR/settings.json`（缺省 `~/.claude/settings.json`），`codex` 为 `$CODEX_HOME/config.toml`（缺省 `~/.codex/config.toml`），`pi` 为 `$PI_CODING_AGENT_DIR/settings.json`（缺省 `~/.pi/agent/settings.json`），`deepseek-harness` 为 `acp` profile 的用户补丁层 `$DSH_HOME/profiles/acp/cordis.patch.yml`（缺省 `~/.dsh/...`）；其他 provider 返回 `UNSUPPORTED`。`GET` 返回 `{ "path", "format", "exists", "revision", "text", "settings", "parseError"? }`：`format` 为 `"json"`、`"toml"` 或 `"yaml"`（DeepSeek Harness；条目是列表，`settings` 恒为 `{}`），`text` 为文件原文（不存在时为空串），`settings` 为解析后的对象（TOML 转为等价 JSON，日期时间为字符串；文件不存在时为 `{}`，无法解析时为 `null` 并带 `parseError`），`revision` 为内容的 SHA-256（文件不存在时为 `"missing"`）。`env` 等值原样返回，可能含密钥，客户端应默认遮蔽。`PATCH` 请求体 `{ "revision", "patch" }` 按 RFC 7386 合并：`null` 删除键，对象递归合并，数组与其他值整体替换，未提及的键保持不变。JSON 文件结果以两空格缩进重写；TOML 文件经 `toml_edit` 原地修改，未触及的键、注释与排版保留，新增的嵌套对象沿用父表写法（`[a.b]` 表头 / 点分键 / 内联表），被清空的表随之删除，数组中的 `null` 返回 `400 INVALID_REQUEST`。文件无法解析时 `PATCH` 返回 `409 CONFLICT`；YAML 不支持 `PATCH`（返回 `UNSUPPORTED`）。`PUT` 请求体 `{ "revision", "text" }` 整体替换原文（须为 JSON 对象、合法 TOML，或 DeepSeek Harness 的 YAML 补丁条目列表（空文件亦可），按原样保存——`!!js` 等标签与注释不变——不超过 1 MiB，否则 `400 INVALID_REQUEST`）。DeepSeek Harness 写入持有 dsh 的 profile 写锁，成功后其常驻进程在下一个 turn 前重启。两种写入都要求 `revision` 与磁盘当前内容一致，否则返回 `409 CONFLICT`（CLI 自身、Agent Provider 切换或手动编辑已改动文件），客户端应重新读取；成功时返回与 `GET` 相同的新文档。Pi 写入前与 Pi 一样获取 `proper-lockfile` 锁（`settings.json.lock` 目录，超过 10 秒未更新视为残留），约 1 秒内拿不到时返回 `409 CONFLICT`。写入保留文件原有权限（新建为 0600）、符号链接写到目标文件、缺失的配置目录会被创建；文件可定义 hook、shell 等会在主机上执行的命令，因此即便在 loopback 部署中写入也必须携带有效设备签名。Claude Code 的 `settings.json` 同时是其 Agent Provider 的 `settingsConfig`：切换 Provider 时，当前内容回填到原 Provider 档案，新 Provider 的配置整体写入；Codex 的 `config.toml` 同理。

`/v2/providers/pi/plugins` 管理 Pi 的包与内置扩展，供 Pi 设定面板的“插件”页使用（只涉及全局 `<pi_dir>/settings.json`，不读项目级配置）。`GET` 返回 `{ "settingsPath", "revision", "packages": [{ "source", "state", "installedPath"?, "name"?, "version"?, "description"? }], "builtins": [{ "id", "enabled" }], "listError"?, "parseError"? }`：`packages` 按 `settings.json` 中 `packages` 的顺序列出，`state` 为 `enabled`（字符串写法）、`disabled`（对象写法且 `extensions`/`skills`/`prompts`/`themes` 全为空数组、无其他键）或 `filtered`（其他对象写法，即用 `pi config` 设过资源过滤）；安装路径取自 `pi list --no-approve` 的 `User packages` 段，`pi list` 失败时 `listError` 给出原因，`npm:` 包改按 `<pi_dir>/npm/node_modules/<name>` 推断；`name`/`version`/`description` 读自包的 `package.json`。`builtins` 固定为 `mcp`、`codemode`、`tool-search`、`llama.cpp`，`extensions` 含 `-builtin:<id>` 时为停用。`revision` 与 `/v2/providers/pi/settings` 相同。

`PUT /v2/providers/pi/plugins/state` 请求体 `{ "revision", "source" | "builtin", "enabled" }`（`source` 与 `builtin` 二选一）：停用包时把该项改写为 `{ "source", "extensions": [], "skills": [], "prompts": [], "themes": [] }`，启用时还原为字符串；`filtered` 包返回 `409 CONFLICT`（应在 `pi config` 中修改），未配置的包返回 `404`。内置扩展先删除已有的 `+builtin:<id>`/`-builtin:<id>`，停用时再追加 `-builtin:<id>`。`settings.json` 其余内容保留，写入与 `/v2/providers/pi/settings` 一样校验 `revision`（不一致返回 `409 CONFLICT`）并获取 Pi 的文件锁；成功时返回与 `GET` 相同的新列表。

`POST /v2/providers/pi/plugins/operations` 请求体 `{ "action": "install" | "remove", "source" }`，在后台运行 `pi install <source>` 或 `pi remove <source>`（工作目录为 daemon 数据目录，超时 10 分钟），立即返回 `{ "id", "action", "source", "status": "running", "startedAt" }`；`GET .../operations/{operationId}` 返回最近一次操作的同一结构，`status` 变为 `succeeded` 或 `failed`（带 `finishedAt`，失败时 `error` 为 Pi 输出的最后一行）。`source` 必须以 `npm:`、`git:`、`https://`、`http://`、`/` 或 `~/` 开头，不含空白与控制字符，不超过 500 字符，否则返回 `400 INVALID_REQUEST`；`remove` 的 `source` 必须与已配置的某项完全一致，否则返回 `404`。同一时间只允许一个操作；有 Pi 任务运行、另一操作进行中、Pi 正在启动或安装/升级时返回 `409 CONFLICT`。操作期间持有 Pi 的锁，新的 Pi 进程等待其结束，其他 Agent 不受影响。尝试、拒绝、成功与失败均记入审计事件 `pi.packages.audit`。安装会执行第三方代码，因此要求有效设备签名。

`/v2/providers/pi/plugin-config/{plugin}` 读写已知 Pi 扩展的配置文件，路径固定在 `<pi_dir>` 下，客户端不能指定任意路径：`permission-system` → `extensions/pi-permission-system/config.json`，`mcp-adapter` → `mcp-adapter.json`（只读写顶层 `settings`，其他键既不返回也不接受），`web-access` → `web-search.json`，`exa` → `pi-exa.json`，`fff` → `pi-fff.json`，`subagents` → `subagents.json`，`goal` → `pi-goal.json`，`plan-mode` → `pi-plan-mode.json`，`tasks` → `tasks-config.json`，`context-prune` → `context-prune/settings.json`，`cache-optimizer` → `pi-cache-optimizer-config.json`；其他名称返回 `404`。`GET` 返回 `{ "plugin", "path", "exists", "revision", "settings", "parseError"? }`（不含原文）；`web-access` 中键名以 `ApiKey`、`ApiToken`、`Token`、`Secret`、`Password` 结尾的值以及 `*Headers`、`authFetch` 下的字符串，若是字面值则替换为 `"__todex_secret__"`（`$ENV`、`!command` 引用原样返回）。`PATCH` 请求体 `{ "revision", "patch", "replace"? }` 按 RFC 7386 合并，`replace` 为键路径数组（如 `[["permission", "bash"]]`），所列路径合并后再整体替换为 `patch` 中该处的对象并保持其键顺序（权限规则按“最后匹配生效”，顺序有意义；路径在 `patch` 中不是对象时返回 `400`）；值为 `"__todex_secret__"` 的条目被忽略、保留磁盘上的原值；文件不存在时创建（含缺失目录），`revision` 不一致返回 `409 CONFLICT`，成功时返回与 `GET` 相同的新文档。写入要求有效设备签名。扩展多在启动时读取配置，改动对之后启动的 Pi 进程生效。

`/v2/providers/pi/subagent-templates` 管理 `@tintinweb/pi-subagents` 的全局自定义 Agent 类型，即 `<pi_dir>/agents/*.md`（`<pi_dir>` 为 `$PI_CODING_AGENT_DIR` 或 `~/.pi/agent`；扩展另读的项目级 `.pi/agents/`、`.agents/agents/` 不在此管理）。模板以文件名（去掉 `.md`，下称 `file`）寻址：1–100 个字母、数字、`.`、`_`、`-`，不能以 `.` 开头。文件为可选的 YAML front matter 加 Markdown 正文（系统提示词），切分方式与 Pi 的 `parseFrontmatter` 相同：以 `---` 开头且后面有以 `---` 开头的行才算 front matter，正文去除首尾空白；front matter 必须是映射，`name` 不能含 `:`，文件不超过 256 KiB（超出返回 `413`）。Agent 类型为 front matter 的 `name`，缺省为 `file`。扩展每次调用 `Agent` 都会重新扫描，改动对运行中的 Pi 也立即生效。

- `GET /v2/providers/pi/subagent-templates` 返回 `{ dir, templates, builtIns }`。`templates` 按文件名排序，每项 `{ file, name, displayName?, description?, color?, model?, enabled, overridesBuiltIn, parseError? }`；无法解析的文件也列出（带 `parseError`，`name` 为 `file`）。`builtIns` 为扩展内置的 `general-purpose`、`Explore`、`Plan`，每项 `{ name, state: "enabled" | "disabled" | "overridden", file? }`：类型与之相同的模板使其 `overridden`（`file` 为该模板）；与内置同名、只含 `enabled: false` 且正文为空的文件视为停用该内置的桩文件（`disabled`），不出现在 `templates` 中。
- `GET …/subagent-templates/{file}` 返回 `{ file, path, revision, content, frontMatter, body, parseError? }`：`content` 为原文，`frontMatter` 为解析后的对象（无法解析时为 `null`），`body` 为去除首尾空白的正文。
- `POST …/subagent-templates` 请求体 `{ file, content }` 或 `{ file, frontMatter, body }` 新建 `<file>.md`，已存在返回 `409`。表单形式按固定键序（`name`、`display_name`、`description`、`color`、`tools`、`model`、`thinking`、`max_turns`、`prompt_mode`、`run_in_background`、`isolation`、`enabled`、`extensions`，其余按字母序）写出，值为 `null` 的键省略。
- `PUT …/subagent-templates/{file}` 请求体 `{ revision, content }` 原样覆盖；`{ revision, frontMatter, body }` 把 `frontMatter` 合并进现有 front matter（`null` 删除该键，已有键原位替换，新键按上述键序追加，其他键保留，但 YAML 注释与格式不保留）并替换正文，现有文件无法解析时返回 `409`。`revision` 不一致返回 `409 CONFLICT`。返回与 `GET` 相同的文档。
- `DELETE …/subagent-templates/{file}` 删除该文件，返回 `{ file }`，不存在返回 `404`。
- `PUT …/subagent-templates/built-ins/{name}` 请求体 `{ enabled }`：`false` 写入 `<name>.md` 桩文件（`---\nenabled: false\n---`），`true` 删除桩文件；内置被模板覆盖时返回 `409`（应改该模板的 `enabled`），未知名称（区分大小写）返回 `404`。返回与列表相同的结构。

写操作要求有效设备签名；写入为临时文件原子替换（已有文件沿用其权限，新文件 0600），解析符号链接后目标不在 `<pi_dir>/agents` 内时返回 `400 INVALID_REQUEST`、不写入。旧 daemon 对这些路由返回 `404`。

`/v2/providers/pi/mcp` 读写 Pi 原生 MCP 的全局配置 `<pi_dir>/mcp.json`，供 Pi 设定面板的“MCP”页使用（项目级 `.pi/mcp.json` 仍经 `/v2/catalog/mcp` 管理）。`GET` 返回 `{ "path", "exists", "revision", "config", "parseError"?, "builtinEnabled", "adapterInstalled" }`：`config` 为解析后的整个文件（不存在时为 `{}`，无法解析时为 `null`），`env`/`headers` 等值原样返回，可能含密钥；`builtinEnabled` 为 `settings.json` 中 `builtin:mcp` 是否启用，`adapterInstalled` 为是否配置了未停用的 `pi-mcp-adapter` 包（两者任一不满足时 Pi 不读取 `mcp.json`）。`PATCH` 请求体 `{ "revision", "patch" }`，`patch` 只能含 `mcpServers` 与 `autoEnableCodemode`（其他键返回 `400`）：`mcpServers.<name>` 为对象时整体替换同名条目、为 `null` 时删除（改名即在同一请求中删除旧名并写入新名），`autoEnableCodemode` 为布尔值或 `null`（删除）；文件其余键保留。写入的条目按 Pi 的规则校验：名称只含字母、数字、`_`、`-`，且不得与其他条目仅差在 `-`/`_`；须有 `command`（stdio，`args` 为字符串数组，`env` 为字符串映射，`cwd` 为字符串）或 `url`（http/https，`headers` 为字符串映射）；`type: "sse"` 被拒绝；`exposure`/`toolExposure` 取 `codemode`、`deferred`、`direct`、`hidden`（或别名 `codemode-deferred`），`enabled` 为布尔值，`timeout` 为正数，`description` 为字符串；不符合时返回 `400 INVALID_REQUEST`。`revision` 不一致返回 `409 CONFLICT`，文件无法解析时返回 `409 CONFLICT`；文件不存在时创建（新建为 0600），成功时返回与 `GET` 相同的新文档。条目会让 Pi 在主机上运行命令，因此写入要求有效设备签名。改动对之后启动的 Pi 进程生效。

`/v2/providers/deepseek-harness/plugins` 管理 DeepSeek Harness `acp` profile 的插件，供其设定面板的“插件”页使用。`GET` 返回 `{ "profilePath", "revision", "manifestRevision", "runtimeVersion"?, "pnpmAvailable", "rows": [...], "packages": [...], "exemptions": [...], "composeError"?, "parseError"? }`。`rows` 来自 `dsh --profile acp --patch <TodeX 覆盖层> --dump-config`（在 daemon 数据目录下运行，不读工作区；结果按补丁层、家目录层、profile 清单的内容与 dsh 可执行文件缓存，最长 5 分钟，CLI 升级或插件操作进行中且无缓存时返回 `409 CONFLICT`），每行 `{ "id"?, "name", "state": { "kind": "enabled" | "disabled" | "conditional", "expression"? }, "source", "sourceLayer": "bundle" | "profile" | "home" | "todex" | "other", "patchedBy"?, "locked", "overridden", "toggleable", "configurable" }`：`source` 为声明该行的 bundle 包或补丁文件，`locked` 表示由 TodeX 覆盖层保持关闭（`session-log-deepseek`、`plugin-package-inventory-deepseek`），`overridden` 表示家目录层决定其开关，`conditional` 表示由 `!!js` 表达式决定；`dsh --dump-config` 失败时 `rows` 为空并带 `composeError`。`packages` 列出 profile 清单（`$DSH_HOME/profiles/acp/package.json`）：dsh 安装自带的 `@deepseek-ai/dsh-base`/`@deepseek-ai/dsh-acp-app`（`builtin: true`）与 `dependencies` 中的包 `{ "name", "spec"?, "version"?, "description"?, "bundle", "selected", "builtin" }`（`selected` 即在 `dsh.profile.bundles` 中）。`exemptions` 为 `compatibility.json` 中的精确版本豁免 `{ "package": "name@version", "runtimeVersions" }`。`runtimeVersion` 为 `dsh -V` 的输出，`pnpmAvailable` 表示 daemon 的 PATH 中有 `pnpm`（安装与移除需要）。`revision` 为补丁层内容的 SHA-256（不存在时为 `"missing"`），`parseError` 表示补丁层不是可逐条编辑的块列表（只能经 `/v2/providers/deepseek-harness/settings` 整体编辑）。

`PUT /v2/providers/deepseek-harness/plugins/state` 请求体 `{ "revision", "id", "enabled" }`：在补丁层为该行写入 `disabled` 覆盖（该行本身由补丁层插入时直接改写那一行）；`locked`、`conditional` 与家目录层决定的行返回 `409 CONFLICT`，不存在的行返回 `404`。`PUT .../plugins/bundles` 请求体 `{ "revision": <manifestRevision>, "name", "enabled" }` 在 `dsh.profile.bundles` 中选中（追加到末尾，与 dsh 插件管理器一致）或取消已安装的 bundle，依赖保留；dsh 自带 bundle 返回 `409`，未安装的包返回 `404`。两者校验 revision（不一致 `409 CONFLICT`）、持有 dsh 的 profile 写锁，成功时返回与 `GET` 相同的新列表。

`POST .../plugins/operations` 请求体 `{ "action": "install" | "remove" | "allow-version" | "revoke-version", "source", "runtimeVersion"?, "acceptRisk"? }`，在后台运行固定的 `dsh plugin --profile acp add <source>`、`remove <source>`、`allow-version <source> --dsh-version <runtimeVersion> --accept-risk` 或 `revoke-version <source> --dsh-version <runtimeVersion>`（不经 shell，工作目录为 daemon 数据目录，超时 10 分钟），立即返回 `{ "id", "action", "source", "status": "running", "startedAt" }`；`GET .../operations/{operationId}` 返回最近一次操作，`status` 变为 `succeeded` 或 `failed`（带 `finishedAt`、`error`；版本不兼容被拒时 `incompatible: [{ "package", "runtimeVersion" }]` 给出可授予的豁免）。`source` 为包名（可带版本）、git/tarball URL 或绝对路径，不以 `-` 或 `.` 开头，不含空白与控制字符，不超过 500 字符；豁免的 `source` 为精确的 `name@version`。`allow-version` 必须带 `acceptRisk: true`（客户端应先提示用户不兼容的插件可能导致崩溃或数据丢失）。安装与移除需要 `pnpm`，否则返回 `PROVIDER_UNAVAILABLE`。同一时间只允许一个操作；有 DeepSeek Harness 任务运行、另一操作进行中、DeepSeek Harness 正在启动或安装/升级时返回 `409 CONFLICT`；操作期间持有 DeepSeek Harness 的锁，其他 Agent 不受影响。尝试、拒绝、成功与失败均记入审计事件 `dsh.plugins.audit`。

`/v2/providers/deepseek-harness/plugin-config/{plugin}` 读写已知插件的设定：`web`（`searchProvider`: `deepseek-official` | `deepseek-search-api`，`fetchProvider`: `http` | `deepseek-fetch-api`）与 `web-deepseek-api`（`sendOriginalQuestion` 布尔值、`baseURL`）；其他名称返回 `404`。`GET` 返回 `{ "id", "revision", "config", "overridden" }`，`config` 为 dsh 组合后的有效值（只含上述字段）。`PATCH` 请求体 `{ "revision", "patch" }` 只接受上述字段（`null` 保持组合值），把合并后的整行配置写入补丁层——dsh 整行替换 `config`，部分写入会丢失 bundle 的其他字段；家目录层覆盖时返回 `409 CONFLICT`。以上所有写入都会改变 dsh 加载的代码或其联网行为，因此要求有效设备签名；改动对之后启动的 dsh 进程生效（TodeX 的常驻进程在下一个 turn 前重启）。

`/v2/providers/{provider}/instructions` 读写 daemon 主机上 CLI 的全局指令文件，供 CLI 管理页的 AGENTS.md 面板使用：`claude-code` 为 `$CLAUDE_CONFIG_DIR/CLAUDE.md`（缺省 `~/.claude/CLAUDE.md`），`codex` 为 `$CODEX_HOME/AGENTS.md`，`pi` 为 `$PI_CODING_AGENT_DIR/AGENTS.md`，`deepseek-harness` 为 `$DSH_HOME/AGENTS.md`（dsh 在 `$DSH_AGENTS_HOME/AGENTS.md` 与项目内文件之前读取）；其他 provider 返回 `UNSUPPORTED`。`GET` 返回 `{ "path", "exists", "revision", "text", "presetId"?, "drifted", "overridePath"? }`：`revision` 为内容的 SHA-256（不存在时为 `"missing"`），`presetId` 表示文件正跟随某个指令预设，`drifted` 表示预设上次写入后文件又被改动过（CLI 自身或手动编辑），`overridePath` 为存在时会被 CLI 优先读取的文件（Codex 的 `AGENTS.override.md`）。`PUT` 请求体 `{ "revision", "text" }` 写入自有内容并解除预设绑定。`PUT /v2/providers/{provider}/instructions/source` 请求体 `{ "revision", "presetId": string | null }`：给出 `presetId` 时把该预设内容写入文件并绑定（已绑定同一预设时等于用预设覆盖，清除 `drifted`），`null` 时解除绑定、文件原样保留。两种写入都要求 `revision` 与磁盘当前内容一致，否则 `409 CONFLICT`；成功时返回与 `GET` 相同的新文档。

`/v2/instruction-presets` 管理指令预设库（保存在 `data_dir/agent-instructions.json`，权限 0600）：`GET` 返回 `{ "presets": [{ "id", "name", "text", "revision", "updatedAt", "usedBy": ["codex", …] }] }`；`POST` `{ "name", "text" }` 新建并返回 `201` 与预设；`PUT /v2/instruction-presets/{id}` `{ "revision", "name", "text" }` 修改（`revision` 不一致返回 `409 CONFLICT`），返回 `{ "preset", "sync": [{ "provider", "status": "updated" | "drifted" | "failed", "message"? }] }`——内容变化时重写所有跟随该预设的文件，已 `drifted` 的文件不覆盖；`DELETE` 返回 `204`，跟随它的 CLI 解除绑定、文件内容保留。名称去除首尾空白后须为 1–80 个字符，文件与预设内容不超过 1 MiB，最多 50 个预设，否则 `400 INVALID_REQUEST`。指令会影响在主机上执行命令的 Agent，因此所有写入即便在 loopback 部署中也必须携带有效设备签名。

`GET /v2/providers/commands?provider=pi&workspace=/path` 会实时读取 Agent 命令目录。Pi 使用 RPC `get_commands` 返回扩展、Prompt Template 和 Skill；响应失败会作为 Provider 错误返回，成功结果中的 `sourceInfo` 会原样保留。Codex 返回与本机 CLI 版本同步的 TUI 命令适配目录。命令描述包含 `invocation`，客户端应据此选择原生 RPC、桌面动作或 Provider prompt，不要把所有 `/` 输入都当作普通 prompt。 已有会话应使用 `?conversationId=...`：后端先校验 owner，再从会话 manifest 确定 provider 与 workspace，忽略客户端同时提交的对应查询参数。Pi runtime 存在时由同一 worker 发送 `get_commands`，响应含 `catalogSource: "session"` 与 `runtimeId`；尚未启动时使用临时发现进程，返回 `catalogSource: "discovery"`。目录查询总时限为 8 秒。可验证的本地包会附带 `packageName` / `packageVersion`：版本取自实际资源附近的 `package.json`，显式加载的资源还会检查 manifest 的 Pi 入口；无法确认时省略，不从 npm spec 猜测。

`/v2/agent-providers` 实现 cc-switch 同款的多供应商/账户管理，当前覆盖 `codex`、`claude-code`、`grok-build`、`pi`、`opencode`、`deepseek-harness`。档案库持久化在 `$TODEX_AGENTD_DATA_DIR/agent-providers.json`（owner-only 0600），`settingsConfig` 是不透明的按 Agent 配置：Claude Code 为 `settings.json` 形状的对象，但档案只托管身份键：`env` 中全部 `ANTHROPIC_*`（`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL` 等）与 `CLAUDE_CODE_USE_BEDROCK`/`CLAUDE_CODE_USE_VERTEX`/`CLAUDE_CODE_USE_FOUNDRY`，以及顶层 `model`、`apiKeyHelper`；档案里的其他键照常保存但不写入 live，Codex 为 `{auth, config}`（分别对应 `~/.codex/auth.json` 与 `config.toml` 文本），Grok Build 同为 `{auth, config}`（`$GROK_HOME/auth.json`——设置 `GROK_AUTH_PATH` 时取该路径——与 `$GROK_HOME/config.toml`，`GROK_HOME` 缺省为 `~/.grok`）：官方订阅档案携带 `grok login` 写入 `auth.json` 的会话（通常经 `import-live` 捕获），API 档案不带 `auth`，由 `[models].default` 指向带 `api_key`（可选 `base_url`、`api_backend`）的 `[model.<id>]`，Grok 会优先使用该密钥而非会话。Pi/OpenCode 为各自 `models.json`/`opencode.json` 中的 provider 节点。DeepSeek Harness 为 `acp` profile 补丁层中的一条 dsh 模型路由，外加虚拟字段 `apiKey`（存入 `$DSH_HOME/.credentials.yaml`，0600，不进补丁文件）：id 为 `deepseek-official` 的档案是 dsh 内置的 DeepSeek 路由，`apiKey` 写入凭据引用 `DEEPSEEK_API_KEY`（联网搜索共用），其余字段（如 `baseURL`）为 `llm-deepseek` 行的 config；其他 id（小写字母、数字与 `-`，不得为 `deepseek-official`/`deepseek-account`）为 `llm-pi-ai` 行 `config.providers.<id>` 下的 OpenAI/Anthropic 兼容路由，形如 `{ "displayName"?, "api": "openai-completions" | "openai-responses" | "anthropic-messages", "baseURL", "apiKey"?, "headers"?, "compat"?, "models": [{ "id", "name"?, "contextWindow"?, "maxTokens"?, "input"?, "reasoningEfforts"? }] }`（`baseURL` 须为 http(s)，至少一个带 id 的模型），`apiKey` 存为凭据引用 `TODEX_DSH_<ID>_API_KEY` 并在路由中写 `apiKeyEnv`（dsh 会把名称含 `KEY` 的变量从它运行的命令的环境中剔除）；不带 `apiKey` 的路由保留自己的 `apiKeyEnv`。dsh 的路由 `config` 整行替换，TodeX 写入完整的 `llm-pi-ai`/`llm-deepseek` 行配置；家目录层覆盖这些行时返回 `409 CONFLICT`。

激活（`activate`）改写该 Agent 的全局配置文件，对 TodeX 拉起的会话和终端里直接运行的 CLI 同时生效；运行中的会话不受影响。独占型 Agent（Claude Code、Codex、Grok Build）先把当前 live 配置回填进旧档案再写入新档案，外部编辑与凭据（含 Codex/Grok `auth.json`）因此被保留可恢复；`auth` 缺失的 Codex/Grok 档案仅在回填成功后删除 `auth.json`。Claude Code 改为合并写入 `settings.json`：读取 live 及其 revision（文件字节的 SHA-256，与 `GET /v2/providers/claude-code/settings` 返回的 `revision` 相同）→ 删除 live 中的托管键 → 写入档案的托管键 → 写前复核 revision → 原子替换；其余键（hooks、permissions、statusLine、其他 `env` 变量等）属于本机配置，激活、保存当前档案、重新激活与删除档案都不会改动它们，也不会被回填进任何档案（切换时只回填托管键）。读写之间文件被外部修改则返回 `409 CONFLICT` 且 live 不变。写入会丢弃不存在于任何档案中的托管值时，先在同目录生成 `settings.json.todex-backup-<UTC 时间戳>`（0600，保留最近 5 份）。没有当前档案时首次激活，若 live 中有不属于任何档案的托管值，先备份，再按 `import-live` 的方式自动导入为 id `imported`（已占用则 `imported-2`…）、名称 `Imported` 的档案（已达 100 个档案上限则只备份）。删除当前 Claude 档案会从 live 剥离托管键并清空当前选中，删除非当前档案不动 live；Codex/Grok 删除档案仍保留 live。Grok 会在后台刷新 `auth.json` 中的会话令牌：`matchesCurrent` 只比较登录账户（各 scope 的 `auth_mode`/`user_id`，API key scope 另比较密钥）与 `config.toml`，令牌刷新不算漂移；编辑或重新激活当前档案且 `auth` 未改动时，沿用 live 中同账户的新令牌，避免写回已轮换的旧令牌。叠加型 Agent（Pi、OpenCode、DeepSeek Harness）在保存时即把 provider 节点同步进 live 文件，`activate` 只移动原生默认选中（Pi 写 `settings.json` 的 `defaultProvider`/`defaultModel`；OpenCode 写顶层 `model`；DeepSeek Harness 写 `acp` 行的 `{provider, model}`，对 `deepseek-official` 不指定模型时删除该覆盖、恢复 dsh 自带默认；可经请求体 `{"modelId": "..."}` 指定，缺省取首个声明模型）。删除叠加型档案同时移除 live 节点，并清理指向它的默认选中。

`GET /v2/agent-providers/{agent}/export` 导出该 Agent 的全部档案，用于在多台主机间同步：`{format: "todex.agent-providers", version: 1, agent, exportedAt, providers: [{id, name, settingsConfig, websiteUrl?, category?, notes?, icon?, iconColor?, sortIndex?}]}`，**密钥为明文**（其余接口一律打码），不含时间戳与当前选中。`POST /v2/agent-providers/{agent}/import` 接受同一文件：按 `id` 覆盖同名档案、新增其余档案，文件中没有的档案保留，当前选中不变（导入与当前档案同 id 的内容会像编辑一样重写 live 配置；叠加型 Agent 的节点同步进 live 文件）。整份文件先校验（格式与版本、`agent` 与路径一致、id 唯一、名称与大小限制、不得含 `__TODEX_MASKED__`、导入后不超过 100 个档案）再写入。两者与 CLI 升级一样要求有效设备签名，并写入 `agent_providers.transfer.audit` 审计事件。

`GET` 响应中按字段名模式（`api_key`/`token`/`secret`/`password`/`authorization`/`credential`/`bearer`，含 Codex/Grok `config` TOML 内的 `experimental_bearer_token`、`api_key`、Codex `http_headers` 与 Grok `extra_headers` 值及内联表中的同类字段，以及 Grok `auth.json` 各 scope 的 `key`）把密钥替换为 `__TODEX_MASKED__`；写回掩码值表示保留已存密钥，新档案不得携带掩码。`/{id}/models` 由后端携带档案凭据代理请求 `{base}/models`（Claude 与 DeepSeek Harness 的 `anthropic-messages` 路由为 `/v1/models`；Grok 取 `[models].default` 条目或其 `model_provider` 的 `base_url`/`api_key`，其次 `[endpoints].models_base_url` 与 `auth.json` 的 `xai::api_key`，仅有密钥时默认 `https://api.x.ai/v1`；订阅会话令牌不会被转发；不跟随重定向），客户端不经手真实密钥。`GET` 读取已存档案；`POST` 接受 `{"settingsConfig": {...}}` 用于保存前的预览拉取，掩码值按同 id 的已存档案或叠加型 live 节点还原。`import-live` 对独占型捕获当前 live 为新档案并记为当前（Claude Code 只捕获托管键，`matchesCurrent` 也只比较托管键，live 中的其他键不算漂移）；对叠加型把 live 中未托管节点按 `id` 收编。所有 live 写入为原子 owner-only 文件，叠加型编辑带内容 revision 校验——外部并发修改返回 `409 CONFLICT`，客户端应刷新后重试。

创建对话：

```json
{
  "provider": "codex",
  "workspace": "/home/user/projects/demo",
  "title": "Review backend",
  "providerProfile": null
}
```

不指定工作区（客户端的「None」选项）时省略 `workspace`，改传 `"scratchWorkspace": true`：后端为该对话创建 `$TODEX_AGENTD_DATA_DIR/workspace/<conversationId>` 并把它作为 workspace，返回的 manifest 带 `"scratchWorkspace": true`（为 `false` 时省略），`workspace` 是新目录的规范路径，`workspaceId` 照常按路径生成，因此每个对话各不相同，不会出现在 `/v2/workspaces` 中。`scratchWorkspace` 与 `workspace` 同时提供或都不提供时返回 `400 INVALID_REQUEST`。删除对话不会删除该目录。

`GET /v2/conversations` 不列出 Agent 调用产生的子会话（manifest 带 `parentConversationId`，见 [Agent 调用（MCP `todex_agents`）](#agent-调用mcp-todex_agents)），除非加 `?includeChildren=true`；按 id 访问子会话不受影响。

发送 prompt：

```json
{
  "text": "检查认证边界",
  "model": null,
  "reasoningEffort": null,
  "skills": [
    { "resourceId": "skill_abc", "name": "review" }
  ],
  "content": [
    { "type": "text", "text": "同时检查这个截图" },
    { "type": "localImage", "path": "screenshots/error.png" },
    { "type": "file", "path": "src/main.rs", "name": "main.rs" }
  ]
}
```

`skills` 可选。Backend 按 `resourceId` 读取 Skill；Codex 使用原生 `skill` input item，其他 Provider 使用受控文本注入，客户端不要拼接完整 Skill 文件。只带 Skill、不带 `text` 时允许发送。注入成功后会发出 `skill.injected` 事件；用户可见的 `message.created` 仍是原始输入。

`content` 可选，最多 16 项，支持 `text`、`localImage`、内联 `image`（`data` + `mimeType`）和 `file`。本地路径可相对 workspace，也可使用 workspace 内绝对路径，或 `PUT /v2/uploads` 返回的上传文件路径（见下文）；规范化后越出 workspace 和上传目录、符号链接逃逸和非普通文件都会拒绝。图片允许 PNG/JPEG/GIF/WebP，解码后合计最多 10 MiB。TodeX 不再按图片输入能力拦截提示：图片原样交给任何 Provider，所选 Agent 或模型不支持图片时由 Agent 自行报错（以 turn 失败或 Agent 消息呈现），daemon 不再返回 `UNSUPPORTED` 或 `IMAGE_INPUT_UNSUPPORTED`。`/v2/providers` 中的 `capabilities.imageInput` / `imageInputMode`、模型目录的 `imageInput` 以及 `/v2/providers/image-input` 仅供客户端提示参考（为兼容旧客户端保留），不影响请求是否被接受。Codex 把文件映射为原生 mention，其他 Provider 使用 workspace 相对 `@` 引用。

上传 prompt 附件：`PUT /v2/uploads?name=<文件名>&offset=0`，请求体是原始字节（`application/octet-stream`），每块最多 8 MiB，单个文件最多 100 MiB。首块不带 `uploadId`，后端新建上传；后续块带首块返回的 `uploadId`，`offset` 必须等于已写入的字节数，否则返回 `409 CONFLICT`，未知的 `uploadId` 返回 `404`。每块的响应都是 `{ "uploadId", "path", "name", "mimeType", "sizeBytes" }`，`path` 是 daemon 所在机器上的绝对路径，`name` 是清理后的文件名（只保留最后一个路径分量、去掉控制字符和开头的点，最长 200 字节）。文件存放在 `/tmp/todex/file/<uploadId>/<name>`（Windows 为 `%TEMP%\todex\file`），目录为 0700、文件为 0600；`/tmp/todex` 或其下目录是符号链接或属于其他用户时上传失败。客户端把 `path` 放进 prompt 的 `file`（或 PNG/JPEG/GIF/WebP 图片的 `localImage`）中，不需要在 workspace 内；上传图片除照常作为图片交给模型外，还在 prompt 文本中追加一行 `Attached image: <path>`（Antigravity 本来就逐张列出图片路径）。Claude Code 与 Antigravity 启动时以 `--add-dir` 获得该目录的读取权限，Antigravity 的审批 hook 在询问模式下也直接放行其中的读取。上传在最后一次写入 7 天后由 daemon 清理（启动时及每 6 小时检查一次）；重试或排队的 prompt 在此之前都能找到文件，`/tmp` 被系统清空后引用它的 prompt 返回 `400 INVALID_REQUEST`。经 `/v2/sealed` 访问时同样适用，外层请求体上限见下文。

每个 conversation 同时只允许一个 mutating turn；并发 `conversation.prompt` 返回 `409 CONFLICT`，不会排队。运行中要追加的消息改用后端追加队列（见下文 `conversation.queue.add`），它对所有 Provider 可用。daemon 重启按 journal（而非 manifest 状态）判断未完成 turn：仍打开的 turn 追加 `conversation.interrupted`（已知时携带该 turn 的 `turnId`），状态标记为 `interrupted`，不会通过重放 prompt 猜测恢复；journal 中已结束但 manifest 仍为 `running` 的会话只按 journal 修正状态，不追加事件。原生会话 ID 由 `provider-state.json` 保存，Provider 支持时下一 turn 使用原生 resume。

取消：`POST /v2/conversations/{conversationId}/cancel`（`/interrupt` 等价）的请求体可省略（空 body，无论是否带 JSON `Content-Type`），也可为 `{ "turnId": "..." }`；非空且不是合法 JSON 时返回 `400 INVALID_REQUEST`。WebSocket `conversation.cancel` / `conversation.interrupt` / `conversation.stop` 的 payload 同样接受可选 `turnId`。不带 `turnId` 时行为不变：取消当前活动 turn（空闲时为无操作），结果为 `{ "conversationId", "accepted": true }`。带 `turnId` 时只取消该 turn：它正是当前活动 turn 时结果为 `{ "conversationId", "accepted": true, "turnId", "cancelled": true }`；它已结束、当前运行的是另一个 turn 或会话空闲时不发送任何取消，结果为 `{ "conversationId", "accepted": true, "turnId", "cancelled": false, "activeTurnId": <当前活动 turn 的 id 或 null> }`——这不是错误，客户端按无操作处理。第一方客户端知道 turnId 时必须带上，避免迟到的取消误停下一个 turn；不带的形式保留给 TUI 与脚本。

每个已开始的 turn 都以一个终态事件结束：driver panic 产生 `turn.failed`（`code: "PROVIDER_PANIC"`，其他任务异常为 `PROVIDER_TASK_CANCELLED`）；终态事件写入失败时按 100 ms / 500 ms / 2 s 重试，每次重试前先查 journal 避免重复，全部失败则 manifest 状态置为 `failed`，重启恢复再在 journal 中关闭该 turn。`[agent].provider_idle_timeout_minutes`（默认 60，`0` 关闭）内 Provider 没有任何输出或事件的 turn 会被取消（有待答权限请求时不计时触发），30 秒内未结束则强制中止，最终以 `turn.failed`（`code: "PROVIDER_IDLE_TIMEOUT"`）结束。Provider stdout 中非 JSON 行或超过 4 MiB 的行不再使 turn 失败：每个 turn 最多 20 条记为 `provider.event`（`{ "kind": "invalid_line", "preview" }`，preview 已脱敏且不超过 512 字节；或 `{ "kind": "oversized_line", "bytes" }`），其余只写日志。Claude Code turn 内有过这类行时，turn 无论以何种方式结束都再记一条汇总 `provider.event` `{ "provider": "claude-code", "providerMethod": "stream_anomaly", "metadata": { "invalidLines", "oversizedLines" } }`，给出完整计数；它只统计 daemon 丢弃的行，Claude Code 自身在背压下丢弃的 `stream_event` 不在其中。

journal 不设总容量上限：追加永不因体积被拒绝，进行中的 turn 总能写完，单个会话也不再有体积上限。只有数据目录所在磁盘可用空间低于 1 GiB 时，新 prompt（保存请求快照时）返回 `STORAGE_LOW`（HTTP 507），释放磁盘空间后重试即可；`JOURNAL_FULL` 不再产生。超过 1 MiB − 16 KiB 的事件 payload 在脱敏后截断而非拒绝：最大的字符串按 UTF-8 边界截断并追加 `…[truncated N bytes]`，对象 payload 顶层增加 `truncated`（该键已被占用时为 `_truncated`）映射，记录 JSON pointer → 原始字节数；仅截断字符串仍放不下时，payload 替换为 `{ "truncated": true, "originalBytes": N }` 加上较短的顶层标量字段。

工具事件：一次工具调用以 `tool.started` 开始，`tool.updated` 携带参数流或累积的中间输出（Pi、ACP 每个工具最多每 500 ms 一次），以 `tool.completed` 或 `tool.failed` 结束；ACP 系 Provider 不发 `tool.completed`，以 `status` 为 `completed`/`failed` 的 `tool.updated` 结束，Pi 的失败以带 `isError: true` 的 `tool.completed` 表示。工具运行但没有输出时（例如命令把输出交给 `tail` 或长时间等待网络），Provider 不发送任何帧，daemon 也不补发心跳：静默期间不会产生事件。客户端用该工具 `tool.started` 事件的 `time` 计算已用时间，用最近一条同 `toolCallId` 事件的 `time` 判断静默时长；这两个时间在 `detail=summary` 与历史加密下都保持明文。静默超过 `[agent].provider_idle_timeout_minutes` 时按上文空闲超时取消该 turn。取消只作用于整个 turn（Pi 先发 `abort`，超时后终止进程组），此时仍打开的工具不会再收到终态事件，客户端应以 turn 终态事件结束其运行状态。

`tool.completed` 的工具输出在此之前另有上限：payload 中输出字段（`aggregatedOutput`、`output`、`result`、`partialResult`、`content`、`contentItems`、`structuredContent`、`rawOutput`、`stdout`、`stderr`、`formattedOutput` 之下的字符串，不含 `arguments`/`input` 等工具输入；短于 256 字节的字符串不计、不截；编码媒体也不计、不截：`data:` URL 字符串，`data`、`imageUrl`、`image_url`、`blob` 键之下的值，以及 `type` 为 `image`、`inputImage`、`input_image`、`audio`、`resource`、`blob` 的对象，它们只受上述 1 MiB payload 上限约束）按文档顺序视作一段文本，超过 256 KiB（UTF-8 字节）时只保留前 192 KiB 与后 32 KiB（均按字符边界截断），中间整段落在省略区的字符串变为空串，保留区开头处插入一行 `…[N bytes of tool output truncated]…`，payload 顶层增加 `truncated: { "originalBytes": N }`（`truncated` 已被占用时为 `_truncated`），N 为原始输出字节数。256 KiB 以内的输出不变。截断发生在加密之前，摘要密文、完整密文与实时推送都来自同一份截断后的 payload。

事件回放支持 `detail=summary`（默认 `full`）：summary 模式把只产生折叠过程行的事件（工具调用、思考、状态、进度）的 `payload` 替换为 `{ "detailStub": true, ... }` 占位对象，保留分类、turn 与流身份所需的元数据，因此事件 sequence 与投影出的时间线条目身份保持不变；结果输出、审批、权限、队列、配置、压缩、subagent、memory、extension 及携带用量数据的事件始终完整返回。客户端展开过程组时用同一接口按 `afterSequence`/`limit` 以 `detail=full` 拉取对应序列区间。

HTTP 响应在请求携带 `Accept-Encoding: gzip` 且响应体不小于 1 KiB 时使用 gzip 压缩（图片、gRPC 与 SSE 除外）；压缩只作用于响应体，设备签名覆盖的请求方法、路径、查询与请求体不受影响，WebSocket 升级不压缩。

`beforeSequence=N` 提供反向翻页（与 `afterSequence` 互斥，优先生效）：返回 `sequence <= N` 的最后 `limit` 条（升序），`hasMore` 表示是否还有更早的事件，下一页游标为本页首条 `sequence - 1`。用于长对话自下向上懒加载：首屏用 manifest 的 `lastSequence` 拉取尾页，滚动到顶部再继续向前翻页。

两个方向的回放页（以及 WebSocket 订阅补放的每一页）都同时受 `limit`（上限 1000）与约 8 MiB journal 字节限制，先到先止，但每页至少返回一条事件；因此一页可能少于 `limit` 条，客户端应在 `hasMore` 为 `true` 时继续翻页，而不是按条数判断是否结束。

### 历史存储与清理（`/v2/history/*`）

以下接口与其他 v2 路由一样需要设备签名，并且只作用于当前 tenant（`ownerId` 与调用方 `tenant_id` 相同）的会话；其他 tenant 的会话既不计入、也不能被删除。

`GET /v2/history/storage` 统计每个会话目录（`$DATA_DIR/conversations/<id>/`）的磁盘占用：递归累加普通文件大小，不跟随也不计入符号链接，遍历在阻塞线程池中进行。`conversations` 的每一项是与 `GET /v2/conversations` 完全相同的 manifest 字段再加 `bytes`，按 `updatedAt` 倒序；无法测量的会话记为 `bytes: 0` 并照常列出（daemon 记录警告）。

```json
{
  "totalBytes": 123456,
  "count": 42,
  "measuredAt": "2026-10-11T08:00:00Z",
  "conversations": [ { "id": "…", "ownerId": "local", "updatedAt": "…", "bytes": 2048 } ]
}
```

`POST /v2/history/cleanup` 批量删除指定会话，请求体 `{ "conversationIds": ["…"] }`，数量须为 1–500（重复 id 去重），否则返回 `400 INVALID_REQUEST`。每个会话都走与 `DELETE /v2/conversations/{id}` 相同的路径（关闭 Provider 会话、清除追加队列、撤销 Agent MCP 授权、关闭事件 hub 通道）；不能删除的会话不会让整批失败，而是进入 `skipped`，`code`/`message` 与单独删除时 API 返回的错误一致（运行中或等待授权为 `CONFLICT`，不存在或属于其他 tenant 为 `NOT_FOUND`）。`freedBytes` 是删除前测得的已删除会话占用之和。

```json
{
  "deleted": ["id1"],
  "skipped": [ { "conversationId": "id2", "code": "CONFLICT", "message": "resource is busy: active conversation cannot be deleted" } ],
  "freedBytes": 4096
}
```

`GET /v2/history/cleanup-policy` 读取、`PUT /v2/history/cleanup-policy` 保存（请求体即 `policy` 对象）当前 tenant 的自动清理策略，两者都返回：

```json
{
  "policy": { "enabled": false, "maxAgeDays": null, "maxTotalBytes": null },
  "lastRun": { "at": "2026-10-11T08:00:00Z", "trigger": "auto", "deleted": 3, "freedBytes": 1024, "error": null },
  "nextRunAt": "2026-10-11T09:00:00Z"
}
```

- 校验（失败为 `400 INVALID_REQUEST`）：`maxAgeDays` 为 `null` 或 1–3650 的整数；`maxTotalBytes` 为 `null` 或 1048576（1 MiB）到 2^53 的整数；`enabled: true` 时两者至少一个非 `null`。关闭的策略可以保留阈值。
- `lastRun` 从未运行时为 `null`；`trigger` 为 `auto`（后台任务）或 `manual`（手动运行）；整次运行失败时 `error` 为错误文本，否则为 `null`。`nextRunAt` 是后台任务下一次计划运行时间，策略关闭时为 `null`。
- 策略按 tenant 持久化在 `$DATA_DIR/history-cleanup.json`（owner-only 0600，原子替换）。文件缺失视为默认值（关闭、两个阈值均为 `null`）；文件损坏时记录警告并按默认值处理，不影响 daemon 启动。

`POST /v2/history/cleanup-policy/run` 立即按已保存策略的阈值清理一次（不论 `enabled`），两个阈值都为 `null` 时返回 `400 INVALID_REQUEST`；结果以 `trigger: "manual"` 记为 `lastRun`。响应为 `{ "deleted": [...], "skipped": [...], "freedBytes": N, "lastRun": {...} }`，`skipped` 形状同上（选中后会话已变化或开始运行时记为 `CONFLICT`）。

策略语义（手动运行与后台任务相同）：

- 运行期间持有 CLI 执行闸门的写锁（与 `history_retention_days` 清理相同），选定与删除之间不会有新 turn 或 fork 预留开始；运行中、等待授权或已预留的会话永不删除。
- 年龄规则：删除 `updatedAt` 早于「现在 − `maxAgeDays` 天」的会话。
- 容量规则：执行年龄规则后，若该 tenant 剩余总占用仍大于 `maxTotalBytes`，按 `updatedAt` 从旧到新继续删除可删除的会话，直到总占用不大于 `maxTotalBytes`；活动会话计入总占用但不会被删除，因此总占用可能仍高于上限。
- 每次删除都执行与单独删除相同的清理（Provider 会话以 `conversation_cleanup` 原因关闭、移除追加队列、撤销 MCP 授权、关闭 hub 通道）。

后台任务随服务启动，首次在启动约 5 分钟后运行，此后每小时运行一次，对每个已启用策略的 tenant 执行上述清理并以 `trigger: "auto"` 记录 `lastRun`（失败写入 `lastRun.error` 并记入日志）。`PUT` 保存为启用的策略会唤醒后台任务尽快运行一次，但 `PUT` 响应不等待清理完成。服务停止时后台任务随之终止。该策略与配置项 `history_retention_days`（`--history-retention-days`，按天对全部 tenant 的每日保留清理）相互独立，可以同时生效。

### Agent 原生历史（`/v2/native-history/*`）

以下接口管理 Claude Code、Pi、Codex、Grok Build、Devin 与 omp 自己保存的原生会话（不属于 TodeX 会话存储），与其他 v2 路由一样需要设备签名。路径中的 `{provider}` 只能是 `claude-code`、`pi`、`codex`、`grok-build`、`devin` 或 `omp`，其他值一律返回 `400 INVALID_REQUEST`。原生目录与 Provider 自身的解析方式一致，每次请求重新解析（daemon 启动后修改的环境变量同样生效）。Claude Code、Pi 与 omp 的会话文件由 TodeX 直接删除；Codex、Grok Build 与 Devin 的会话只通过各自 CLI 的官方删除命令删除（使用 daemon 配置的 `codex_bin` / `grok_bin` / `devin_bin`），TodeX 从不直接删除它们的文件：

- Claude Code：配置目录为 `$CLAUDE_CONFIG_DIR`（默认 `~/.claude`），会话文件为 `<claude_dir>/projects/<编码后的 cwd>/<sessionId>.jsonl`（只认文件名是 UUID 的 `.jsonl`，跳过符号链接目录）。按会话 id 严格关联、随会话一起计量和删除的附属数据（存在才计入）：同目录的 `<sessionId>/`（subagents、tool-results）、`session-env/<sessionId>/`、`file-history/<sessionId>/`、`todos/<sessionId>-*.json`、`debug/<sessionId>.txt`。其他文件（`.credentials.json`、`settings.json`、`history.jsonl`、plugins、skills、memory 等）永不触碰。
- Pi：会话目录依次取 `$PI_CODING_AGENT_SESSION_DIR`、`<pi_dir>/settings.json` 的 `sessionDir`（支持 `~/`，相对路径基于 `pi_dir`）、`<pi_dir>/sessions`，其中 `pi_dir` 为 `$PI_CODING_AGENT_DIR`（默认 `~/.pi/agent`）。只把首行能解析为 `{"type":"session","id",…}` 头的 `.jsonl` 视为会话（扫描会话目录及其一级子目录），其他文件（如 `permission-forwarding/`）一律忽略；会话 id 取自文件头，删除会话即删除该 `.jsonl`（与 Pi 自身一致）。
- omp：会话目录固定为 `~/.omp/agent/sessions`（omp 的目录覆盖变量沿用 Pi 的名字，为 Pi 设置，因此不读取）。布局同 Pi（`<编码后的 cwd>/<时间>_<id>.jsonl`）；当前格式的文件首行是定宽的 `{"type":"title",…}` 标题槽，其后才是会话头，旧文件直接以会话头开始，两种都识别。标题依次取标题槽、会话头的 `title`、首条用户消息。会话同名的 `<时间>_<id>/` artifacts 目录（工具输出、子 agent 记录）随会话一起计量和删除；`~/.omp/agent/blobs/` 为跨会话共享的内容寻址存储，从不删除（由 `omp gc` 回收）。
- Codex：主目录为 `$CODEX_HOME`（默认 `~/.codex`），只读扫描 rollout 文件 `sessions/YYYY/MM/DD/rollout-<时间>-<thread id>.jsonl` 与已归档的 `archived_sessions/rollout-*.jsonl`（会话 id 即文件名末尾的 thread UUID，即 TodeX Codex 会话的 `nativeSessionId`）；首行须为 `session_meta`（提供 `cwd`、`createdAt`）。不打开 Codex 的 `state_<N>.sqlite`（运行中的 app-server 可能持有它），也不启动 app-server（其 `thread/list` 默认只返回交互来源且有预览的线程）。线程名（`thread/name/set`、`/rename`）取自追加写入的 `session_index.jsonl`（同一 id 以最后一行为准，空名表示清除）。`bytes` 为 rollout 文件大小（Codex 0.162 起的分页历史还会在 `thread_history_1.sqlite` 中保存索引，不计入）。删除执行 `codex delete --force <id>`（`--force` 跳过确认且要求 UUID），子进程的 `CODEX_HOME` 固定为列表所读的目录；该命令会一并删除 rollout、SQLite 中的线程记录与 `session_index.jsonl` 条目，已用临时 `CODEX_HOME` 实测可离线删除未归档、已归档及未建索引的 rollout。
- Grok Build：主目录为 `$GROK_HOME`（默认 `~/.grok`），只读扫描 Grok 自己的磁盘索引 `sessions/<URL 编码的 cwd>/<session id>/summary.json`（cwd 过长时目录名为 slug+hash，并另有 `.cwd` 文件，均不影响扫描）；跳过 Grok 自己在列表中隐藏的会话（`hidden: true`，或未显式 `hidden: false` 的 `subagent*` 类会话——`grok sessions delete` 同样找不到它们）。`bytes` 为整个会话目录的大小（`updates.jsonl`、`chat_history.jsonl`、检查点等，不跟随符号链接）。删除执行 `grok --no-auto-update sessions delete <id>`（该命令不提示确认），子进程设置 `GROK_HOME`（及已设置的 `GROK_AUTH_PATH`）与 `grok_env_allowlist` 中的变量。**注意：已登录且非 ZDR 团队时，Grok 会先删除该会话的云端副本，云端删除失败则整个删除中止、本地文件不动**（该项以 `PROVIDER_UNAVAILABLE` 跳过，可重试）。Grok 自带按空闲天数清理会话目录的设置 `config.toml` 的 `[storage] cleanup_ttl_days`（未设置或 0 为关闭）；TodeX 目前不读写该设置，它也不会考虑 TodeX 的引用。
- Devin：本地会话保存在 SQLite 数据库 `$XDG_DATA_HOME/devin/cli/sessions.db`（默认 `~/.local/share/devin/cli`，含 `-wal` / `-shm`），TodeX 不读取该数据库。列表通过一个短时 `devin acp` 进程完成：`initialize` 后分页调用 ACP `session/list`（无需登录；每页 50 条，按 `nextCursor` 翻页，整个过程 30 秒超时，结束即终止进程）。`devin list --format json` 只列当前目录的会话，因此不使用。会话 id 即 ACP session id（TodeX Devin 会话的 `nativeSessionId`）；`_meta["cognition.ai/isLocked"]` 为 true（另一个 Devin 进程打开着该会话）时视为运行中。删除执行 `devin rm --force <id>`（`--force` 跳过确认；另一个 Devin 实例打开着该会话时 Devin 拒绝删除）。只覆盖本地会话，`devin --cloud` 云端会话不在范围内。

`GET /v2/native-history/providers`：

```json
{
  "providers": [
    {
      "provider": "claude-code",
      "available": true,
      "root": "/home/u/.claude/projects",
      "sessionCount": 17,
      "totalBytes": 118000000,
      "measuredAt": "2026-10-11T08:00:00Z",
      "capabilities": { "view": true, "sizes": true },
      "native": { "cleanupPeriodDays": 5, "settingsPath": "/home/u/.claude/settings.json" }
    },
    { "provider": "pi", "available": false, "root": "/home/u/.pi/agent/sessions", "sessionCount": 0, "totalBytes": 0, "measuredAt": "…", "capabilities": { "view": true, "sizes": true }, "native": null },
    { "provider": "codex", "available": true, "root": "/home/u/.codex/sessions", "sessionCount": 4, "totalBytes": 2100000, "measuredAt": "…", "capabilities": { "view": true, "sizes": true }, "native": null },
    { "provider": "grok-build", "available": false, "root": "/home/u/.grok/sessions", "sessionCount": 0, "totalBytes": 0, "measuredAt": "…", "capabilities": { "view": true, "sizes": true }, "native": null },
    { "provider": "devin", "available": true, "root": "/home/u/.local/share/devin/cli", "sessionCount": 9, "totalBytes": 5300000, "measuredAt": "…", "capabilities": { "view": false, "sizes": false }, "native": null }
  ]
}
```

`providers` 固定按 `claude-code`、`pi`、`codex`、`grok-build`、`devin`、`omp` 顺序列出全部六项。

- `available` 表示该 Provider 的历史存储存在：Claude/Pi/Grok 为 `root` 目录存在；Codex 为 `sessions/` 或 `archived_sessions/` 存在；Devin 为 `sessions.db` 存在、`devin_bin` 可执行且 `session/list` 成功（失败只记警告）。不可用时会话列表返回 `count: 0, sessions: []`，不是错误。
- `capabilities.view`：转录接口可用（Devin 为 `false`）。`capabilities.sizes`：已知每个会话的字节数（Devin 为 `false`）。
- `totalBytes`：`sizes` 为 `true` 时为各会话 `bytes` 之和（Claude 含上述附属数据，不跟随符号链接）；为 `false` 时为整个历史存储的大小（Devin 为 `sessions.db` 及其 `-wal` / `-shm`，无法测量时为 0）。
- `native` 仅 Claude 有（其他为 `null`）：`cleanupPeriodDays` 为 `settings.json` 中的数值，缺失或文件无法解析时为 `null`。

`GET /v2/native-history/{provider}/sessions`：

```json
{
  "provider": "pi", "root": "…", "totalBytes": 0, "count": 0, "measuredAt": "…",
  "sessions": [
    { "id": "uuid", "title": "…", "cwd": "/work", "createdAt": "…", "updatedAt": "…", "bytes": 1234, "referencedBy": ["todex-conversation-id"], "running": false }
  ]
}
```

- `bytes` 类型为 `number | null`：`capabilities.sizes` 为 `false`（Devin）时恒为 `null`；`totalBytes` 含义同上。
- 按 `updatedAt` 倒序：Claude/Pi/Codex 为会话文件 mtime，Grok 为 `summary.json` 与 `updates.jsonl` 中较新的 mtime，Devin 为 `session/list` 的 `updatedAt`（缺失时按刚写入处理）。`title`：Claude 取最新的 `custom-title` / `ai-title` / `summary` 记录，否则取第一条真实用户消息（跳过 meta、命令/通知包装、tool result 与 sidechain）；Pi 取最新的 `session_info.name`，否则取第一条用户消息；Codex 取线程名，否则取第一条真实用户消息（跳过 Codex 以用户身份注入的 `<environment_context>` 等 `<…>` 包装与 AGENTS.md 指令）；Grok 取 `generated_title`，否则取 `session_summary`；Devin 取 `session/list` 的 `title`。空白折叠为单行，最多 200 字符，没有时为 `null`。`cwd`、`createdAt` 来自 Claude 首条记录、Pi 文件头、Codex `session_meta`、Grok `summary.json`（`info.cwd`、`created_at`）或 Devin 的 `cwd` / `_meta["cognition.ai/createdAt"]`，可能为 `null`。
- 列表只读取每个文件开头和末尾各 256 KiB（Codex 只看开头），并按（路径、大小、mtime）在内存中缓存解析结果，因此超大文件中间的标题可能不会被识别。
- `referencedBy`：当前 tenant 中 Provider 相同（`codex`、`grok-build`、`devin` 分别对应 TodeX 的 Codex、Grok Build、Devin 会话）、恢复该原生会话的 TodeX 会话 id（`provider-state.json` 的 `nativeSessionId`，尚未记录时为会话 id 本身——Claude/Pi 驱动首轮即以它作为会话 id）。
- `running`：Claude 的 `<claude_dir>/sessions/<pid>.json` 标记指向该会话且该 pid 仍存活（Linux 检查 `/proc/<pid>`，其他 Unix 用 `kill(pid, 0)`，无法检查的平台一律视为存活）；Devin 的 `session/list` 报告该会话被锁定（`cognition.ai/isLocked`）；或引用它的 TodeX 会话正在运行/等待授权；或会话在最近 2 分钟内被写入（同 `updatedAt`）。

`GET /v2/native-history/{provider}/sessions/{id}/transcript?limit=200`：

```json
{
  "provider": "pi", "id": "uuid", "title": "…", "cwd": "…", "totalMessages": 523, "truncated": true,
  "messages": [ { "role": "user", "text": "…", "at": "2026-10-11T08:00:00Z" } ]
}
```

- `limit` 为 1–1000 的整数（默认 200），否则 `400 INVALID_REQUEST`；id 格式非法为 `400 INVALID_REQUEST`，不存在为 `404 NOT_FOUND`；`capabilities.view` 为 `false` 的 Provider（Devin）返回 `501 UNSUPPORTED`。
- 逐行流式读取文件，只保留最后 `limit` 条可显示消息（按时间正序）；`totalMessages` 为可显示消息总数，`truncated` 表示更早的消息被省略。
- 映射：用户/助手文本块 → `user`/`assistant`（图片显示为 `[image]`）；工具调用 → `tool`，文本为 `name: <JSON 参数>`（最多 300 字符）；工具结果 → `tool`，最多 500 字符预览；Claude `system` 记录、压缩摘要与 Pi compaction/branch 摘要 → `system`；thinking、Claude sidechain（`isSidechain: true`）与 meta 记录、Pi 系统提示均跳过。每条文本最多 8000 字符，超出部分以 `…` 截断。`at` 为记录时间戳，缺失时为 `null`。
- Codex 读取 rollout 的 `response_item` 记录：用户消息（同样跳过注入的 `<…>` 上下文与 AGENTS.md 指令）→ `user`；助手消息 → `assistant`；`function_call` / `custom_tool_call` / `local_shell_call` / `web_search_call` → `tool`（`name: <参数>`）；`function_call_output` / `custom_tool_call_output` → `tool` 结果预览；`compacted` 记录 → `system`；developer/system 消息、`reasoning` 以及重复同一内容的 `event_msg` 记录均跳过。
- Grok 读取 `updates.jsonl`（每行一条持久化的 ACP `session/update` 或 `_x.ai/session/update` 通知，`timestamp` 为 Unix 秒）：连续的 `user_message_chunk` / `agent_message_chunk` 合并为一条 `user` / `assistant` 消息（`at` 取第一块的时间；宿主注入的 `hostTurn` 用户块跳过）；`agent_thought_chunk` 跳过且不打断合并；`tool_call` → `tool`（`title: <rawInput>`）；同一调用第一次 `completed` / `failed` 的 `tool_call_update` → `tool` 结果预览（内容文本，否则 `rawOutput`）。`/rewind` 撤销的轮次仍会出现在转录中。

`POST /v2/native-history/{provider}/cleanup` 删除指定会话，请求体 `{ "sessionIds": ["…"] }`，去重后数量须为 1–500，且每个 id 只能由 `[A-Za-z0-9._-]` 组成、不以 `.` 或 `-` 开头、不含 `..`（最长 128），否则整个请求返回 `400 INVALID_REQUEST`。响应与 `/v2/history/cleanup` 同形（`skipped[].conversationId` 存放的是原生会话 id）：

```json
{
  "deleted": ["id"],
  "skipped": [ { "conversationId": "id", "code": "CONFLICT", "message": "resource is busy: session is running" } ],
  "freedBytes": 123
}
```

- 被任意 tenant 的 TodeX 会话引用 → `CONFLICT`（`a TodeX conversation uses this session`）；正在运行（同上 `running` 判定）→ `CONFLICT`（`session is running`）；扫描中找不到 → `NOT_FOUND`。只要该 Provider 有任一 TodeX 会话的 manifest 或 provider state 读取失败（不存在除外），无法确定它引用哪个原生会话，该 Provider 的所有会话都以 `CONFLICT`（`… could not be read …`）跳过，策略运行同样不删除。删除 Pi 会话会让引用它的 TodeX 会话永久无法继续，因此引用保护不可绕过。
- Codex / Grok / Devin 通过 CLI 删除（见上文，单个删除 60 秒超时）：CLI 未安装 → `UNSUPPORTED`（不会退回到直接删除文件）；非零退出时取 stderr（其次 stdout）最后一行 `Error: …`（否则最后一个非空行），脱敏并截断到 300 字符作为 `message`：含 `no session found` / `not found` → `NOT_FOUND`，含 `already open` / `another process`（Devin 拒绝删除被打开的会话）→ `CONFLICT`，其他 → `PROVIDER_UNAVAILABLE`。`grok sessions delete` 找不到会话时也以 0 退出并输出 `No session found…`，按 `NOT_FOUND` 处理。`freedBytes` 计入会话的 `bytes`（Devin 为 0）。
- 安全规则（Claude/Pi）：id 先校验再通过一次全新的会话扫描解析为路径；每个待删路径都按其规范化（canonicalize）后的父目录校验必须位于 Provider 根目录内（Claude 会话文件为 `<claude_dir>/projects`，附属数据为 `<claude_dir>`；Pi 为会话目录）；符号链接从不跟随也不删除（扫描用 `symlink_metadata`，符号链接本身被拒绝）。先删会话文件，再尽力删除附属数据（失败只记警告）。每次删除写一条 `native history session deleted` 的 info 日志（provider、session_id、freed_bytes、trigger）。删除与策略运行、设置修改互斥串行。

自动清理策略（由 TodeX 管理，按 Provider、全 daemon 共享，不区分 tenant）：`GET`/`PUT /v2/native-history/{provider}/policy` 与 `POST /v2/native-history/{provider}/policy/run` 的请求、响应与校验和 `/v2/history/cleanup-policy`、`/v2/history/cleanup-policy/run` 完全相同（`policy` 为 `{enabled,maxAgeDays,maxTotalBytes}`，响应 `{policy,lastRun,nextRunAt}`；运行响应 `{deleted,skipped,freedBytes,lastRun}`）。

- 策略持久化在 `$DATA_DIR/native-history-cleanup.json`（owner-only 0600，原子替换），文件缺失或损坏按默认值（关闭）处理并记录警告。
- 语义与 TodeX 策略一致：先按年龄规则删除 `updatedAt` 早于「现在 − `maxAgeDays` 天」的会话，再按容量规则从旧到新删除直到总占用不大于 `maxTotalBytes`。被引用或运行中的会话计入总占用但永不删除；选定后文件又被写入的会话以 `CONFLICT` 跳过（Devin 没有可检查的文件，由 `devin rm` 拒绝删除被打开的会话）。
- `capabilities.sizes` 为 `false` 的 Provider（Devin）没有容量规则：`PUT` 中 `maxTotalBytes` 非 `null` 时返回 `400 INVALID_REQUEST`。
- 不另起定时器：`/v2/history/*` 的后台任务每次运行在处理各 tenant 策略之后，也会对每个已启用的原生策略运行一次（`trigger: "auto"`），`nextRunAt` 即该任务的下一次运行时间；`PUT` 保存为启用的原生策略同样会唤醒它。

Claude 自身保留期：`GET`/`PUT /v2/native-history/claude-code/native-retention`，请求与响应均为 `{ "cleanupPeriodDays": 30, "settingsPath": "/home/u/.claude/settings.json" }`。

- `PUT` 请求体必须包含 `cleanupPeriodDays`，取值为 `null` 或 1–3650 的整数（否则 `400 INVALID_REQUEST`，`settingsPath` 会被忽略）；`null` 表示删除该键，恢复 Claude 的默认值。
- 修改 `<claude_dir>/settings.json` 时保留其他所有键（及其顺序）和文件原有权限，原子替换；文件不存在时以 0600 新建；`settings.json` 是符号链接时修改其目标文件。文件存在但不是合法 JSON 对象时 `GET`/`PUT` 都返回 `409 CONFLICT`，且不改动文件。
- 其他 Provider 没有对应设置，`/v2/native-history/{pi|codex|grok-build|devin}/native-retention` 返回 `400 INVALID_REQUEST`（Grok 的 `[storage] cleanup_ttl_days` 见上文，暂不提供接口）。
- 注意：Claude Code 按 `cleanupPeriodDays` 在自身启动时清理旧会话，**不会**考虑 TodeX 的引用；被 TodeX 会话引用的 Claude 会话同样会被它删除。TodeX 的原生清理策略和手动删除才会保护被引用的会话。

### v2 WebSocket

客户端命令 envelope：

```json
{
  "id": "request-1",
  "type": "conversation.subscribe",
  "payload": {
    "conversationId": "00000000-0000-4000-8000-000000000000",
    "afterSequence": 0,
    "limit": 500,
    "detail": "full",
    "backfillLimit": 2000
  }
}
```

`conversation.subscribe` 的 `limit` 只是 replay 分页大小。可选 `detail`（`full` 默认 | `summary`）按 HTTP `detail=summary` 同样的规则折叠首次 replay 的过程事件；实时事件与缺口/滞后补放始终完整。可选 `backfillLimit` 限制首次 replay 的事件数：从 `afterSequence` 之后按 sequence 升序最多发送 `backfillLimit` 条，不跳过也不只发最新事件。结果为 `{ "conversationId", "subscribed": true, "nextSequence", "hasMore", "lastSequence" }`：`nextSequence` 是最后一条已 replay 的 sequence（未截断时等于高水位），`hasMore` 表示 `nextSequence` 与 `lastSequence`（订阅时的 journal 高水位）之间仍有未发送事件，实时推送从 `lastSequence` 之后继续。`hasMore: true` 时客户端用 HTTP `afterSequence=nextSequence` 翻页补齐 `(nextSequence, lastSequence]`，服务端不会把这段当作序号缺口补放。两个字段都省略时行为与之前完全一致（`hasMore` 恒为 `false`）。

支持 `conversation.subscribe`、`conversation.unsubscribe`、`conversation.create`、`conversation.prompt`、`conversation.queue.add` / `remove` / `clear` / `resume` / `list`、`conversation.schedule`、`conversation.resume`、`conversation.cancel`、`conversation.stop`、`conversation.runtime.stop`、`conversation.warm`、`conversation.permission.respond`、`mcp.list`、`mcp.refresh`、`mcp.call`、`server.ping` 和 `history.*`（见下文「历史加密密钥」）。服务端返回 `server.result`、`server.error` 与按 conversation 隔离的 `conversation.event`。订阅会先 replay，再接续实时 sequence；实时广播滞后时会从最后已交付 sequence 自动补放到当前高水位，补放失败则发送错误并移除该订阅，避免静默缺事件。 `conversation.event` 外层新增 `delivery: "live" | "replay"`：首次订阅、序号缺口和广播滞后的补放均为 `replay`，事件日志 payload 不变。客户端只对明确为 `live` 且未处理的事件执行 toast 或编辑器填充等瞬时操作。

`conversation.resume` 的 payload 为 `{ "conversationId" }`。daemon 不实现原生恢复：会话存在且属于当前租户时总是以 `server.error` `UNSUPPORTED` 应答，提示改为提交显式的 follow-up 继续对话，会话状态与 journal 都不变；`conversationId` 不是 UUID v4 时返回 `INVALID_REQUEST`，会话不存在或不属于当前租户时返回 `NOT_FOUND`，payload 缺少 `conversationId` 时与其他会话命令一样返回 `SERIALIZATION_FAILED`。断线后恢复事件流请用 `session.resume`（见「统一 WebSocket 命令面」）或带 `afterSequence` 的 `conversation.subscribe`。

命令执行顺序：`server.ping`、`conversation.subscribe`、`conversation.unsubscribe` 与 `session.resume` 由读循环按到达顺序直接处理；`mcp.refresh`、`mcp.call`、`conversation.control`、`conversation.warm` 在后台并发执行（每条 socket 最多 16 个，超出返回 `RESOURCE_EXHAUSTED`）；其余命令按 payload 的 `conversationId` 分道执行——同一会话的命令严格按到达顺序逐条执行，不带 `conversationId` 的命令共用一条通道，不同通道互不等待。因此某个会话的慢命令（如等待 Agent 启动）不会延迟 `server.ping` 或其他会话的命令。每条通道最多 16 条命令排队或执行中，超出时该命令立即返回 `RESOURCE_EXHAUSTED`，不会阻塞读循环；每条 socket 合计最多 64 条分道命令排队或执行中，超出时服务端暂停读取后续消息直到有空位，不返回错误。`conversation.cancel` / `interrupt` / `stop` 不排在所属会话的通道后面：立即执行并据此应答，同时在该会话通道中按顺序追加一次同样 payload 的静默取消（不另发应答），使排在它之前、尚未登记 turn 的 prompt 也会被取消；socket 已满 64 条时退回普通分道排队。`conversation.permission.respond` 不进入通道，立即执行（计入 64 条，已满时返回 `RESOURCE_EXHAUSTED`）。响应可能不按请求顺序到达，客户端须按 `id` 匹配。连接断开时已开始的分道命令会执行完毕，只是结果不再送达；仍在排队、尚未开始的命令不再执行。

每条 socket 最多同时持有 128 个会话订阅，超出后 `conversation.subscribe` 返回 `INVALID_REQUEST`。`conversation.unsubscribe` 的 payload 为 `{ "conversationId": "..." }`，释放该订阅槽位并停止转发任务，幂等且返回 `{ "conversationId", "unsubscribed" }`；订阅任务因补放失败终止时发出的 `server.error` 在 `payload.conversationId` 中携带会话 ID，客户端可据此清理本地订阅记录并重订阅。删除会话（含过期清理）会关闭其广播通道，等同于终止该会话的全部订阅。

订阅补放在该订阅自己的任务中执行，不阻塞读循环：同一订阅的帧顺序不变（补放帧 → 带请求 `id` 的订阅结果 → 实时事件），但其他命令的应答可能穿插其间。每条连接最多同时进行 4 个补放，其余排队。已订阅（含补放进行中）的会话再次 `conversation.subscribe` 立即返回 `{ "conversationId", "subscribed": true, "alreadySubscribed": true }`；补放期间 `conversation.unsubscribe` 会先以 `server.result` `{ "conversationId", "subscribed": false, "cancelled": true }` 应答那条挂起的订阅请求。广播通道只在有订阅者时存在（发布不会创建通道，最后一个订阅者离开即回收），容量 256，滞后部分从 journal 补放；滞后时发出的 `EVENT_STREAM_LAGGED` `server.error` 帧没有顶层 `id`，`payload.conversationId` 标明所属会话。

发送侧有截止时间：单次 socket 发送超过 20 秒，或出站队列持续满 10 秒，连接即被关闭；关闭时最多等待 2 秒让发送任务排空，随后中止。

MCP 真实调用只走 Backend：客户端只发送 `resourceId`、`toolName` 和对象类型的 `arguments`。Catalog JSON 不含 command、URL 或凭据。调用前必须通过权限 broker，默认拒绝；仅 `allow_once` / `allow_always` 会放行。Backend 使用标准 MCP SDK 连接 stdio JSONL 或 Streamable HTTP transport，并对初始化、调用和关闭分别设置时限。

### 后端追加队列

Provider 能力中的 `backendQueue: true` 表示 daemon 为该会话保存追加消息（所有 Provider 都支持；`followUpQueue` 仍只表示 Provider 原生队列）。队列只保存在 daemon 内存中，不写入磁盘（明文由 daemon 持有至投递，见 [history-encryption.md](history-encryption.md) §1），保存完整请求（含附件与 Skill），fork 不复制，删除会话一并丢弃；daemon 重启或崩溃会丢失全部未投递的追加消息。旧版 daemon 留下的 `queue.json` 在启动时一次性导入内存（按 `daemon_restarted` 暂停）并删除。能力 `backendQueueControl: true`（恒为 true）表示还支持 `conversation.queue.pause`、`conversation.queue.take` 与 `add` 的 `paused`。每个会话最多 32 条、序列化后合计 32 MiB，超出返回 `RESOURCE_EXHAUSTED`。

- `conversation.queue.add`：payload 与 `conversation.prompt` 相同，另加 `itemId`（1–200 字节，省略时用命令 `id`）、可选 `front: true`（插到队首）与可选 `paused: true`（入队并以 `user` 暂停，即使会话空闲、队列为空也不开始；队列已被用户持有的暂停占用时保持原因不变）。入队前按 prompt 的规则校验内容、路径与 Skill，不合法立即拒绝。会话空闲且队列为空时直接开始，结果为 `{ "conversationId", "itemId", "status": "started", "turnId" }`；否则入队，结果为 `{ ..., "status": "queued" }`。同一 `itemId` 已在队列中或已投递时幂等返回其状态，不会重复执行。
- `conversation.queue.remove`（`itemId` 必填，不存在返回 `NOT_FOUND`）、`conversation.queue.pause`、`conversation.queue.clear`、`conversation.queue.resume`、`conversation.queue.list`：payload 为 `{ "conversationId", "itemId"? }`，结果为 `{ "conversationId", "queue": <快照> }`。`resume` 解除暂停，会话空闲时立即开始队首。`pause` 以 `user` 暂停：队列为空时不产生暂停（快照 `paused` 仍为 false），已处于 `turn_cancelled` / `start_failed` / `daemon_restarted` / `user` / `scheduled` 暂停时不改原因。
- `conversation.queue.take`（`itemId` 必填，不存在返回 `NOT_FOUND`；旧版明文会话同 `remove` 可用）：原子移出该项并返回 `{ "conversationId", "itemId", "item": { "id", "text", "content": [<提示内容引用，与入队时一致：text / localImage / image{data,mimeType} / file>], "skills": [{ "resourceId", "name" }] }, "queue": <快照> }`，并追加 `followups.updated`。移出后队列为空则解除暂停（同 `remove`）。

快照为 `{ "items": [{ "id", "text", "status": "queued", "queuedAt", "contentCount", "skills" }], "paused", "pauseReason", "pauseMessage", "resumeAt" }`，不含内联图片数据。每次变化都会追加 `followups.updated` 事件（payload 即快照）；懒加载窗口可能不含最近一次该事件，客户端打开会话或重连后应调用 `conversation.queue.list` 取当前快照。

`pauseReason: "user"`（`pause` 或 `add` 的 `paused`）由用户持有：`turn.completed`、额度窗口重置都不会解除，只有 `resume`、`clear` 或移空队列才会；额度续写项照常插入队首，但不改原因、不设定时器。

派发规则：turn 以 `turn.completed` 结束（或原生压缩结束）后，daemon 以队列项的 `itemId` 作为 `clientRequestId` 开始队首，成功后移出队列。`turn.failed` / `turn.cancelled` / `turn.interrupted` 使队列暂停（`pauseReason` 为 `turn_failed` / `turn_cancelled` / `turn_interrupted`）；队首无法开始时保留在队首并暂停（`start_failed`，`pauseMessage` 为原因）；导入旧版 `queue.json` 的队列以 `daemon_restarted` 暂停。daemon 开始关停后不再从队列取下一项（关停期间完成的 turn 也不会带起队首）；自动更新判定"空闲"时除没有运行中的 turn 外，还要求没有任何非空队列（含已暂停的），避免空闲自重启丢掉追加消息。暂停期间空闲会话仍可直接 `conversation.prompt`，该 turn 完成后队列保持暂停（队列已空时除外，见下文）。

额度续写：turn 失败且该 turn 最近一次 `quota.updated` 显示套餐窗口已用尽时，daemon 不按普通失败处理，而是在队首插入一条续写项（`id` 为 `rate-limit-continue-<turnId>`，沿用失败请求的模型、思考强度与权限设置，正文为固定英文续写指令，不重复附件与 skills）——队列为空时也插入，已有续写项时不重复插入——并以 `rate_limited` 暂停，`resumeAt`（ISO 8601）为续写项可开始的时间。到点后 daemon 自行解除暂停并开始队首（按墙钟时间每 30 秒复核，机器休眠后也会补上），等待计时器同样只在内存中，重启后队列与等待一并丢失。等待期间客户端可移除续写项或 `resume` 提前开始；若期间有 turn 以 `turn.completed` 结束（用户已手动继续），续写项被移除、暂停解除。具体规则：

- 用尽判定：快照带 `status` 时只有 `status: "rejected"` 算用尽（`allowed` / `allowed_warning` 即使某窗口达到 100 % 也不算）；不带 `status` 时需某窗口 `usedPercent` ≥ 100 且 `isUsingOverage` 不为 `true`。
- 等待下限：`resumeAt` = max(窗口重置时间, 现在 + 60 秒)，重置时间已过去时也至少等 60 秒。Claude 只在错误文本中给出重置时刻（如 "resets 5:30pm (UTC)"）时，解析出的时刻早于现在但不超过 10 分钟按"现在"处理（再套 60 秒下限），不再顺延到次日或下周。
- 退避与上限：续写项本身再次因用尽失败时，第 n 次（n 从 1 起）等待 max(重置时间, 现在 + 60 秒 × 2^(n−1))。连续 3 个续写项都因用尽失败后不再插入续写项，队列以 `turn_failed` 暂停（队列为空时也暂停），`pauseMessage` 说明已停止自动续写；任一 turn 以 `turn.completed` 结束时清零，这一暂停也随之解除（窗口已恢复，此前排在其后的项照常开始）。
- 用户持有的暂停：队列已因 `turn_cancelled` / `start_failed` / `daemon_restarted` / `user` / `scheduled` 暂停时（例如暂停期间手动发送的 prompt 遇到额度用尽），续写项照常插到队首，但 `pauseReason` 不变、不设定时器，`resumeAt` 照常下发仅供显示；用户 `resume` 后从续写项开始。

### 定时会话（`conversation.schedule`）

能力 `scheduledConversations: true`（与 `backendQueueControl` 一起恒为 true）表示 daemon 可把会话的追加队列扣留到指定时刻再发送。客户端通常新建会话后调用 `conversation.schedule`，定时期间所有消息都经 `conversation.queue.add` 入队（被扣留）；到点后与普通队列一样派发：开始队首，每个 turn 以 `turn.completed` 结束后开始下一项。扣留的消息与普通队列一样只在 daemon 内存中（见 [history-encryption.md](history-encryption.md) §1），重启即丢失。

- 命令：payload 为 `{ "conversationId", "at": <RFC 3339 时刻> | null }`，结果为 `{ "conversationId", "queue": <快照>, "schedule": { "at", "interrupted"? } | null }`。
- 设定（`at` 非空）：会话须可写（旧版明文会话返回 `HISTORY_READ_ONLY`）且没有运行中的 turn（否则 `CONFLICT`）；`at` 须晚于现在且不超过 366 天后（否则 `INVALID_REQUEST`）。manifest 记 `schedule: { "at" }`，队列以 `scheduled` 暂停、`resumeAt` 为 `at`（队列为空时也保持暂停）；原有的 `user` 等暂停被取代。对已定时的会话再次调用只改时间。到点（按墙钟时间每 30 秒复核，机器休眠后也会补上）后清除 manifest 的 `schedule`、解除暂停并开始队首；队列为空时只是回到空闲。
- 取消（`at` 为 `null`）：清除 `schedule`（包括已中断的）。队列为 `scheduled` 暂停时：有消息则改为 `user` 暂停，不发送任何内容；没有消息则解除暂停。归档会话同样取消其定时。删除会话一并丢弃队列，计时器到点后不做任何事。
- 定时期间：`conversation.queue.add` 即使会话空闲、队列为空也只入队不开始；`conversation.queue.resume` 即「立即发送」，清除 `schedule` 并开始队首；`conversation.queue.clear`（以及 `remove` / `take` 移空队列）只移除消息，保留 `scheduled` 暂停与 `schedule`；`conversation.queue.pause` 不改原因；额度续写也不会解除该暂停。直接的 `conversation.prompt`（以及 `conversation.retry`）返回 `INVALID_REQUEST`：`conversation is scheduled; queue the prompt instead`，旧客户端因此无法绕过扣留。
- manifest 字段 `schedule: { "at": <RFC 3339>, "interrupted"?: true }`（未定时时省略，`interrupted` 为 `false` 时省略）出现在会话列表、详情、创建/更新结果中，客户端据此在侧栏显示倒计时；它与标题、归档一样随会话列表刷新获得，不产生单独的事件。队列的变化（含到点发送）照常以 `followups.updated` 推送给订阅者。
- 重启：daemon 启动时把所有仍生效的 `schedule` 标记为 `interrupted: true` 并写回（队列已随重启丢失）。中断的定时不扣留任何消息、没有计时器、不拒绝直接 prompt；客户端显示后由用户重新设定（`conversation.schedule` 给出新的 `at`，同时清除 `interrupted`）或取消。

### 历史加密密钥（`history.*`）

规格见 [history-encryption.md](history-encryption.md) §3、§7。所有命令经 `/v2/ws` 发送，任一已注册设备都可调用；调用方身份取自连接的设备签名（`deviceId`），不读 payload。命令执行时设备已被吊销则返回 `UNAUTHENTICATED`。payload 拒绝未知字段，缺省 payload 视为 `{}`。

| 命令 | 请求 | 结果 |
| --- | --- | --- |
| `history.encryption.get` | `{}` | `{ "mode": "e2e", "epoch", "recipients": [...], "myRid"?, "myAccess": "active"\|"unregistered"\|"revoked", "grants": [...], "revokedDevices": [{ "deviceId", "revokedAt" }] }`；`mode` 恒为 `"e2e"`；被封禁设备也可调用 |
| `history.recipient.register` | `{ "publicKey" }`（X-Wing 公钥 1216 字节，base64url） | `{ "rid" }`；同一公钥幂等；开启设备认证时换钥吊销旧 `rid`，未开启时（所有连接都是 `local`）每个客户端的公钥并存、各自按 `rid` 吊销；已吊销或属于其他接收方的公钥返回 `CONFLICT` |
| `history.recipient.revoke` | `{ "rid" }` | 同 get；未知 `rid` 为 `NOT_FOUND`，已吊销幂等；吊销设备当前的接收方同时封禁该设备 |
| `history.device.restore` | `{ "deviceId" }` | 同 get；解除封禁（不恢复旧公钥，设备须登记新公钥并重新申请授权）；设备未被封禁为 `NOT_FOUND` |
| `history.recovery.set` | `{ "publicKey" }` | `{ "rid" }`；替换（吊销）现有恢复接收方 |
| `history.grant.request` | `{}` | `{ "grantId" }`；同一接收方已有待办时返回原 `grantId` |
| `history.grant.list` | `{}` | `{ "grants": [{ "grantId", "rid", "deviceId", "requestedAt", "status", "updatedAt"?, "publicKey" }] }` |
| `history.grant.dismiss` | `{ "grantId" }` | `{}` |
| `history.keys.list` | `{ "conversationId"?, "cursor"?, "limit"? }`（1–500，默认 500） | `{ "items": [{ "conversationId", "kid" }], "nextCursor"? }` |
| `history.keys.wraps` | `{ "conversationId", "kids": [≤500], "rid"? }`（默认调用方 `rid`） | `{ "wraps": { "<kid>": { "rid", "kemCt", "wrapped" } } }`，无封装的 `kid` 省略 |
| `history.grant.fulfill` | `{ "grantId"?, "rid", "wraps": [{ "conversationId", "kid", "wrapped" }]（≤500）, "complete"? }` | `{ "added" }` |

`recipients[]` 项为 `{ "rid", "kind": "device"\|"recovery", "deviceId", "publicKey", "addedAt", "revokedAt" }`（含已吊销项）。授权状态 `status` 为 `pending`、`fulfilled`、`dismissed`、`revoked`（请求方接收方被吊销或换钥）。接收方集合变化（登记、换钥、吊销、恢复密钥替换、设备被吊销）使 `epoch` 加一。

历史始终端到端加密：`history.encryption.enable` / `disable` 已删除，旧客户端调用时按未知命令处理；配置 `history_encryption` 与环境变量 `TODEX_AGENTD_HISTORY_ENCRYPTION` 已作废，读到任何值只记一条警告。没有未吊销的接收方时，新建会话、prompt、追加、重试等写入返回 `HISTORY_KEY_REQUIRED`（HTTP 409），客户端应提示用户在客户端登记设备密钥（`history.recipient.register`）。未开启设备认证时 `myRid` 与 `history.keys.wraps` 的缺省 `rid` 取 `local` 最新登记的公钥，多个客户端并存时应显式传自己的 `rid`。

`history.grant.fulfill`：带 `grantId` 时目标 `rid` 必须是该待办授权的接收方（否则 `UNAUTHORIZED`），授权已结束为 `CONFLICT`；不带 `grantId`（恢复密钥导入）时目标只能是调用方自己的 `rid`。每个 `wrapped.rid` 必须等于 `rid`（`INVALID_REQUEST`），每个 `kid` 必须已存在（`NOT_FOUND`），会话须属于调用方；全部校验通过后才写入。目标已有封装的 `kid` 跳过，因此分批重试安全。最后一批带 `complete: true` 把授权标为 `fulfilled`。

设备封禁（规格见 [history-encryption.md](history-encryption.md) §3.4）：吊销某设备当前的接收方（`history.recipient.revoke`）或在 TUI 吊销设备时，该设备进入 `revokedDevices`，除 `history.encryption.get` 外的所有 `history.*` 命令返回 `HISTORY_ACCESS_REVOKED`（HTTP 403），重新配对同一设备身份也不解除，直到另一台未被封禁的设备调用 `history.device.restore`。被封禁设备仍可订阅、回放会话并收到密文。未开启设备认证时命令吊销不封禁。

状态推送：接收方、授权、模式或封禁列表每次持久化变化后，所有 `/v2/ws` 连接（含被封禁设备）都会收到全局事件 `{ "eventId", "type": "history.encryption.updated", "payload": { "epoch", "mode", "reason", "rid"?, "deviceId"?, "grantId"?, "conversationIds"? } }`，不含任何公钥或封装密钥；`mode` 恒为 `"e2e"`；`reason` 为 `recipient.registered`、`recipient.revoked`、`device.restored`、`device.revoked`（TUI 吊销，daemon 2 秒内推送）、`recovery.set`、`grant.requested`、`grant.dismissed`、`grant.progress`（`conversationIds` 为本批新增封装的会话）、`grant.fulfilled`，各字段含义见 history-encryption.md §7.1。客户端收到后重新读取 `history.encryption.get`。

客户端须声明能解密历史：`/v2/ws` 握手 query 与 `GET /v2/conversations/{id}/events` query 带 `historyEncryption=1`（握手 query 受设备签名覆盖）。未声明的 `conversation.subscribe` 与 HTTP 回放返回 `CLIENT_UPGRADE_REQUIRED`（HTTP 426）。`/v2/version` 的 `historyEncryption` 字段给出后端支持的版本；旧后端对 `history.*` 返回 `UNSUPPORTED`。旧版明文会话（`legacyPlaintext`）同样只发给已声明的客户端。

旧版明文历史（规格见 [history-encryption.md](history-encryption.md) §8）：强制加密之前写下、含任何明文记录、明文标题或明文请求快照的会话，manifest 带 `legacyPlaintext: true`（为 `false` 时省略），出现在会话列表、详情、创建/更新结果中，`conversation.subscribe` 的结果也带 `legacyPlaintext: true`。这类会话保持原样不迁移、不改写，并且只读：可读取、订阅、回放、导出、归档/取消归档与删除；prompt/followUp、`conversation.retry`、`conversation.queue.add`/`resume`、`conversation.compact`、`conversation.fork`、改名（`PATCH` 的 `title`）、权限应答与 `conversation.control` 均返回 `HISTORY_READ_ONLY`（HTTP 409），队列被拒绝时不会入队。daemon 启动后在后台一次性扫描所有会话写入该标记（不阻塞读取与 `/health`），尚未扫描到的会话在第一次写入尝试时当场判定。

加密历史的线上形状（细节见 [history-encryption.md](history-encryption.md) §5）：

- 事件 `payload` 只剩信封字段（`turnId`、`role`、`status` 等）加 `$enc`。活动分片与实时事件为事件级 `{ "v": 1, "kid", "c", "n", "s"?, "f"? }`：`detail=summary` 带 `s`（无单独摘要时带 `f`），`detail=full`、实时事件与缺口/滞后补放带 `f`，不会同时出现。封存分片为帧级 `{ "v": 1, "kid", "c", "n", "fr": { "s", "f", "i" } }`。
- HTTP 回放页顶层附 `frames: { "<帧 id>": { "kid", "stream", "counter", "c", "ct" } }`（只含所请求 detail 的帧，页内去重）；WebSocket 每条引用帧的 `conversation.event` 消息顶层各自附带它需要的 `frames`；一次订阅补放累计携带超过 16 MiB 帧数据时提前结束并返回 `hasMore: true`，其余走 HTTP 分页。
- 会话 manifest（列表、详情、创建与更新的结果）在加密时不含 `title`，改为 `titleEnc: { "kid", "ct" }`；fork 不继承加密标题（可在 `conversation.fork` 中另给标题）。确认完全加密的会话（新建、导入或 fork 的会话从一开始即是）带 `historyEncryptedAt`；旧版明文会话带 `legacyPlaintext: true`。
- `conversation.retry` payload 为 `{ "conversationId", "text"?, "content"?, "prompt"? }`：加密时客户端发回从用户 `message.created` 解密得到的 `retryRequest`（`text` 与 `content`），两者均按原请求的 MAC 校验后重放；旧式只带 `prompt` 仅适用于没有内联文本或图片的请求。缺失为 `INVALID_REQUEST`，与原请求不符为 `CONFLICT`；未加密的快照（仅旧版只读会话才有）忽略这些字段。
- 加密时 `conversation.control` 以同一 `requestId` 重试：同一 control 返回原结果（daemon 重启后为 `null`），被拒绝的返回 `Control was rejected (<code>).`。
- 没有未吊销接收方时，新 prompt（含队列投递与重试）返回 `CONFLICT`，进行中的 turn 不受影响。

### Pi 扩展与常驻 runtime

Pi 在回合之间持续读取 RPC stdout，支持后台通知、消息、工具进度、压缩与扩展表单。`provider.runtime` 事件包含 `provider`、`runtimeId`、`scope: "session"`、`status: "ready" | "stopped"`，停止时附带 `reason`。回合成功、纯扩展命令，以及经 `abort` ACK、清队列 ACK 和 idle 状态确认的取消均保留进程；协议失步会关闭进程，不自动重放输入。每个 daemon 最多保留 32 个 Pi runtime，达到上限后拒绝新建，已有会话不会被静默淘汰。runtime 在 `[agent].provider_keepalive_seconds`（默认 300 秒，最小 30 秒）内没有回合、会话查询、后台事件且没有未决扩展对话框时自动停止（`reason: "idle_timeout"`），下一回合以 `--session` 重新打开同一原生会话。

`pi-agenticoding` 的 `spawn` 工具在前台运行一个进程内子会话，同样映射为 `subagent.*`（`subagentId` 为该工具调用 id，`agentKind: "spawn"`，`title` 取 `prompt` 第一行非空文本的前 60 个字符，`task` 为完整 `prompt`）：`details.outcome` 为 `success` 记 `subagent.completed`（`result` 为子代理回复），`aborted` 记 `subagent.cancelled`，`error` 或工具以错误返回记 `subagent.failed`（`error` 为返回文本）；`details.stats` 的 `inputTokens` / `outputTokens` / `totalTokens` 记为 `usage.input_tokens` / `output_tokens` / `total_tokens`，模型、思考等级、模型组、`cost` 与 `turns` 记在 `metadata`。子会话运行期间不写 stdout，因此没有中间进度。没有 `prompt` 参数或 `details` 不含 `outcome` 与 `model` 的 `spawn` 调用仍是普通工具卡片。前台运行的子代理（`spawn` 与非后台的 `Agent`）在回合结束时若还没有结束结果（用户停止、进程退出），记 `subagent.cancelled`；已转入后台的运行不受影响。

Pi 本身没有后台工作的概念；`@tintinweb/pi-subagents` 的后台子代理（默认后台运行）与 `SubagentWorkflow` 运行在 Pi 进程内，运行期间几乎不写 stdout，进程停止时随之中止。daemon 依据该插件的 `Agent`（`details.status: "background"`）/ `SubagentWorkflow`（`details.taskId`）工具结果、`subagents:record` 会话记录、`subagent-notification` 消息及其状态栏 `setStatus`（`statusKey: "subagents"`）跟踪这些运行：仍有未结束的后台运行，或 Pi 正在自行运行时，runtime 不按 keepalive 回收，最长保留到上一个回合结束后 6 小时；运行结束后再保留 60 秒（插件的分组通知最多延迟 30 秒），等待 Pi 为汇报自行开始的运行。回合结束时若仍有后台运行，结束前先记一条 `provider.event` `{ "provider": "pi", "providerMethod": "background_tasks_pending", "metadata": { "taskIds": [...], "tasks": [{ "taskId", "description"?, "taskType": "subagent", "toolUseId", "startedAt" }] }, "turnId" }`（`taskId` 为插件的 agent id 或工作流运行 id，`toolUseId` 为发起它的工具调用）；回合之间 Pi 的自行运行结束后若仍有，同样补记一条（不带 `turnId`）。Pi 在回合之间自行开始运行（`agent_start`，通常由子代理完成通知触发）时，daemon 与 Claude Code 一样为它开 **provider 发起的 turn**（`trigger: "provider"`，`triggerDetail` 为 `{ kind: "background_task", taskIds, taskType: "subagent", summary? }` 或 `{ kind: "unknown" }`），不发送 prompt，读取该运行直到 `agent_settled`，可被取消；10 秒内未能开出该 turn 时，运行照旧作为不属于任何回合的后台输出记录，进程保留。Pi 自行运行期间到达的用户 prompt 以 `streamingBehavior: "followUp"` 排在该运行之后，记在该用户回合中，不再以“extension is still working”拒绝（压缩进行中仍拒绝）。进程带着未结束或未汇报的后台运行停止时，daemon 记 `background_tasks_ended`（`reason`：`expired` 超过 6 小时保留、`settings_changed`、`stopped` 用户停止/会话删除/工作区权限撤销/daemon 关闭、`process_ended` 进程退出或协议错误），daemon 未能正常关闭时由重启恢复补记 `daemon_restarted`，规则同 Claude Code。仍有后台运行时，原生压缩与会话复制以 `CONFLICT` 拒绝（它们需要重启进程）；注入的 TodeX MCP 服务器集合变化不会重启进程，新集合在后台运行结束后的下一次重启生效。

`conversation.runtime.stop` 的 payload 为 `{ "conversationId": "..." }`，与 HTTP `/runtime/stop` 等价，关闭 Pi 进程及待答表单，保留会话日志与原生 session 信息。`conversation.cancel` / `conversation.stop` 仅取消当前回合（可带 `turnId`，见上文「取消」）。删除工作区或会话、会话过期与 daemon 关闭也会停止对应 runtime。daemon 重启时补记旧 runtime 的停止事件，并将未答表单标记为取消；session 表单不会把空闲会话改成运行中。

### 预热（conversation.warm）

客户端可在用户提交 prompt 之前请求 daemon 预先启动当前会话的 Agent，省去首个 turn 的进程启动、`initialize` 与会话建立/恢复耗时。WebSocket 命令 `conversation.warm` 的 payload 为 `{ "conversationId": "..." }`，与 HTTP `POST /v2/conversations/{conversationId}/warm`（无请求体）等价；它在读循环之外执行，不阻塞同一连接上的其他命令。结果为 `{ "conversationId", "warmed": true }`，或跳过时 `{ "conversationId", "warmed": false, "reason" }`。预热是尽力而为：只有会话不存在或不属于该设备所有者（`NOT_FOUND`；预热期间会话被删除或过期时同样如此，预热启动的进程随即关闭）以及请求无效（缺少 `conversationId`）才返回错误，其余情况都以 `warmed: false` 应答，`reason` 取值：

| reason | 含义 |
| --- | --- |
| `disabled` | `[agent].provider_prewarm = false` |
| `running` | 会话有正在运行的 turn（或 fork、压缩） |
| `scheduled` | 会话处于定时发送状态 |
| `read_only` | 旧版明文历史（只读） |
| `unavailable` | Provider CLI 不可用 |
| `workspace_unavailable` | 工作区路径无效或不在允许的根目录下 |
| `busy` | 已有 4 个预热在进行、该会话的 CLI 正在安装或升级、过期清理占用执行闸门，或 daemon 正在关闭 |
| `unsupported_provider` | Provider 每个 turn 启动一个进程（Antigravity、`resident = false` 的 ACP profile） |
| `already_warm` | 会话已有常驻进程 |
| `limit` | Provider 的常驻进程已达上限；预热不会淘汰其他会话的进程 |
| `auth_required` | 登录可能需要用户操作（如未配置 API key 时 Devin 的浏览器登录、ACP profile 只配置了 `auth_method`）；预热从不发起交互式登录，也不启动进程，由首个 turn 完成登录 |
| `failed` | 启动或握手失败（详情只写入 daemon 日志） |
| `timeout` | 30 秒内未完成；进程继续在后台准备，下一 turn 照常复用或重启 |

预热按会话最近一次请求快照中的 `model`、`reasoningEffort`、`permissionMode`、`workMode`、`permissionProfile`、`sandboxMode`、`approvalPolicy`（无快照或已无法校验时用默认值）启动进程，并完成首个 turn 在发送 prompt 之前的全部步骤：常驻 ACP 进程（ACP profile、Grok Build、Devin、OpenCode、DeepSeek Harness、omp）完成 `initialize`（及 `authenticate`）与 `session/new` / `session/resume` / `session/load`；Codex 完成 `initialize` 与 `thread/start` / `thread/resume`；Claude Code 启动 stream-json 进程并完成 `initialize`；Pi 启动 RPC runtime。随后的 turn 若设置不同，按各 Provider 的既有规则处理（ACP 通过 `session/set_config_option`；只能经命令行参数应用某项设置的 Agent（如 Grok Build 未暴露的选项）在预热后的首个 turn 按该 turn 的设置重启进程，与冷启动一致；Claude Code 以 `--resume` 重启进程）。Codex 的 `turn.configuration` 由首个 turn 记录：设置与预热相同时为 `thread/start` / `thread/resume` 响应确认的值（`effective.source: "provider-confirmed"`），不同时为该 turn 的 `turn/start` 所覆盖的值（`effective.source: "requested"`）。预热不写入会话日志、不改变 manifest 状态，也不保存 Provider 状态：新建的原生会话由首个 turn 记录。预热期间 Provider 发出的事件（如 `provider.commands.updated`、`session/recreated`）先由 daemon 暂存，在下一个 turn 的 `turn.started` 之后写入日志；此后（包括预热仍在进行或已超时时提交的 turn）预热进程再发出的事件直接记入该 turn，与常驻进程在 turn 之间的通知相同，不会留给之后的 turn。Pi runtime 的 `provider.runtime` `ready` 事件推迟到首个 turn 才记录；未被使用的预热进程在 `provider_keepalive_seconds` 后静默停止，不记录 `stopped`。与预热同时到达的 turn 在同一进程上排队，等预热完成后执行。

Pi capability 额外声明 `runtimeStop`、`sessionCommands`、`extensionMessages` 与 `extensionUi` 方法列表；其他常驻 Provider（Devin、OpenCode、DeepSeek Harness、omp）同样声明 `runtimeStop` 与 `sessionCommands`，但没有扩展协议，报告 `extensionMessages: false` 与空的 `extensionUi`。标准 RPC 扩展输入 `select`、`confirm`、`input`、`editor` 继续经 permission broker 回答；`permission.requested` / `permission.resolved` 带 `runtimeId` 与 `scope: "session" | "turn"`，请求 details 中也保留上下文。session 表单可跨越回合，回合取消只关闭 turn 表单。

`extension.ui` 保留 Pi 标准字段，并附带 `provider`、`runtimeId` 和生命周期 `scope`：`notify` 使用 `message` / `notifyType`；`setStatus` 使用 `statusKey` / `statusText`；`setWidget` 使用 `widgetKey` / `widgetLines` / `widgetPlacement`；`setTitle` 使用 `title`；编辑器填充方法名为 `set_editor_text`，文本字段为 `text`。省略 `statusText` 或 `widgetLines` 表示清空。状态、widget、标题按键去重，并以 100 ms 合并高频更新，最终值与清空操作会保留；通知与编辑器填充直接交付。

原生 custom message 以 `extension.message` 交付，包含独立 `messageId` 与完整 `message`（`customType`、`content`、`display`、可选 `details`、`timestamp`）。客户端尊重 `display: false`，并以普通文本安全显示可见内容。原生后台消息与工具不携带旧 `turnId`；usage 保留既有 `scope: "message"` 用于计费去重。`ui.custom` 等依赖终端渲染的自定义界面没有通用 RPC 表示，客户端不能宣称支持或自动回退执行。

### Skill/MCP Catalog

```http
GET /v2/catalog/skills?provider=claude-code&workspace=/home/user/projects/demo
POST /v2/catalog/skills?provider=claude-code&workspace=/home/user/projects/demo
GET /v2/catalog/skills/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
PUT /v2/catalog/skills/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
DELETE /v2/catalog/skills/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
GET /v2/catalog/skills/{resourceId}/bundle?provider=claude-code&workspace=/home/user/projects/demo
PUT /v2/catalog/skills/{resourceId}/bundle?provider=claude-code&workspace=/home/user/projects/demo
POST /v2/catalog/skills/bundle?provider=claude-code&workspace=/home/user/projects/demo
GET /v2/catalog/mcp?provider=claude-code&workspace=/home/user/projects/demo
POST /v2/catalog/mcp?provider=claude-code&workspace=/home/user/projects/demo
GET /v2/catalog/mcp/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
PUT /v2/catalog/mcp/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
DELETE /v2/catalog/mcp/{resourceId}?provider=claude-code&workspace=/home/user/projects/demo
```

Skill 与 MCP 列表另带 `editable`（列出的条目能否在此编辑、删除）以及 `creatableSources`（Skill，可新建到的来源，取值同条目的 `source`：`shared-user`、`provider-user`、`shared-project`、`provider-project`，与 Provider 目录重合的来源只保留优先级高的一个）或 `creatableScopes`（MCP，`user` / `project`）。Grok Build 由 Provider 自行管理，`editable: false` 且不可新建；没有 MCP 配置文件的 Provider（OpenCode、omp、ACP）MCP 列表 `editable: false`、`creatableScopes: []`；旧 daemon 不返回这些字段，客户端应视为只读。

Skill 条目位于 Skill 根目录下的隐藏目录中（如 Codex 自带的 `skills/.system/*`）时带 `builtIn: true`，表示该 Skill 由 CLI 自行安装和管理；其他条目不返回该字段。配置同步等批量复制会跳过这类 Skill。

- `POST /v2/catalog/skills` 请求体 `{ source, name, content }`：在该来源的 Skill 根目录下新建 `<name>/SKILL.md`。`name` 为 1–100 个字母、数字、`.`、`_`、`-`，不能以 `.` 开头；目录已存在返回 `409`。`content` 必须带可解析的 YAML front matter，且不超过 256 KiB。返回新 Skill 的 descriptor。
- `PUT /v2/catalog/skills/{resourceId}` 请求体 `{ content }`：校验同上后原子覆盖该 `SKILL.md`（沿用原权限），`resourceId` 不变；格式无效的 Skill 也可借此修复。返回更新后的 descriptor。
- `DELETE /v2/catalog/skills/{resourceId}`：删除该 Skill 所在目录（含其中脚本等附属文件）；`SKILL.md` 直接位于 Skill 根目录时只删除该文件。返回 `{ resourceId, name, path }`，`path` 为被删除的目录或文件。
- `GET /v2/catalog/skills/{resourceId}/bundle`：读取该 Skill 所在目录的全部文件（Skill bundle，用于在两个 TodeX 后端之间复制 Skill 及其脚本、资源），返回 `{ resourceId, name, source, files: [{ path, contentBase64, executable }], skipped: [path] }`。`path` 为相对 Skill 目录、以 `/` 分隔的路径，`files` 按 `path` 升序；只读取普通文件，`contentBase64` 为标准 base64（带 padding），`executable` 表示文件有任一执行位（仅 Unix，其他平台恒为 `false`）。符号链接（指向文件或目录）、特殊文件与非 UTF-8 文件名不跟随也不读取，其相对路径按升序列在 `skipped` 中（被跳过的目录不再列出其内容）。`SKILL.md` 直接位于 Skill 根目录时 bundle 只含该文件。上限：最多 200 个文件、解码后合计 4 MiB、每个文件路径最多 8 段，超出返回 `413 PAYLOAD_TOO_LARGE`；未知 `resourceId` 返回 `404`，格式无效的 Skill 返回 `400`（与 `GET /v2/catalog/skills/{resourceId}` 相同）。
- `PUT /v2/catalog/skills/{resourceId}/bundle` 请求体 `{ files: [{ path, contentBase64, executable? }] }`：用且仅用 `files` 替换该 Skill 的目录：先写入同级的临时目录，再把旧目录改名移开、新目录改名就位、删除旧目录（就位失败时恢复旧目录），旧目录中的任何内容（含符号链接与权限）都不保留，`resourceId` 不变。`files` 必须在根部包含 `SKILL.md`，按与 `PUT /v2/catalog/skills/{resourceId}` 相同的规则校验（UTF-8、可解析的 YAML front matter、不超过 256 KiB）。`SKILL.md` 直接位于 Skill 根目录的 Skill 没有可替换的目录，返回 `400`（改用 `PUT /v2/catalog/skills/{resourceId}`）。返回更新后的 descriptor（与 `PUT /v2/catalog/skills/{resourceId}` 相同）。
- `POST /v2/catalog/skills/bundle` 请求体 `{ source, name, files }`：在该来源的 Skill 根目录下新建 `<name>/`，内容为 `files`（同样经临时目录改名就位）。`source`、`name` 的校验与 `POST /v2/catalog/skills` 相同，目录已存在返回 `409`。返回新 Skill 的 descriptor（HTTP 200，与 `POST /v2/catalog/skills` 相同）。
- 两个 bundle 写操作对 `files` 的校验：`path` 非空、相对路径、以 `/` 分隔，不含 `\` 或 NUL，没有空段、`.` 或 `..` 段，不以 `/` 或盘符（如 `C:`）开头，最多 8 段、1024 字节，且互不重复（区分大小写），同一路径不能既是文件又是另一文件的目录；`contentBase64` 必须是标准 base64（带 padding）。这些问题返回 `400 INVALID_REQUEST`；超过 200 个文件或解码后合计超过 4 MiB 返回 `413 PAYLOAD_TOO_LARGE`。在 Unix 上文件写为 `0644`，`executable: true` 时为 `0755`（不受 umask 影响）。这两个路由的请求体上限放宽到约 7.3 MiB（4 MiB 的 base64 加路径与 JSON 开销），其他路由不变。Grok Build 这类由 Provider 自行管理 Skill 的写入返回 `UNSUPPORTED`，与其他 Skill 写操作相同。
- 以上 Skill 写操作（含 bundle 写入）都会先解析符号链接：项目级来源的目标必须仍在工作区内，用户级来源的目标必须仍在该 Skill 根目录内（根目录本身可以是符号链接），否则返回 `400 INVALID_REQUEST`，不写入也不删除。例如仓库自带指向工作区外的 `.claude` 符号链接时，`provider-project` 来源的新建、编辑与删除都会被拒绝。项目级 MCP 配置文件同样如此：解析符号链接后不在工作区内的项目 MCP 文件（例如仓库自带 `.codex -> ~/.codex`）不出现在列表中，对它的新建、编辑与删除返回 `400 INVALID_REQUEST`（`… resolves outside …`），不读取也不改写该文件。
- `POST /v2/catalog/mcp` 请求体 `{ scope, name, config }`：把条目写入该 scope 的第一个 MCP 配置文件（如 Claude Code 的 `~/.claude.json` 顶层 `mcpServers` 或项目 `.mcp.json`，Codex 的 `config.toml` `[mcp_servers]`），文件不存在时创建（权限 0600）。`name` 去除首尾空白后为 1–200 个可打印字符，`config` 必须是 JSON 对象；同一文件中已有同名条目返回 `409`。返回与 `GET /v2/catalog/mcp/{resourceId}` 相同的结构。
- `PUT /v2/catalog/mcp/{resourceId}` 请求体 `{ name, config }`：在声明它的文件中原位替换该条目，`name` 与原名不同则重命名（新名已存在返回 `409`）。`resourceId` 由名称与文件计算，重命名后会改变，以响应中的 descriptor 为准。

DeepSeek Harness 的 MCP 服务器是 `acp` profile 补丁层插入的 `@deepseek-ai/dsh-mcp-client` 行（`format: "dsh_patch"`，只有 `user` scope，dsh 没有项目级 MCP 文件）：`config` 使用与其他 Agent 相同的条目写法——stdio 为 `{ "type": "stdio", "command", "args"?, "env"?, "cwd"? }`，Streamable HTTP 为 `{ "type": "http", "url", "headers"? }`，`"enabled": false` 对应行的 `disabled: true`，另可带 dsh 的 `toolCallTimeoutMs`、`maxInstructionBytes`、`failOnStartupError`、`reconnect`；其他键与 `type: "sse"`（dsh 不支持）返回 `400`。名称为 1–32 个字母、数字、`_`、`-`，以 `todex_` 开头的名称保留给 TodeX 经 ACP 注入的工具（dsh 拒绝重名的会话），同名（不区分大小写）返回 `409`。新行的 id 为 `todex-mcp-<name>`；只改写对应的那一条补丁条目，文件其余条目（含注释与 `!!js` 表达式）按原文保留，含表达式的行只读（编辑返回 `409`）。

写入规则与删除相同：JSON 按 2 空格缩进重写全文；TOML 只替换 `[mcp_servers.<name>]`，嵌套对象写为内联表，保留其余内容与注释，且 TOML 不接受 `null`；写入权限 0600 的新建临时文件后原子替换（目标已存在时沿用其权限）。

Catalog 只读取 Provider 的用户级和项目级原生配置，项目级同名资源优先。空文件或只含空白的 MCP 配置文件视为没有条目；无法解析的配置文件被跳过，同一文件的同一版本（按修改时间）只记录一次警告。文件系统来源的 Skill/MCP 列表按 (provider, workspace) 缓存：每次请求只检查扫描读过的配置文件、Skill 目录与 `SKILL.md` 的修改时间、大小与类型，均未变化时直接返回上次结果，外部修改在下一次请求即可见；经本接口的新建、编辑、删除会立即清空缓存。Grok Build 的 inspect 结果沿用其自身缓存。Skill 正文只能通过后端生成的 `resourceId` 读取；MCP 列表仅返回名称、来源、scope、transport、active 状态以及可选的 tools/authStatus/error，不返回 command、args、env、URL 或凭据。`GET /v2/catalog/mcp/{resourceId}` 返回单个条目的原始配置 `{ descriptor, path, format: "json" | "toml" | "native" | "dsh_patch", config }`：`config` 是配置文件中该条目的原样内容（TOML 转为 JSON，Grok Build 为其 inspect 输出中的条目），**包含 env、headers 等凭据**，`path` 为声明该条目的文件。`DELETE /v2/catalog/mcp/{resourceId}` 从声明它的文件中删除该条目并返回 `{ resourceId, name, path }`：JSON 文件只删除对应 `mcpServers` 映射中的那一个键（例如 `~/.claude.json` 中某个项目下的条目），按 2 空格缩进重写全文，保留键顺序但不保留原排版；TOML 文件只删除 `[mcp_servers.<name>]` 及其子表，保留其余内容与注释；写入临时文件后原子替换并沿用原文件权限。条目已不存在时返回 `404`，Grok Build 这类由 Provider 自行管理的 MCP 返回 `UNSUPPORTED`。被同名高优先级条目遮蔽的条目也可单独删除或编辑。后端不提供安装或启停接口。实际 MCP 调用使用 `mcp.call`，由 daemon 按内部配置执行。`mcp.call` 的结果整体写入 `mcp.completed`/`mcp.failed` 事件，因此序列化后超过 512 KiB（单事件上限的一半）时会被截断：按顺序保留放得下的内容块，第一个放不下的文本块截短，其他放不下的块换成一条说明，顶层字段只保留 `isError` 与 `resultType`（丢弃 `structuredContent`、`_meta` 及其他未知字段），结果带 `truncated: true` 并以一条说明文字结尾。这一上限在结果完整读入后才施加：stdio 服务器的每条 JSON-RPC 消息（一行）另有 16 MiB 的读取上限，超过即断开连接、调用失败（错误说明超过上限）；HTTP 服务器的响应在读入时没有单独的大小上限。`mcp.refresh` 的工具列表每个服务器最多 512 个，描述最多 4096 字（截断处以 `…` 结尾）。

### Conversation Folder 与旧数据迁移

```text
$DATA_DIR/conversations/<uuid-v4>/
  manifest.json
  events.jsonl
  events.NNNNNN.jsonl   # 已封存段，可选、可多个
  snapshot.json
  provider-state.json
```

journal 存储格式为 history v3（规格见 `docs/history-encryption.md` §4）：按段号排列的封存分片（`events.NNNNNN.seg` + `.idx`，或短暂存在、等待转换的 `events.NNNNNN.jsonl`），随后是唯一可写的 `events.jsonl`；`events.jsonl` 超过 16 MiB 后在下一次追加前被改名为下一个段号，后台任务把它转为 raw DEFLATE 分帧的 `.seg`；达到 `history_seal_min_bytes`（默认 2 MiB）的活动分片还会在 turn 结束、会话空闲 15 分钟或 DEK 因时长轮换前提前封存（见 `docs/history-encryption.md` §4.1）。新写入的行是短键 v3 记录，旧 v2 行照常读取；WebSocket 与 HTTP 返回的事件形状与 v2 完全相同。sequence 从 1 连续递增；损坏按文件（明文）或按分片（`.seg`）处理，以 `journal.recordLost` 占位，不再坍缩成单个文件。封存时，已有同 id 终态记录的 `message.delta`、`tool.updated`、`subagent.updated` 换成 `journal.compacted` 标记（sequence、eventId、time 不变）。旧 v2 会话不再迁移，按原样读取（它们都是旧版明文历史，只读，见「历史加密密钥」）；旧版本迁移留下的 `journal-v2-backup/` 在 7 天后删除。新会话的 manifest 带 `storageVersion: 3`，旧版 daemon 无法读取。

`events.jsonl` 是规范事件日志，sequence 从 1 连续递增；每次追加以 fsync 后的 journal 行为唯一提交点。manifest 缓存在内存中：创建、状态变化、元数据更新、强制置状态，以及会改变 manifest 的恢复时立即写 `manifest.json` 与 `snapshot.json`（与 journal 一致的 manifest 在启动恢复时不重写）；仅 `lastSequence`、`updatedAt` 变化时最多延迟 2 秒写 `manifest.json`，关闭时刷盘，崩溃后从 journal 重建。journal 修复：末条记录缺少换行时恢复阶段补上；明文文件中间损坏时先把受损文件备份为 `events.corrupt.<ts>.jsonl`，再原子重写这些文件，有效记录原样保留，每个丢失的 sequence 以 `journal.recordLost` 占位（payload `{ "reason": "corrupt", "runStart", "runLength", "backup" }`，同一段丢失共享 `runStart`/`runLength`，客户端可合并显示；普通追加无法伪造该事件类型）；末尾损坏仍隔离到备份文件后截断。`journal.compacted` 标记的 payload 为 `{ "reason": "compacted", "originalType", "runStart", "runLength" }`，同一剥离段共享 `runStart`/`runLength`；旧版本压缩写入的紧凑行 `{"sequence", "compacted": {...}}` 仍按原样读取。客户端把该类型按未知事件处理、不产生时间线条目。daemon 就绪后会在后台复制迁移旧 `$DATA_DIR/codex_gateway/sessions`；旧文件不修改，迁移可重复执行，并会去除 approval response 和常见 secret 字段。映射 `$DATA_DIR/migrations/codex-gateway-v1.json` 为 `{ schemaVersion: 1, cutoverAt?, entries: { <sourceKey>: { conversationId, complete, sourceHash, deleted? } } }`。首次迁移运行结束后映射写入 `cutoverAt`（RFC 3339；全新数据目录在首次启动时、旧版本写下的映射在本版本首次读取时立即写入），此后只更新映射中已有的来源，新出现的会话目录（旧 `codex.local` 网关仍会写入）不再导入；没有任何 Codex thread/turn（`state.json` 的 `threads`、`turns` 为空且事件不带 `codex_thread_id`/`codex_turn_id`，例如只用于 Git diff 的会话）的会话从不导入；已导入后被用户删除的会话在映射中标记 `deleted: true`，不再重建。映射的新字段对旧版 daemon 无害（它们忽略未知字段），但旧版 daemon 重写映射时会丢弃 `cutoverAt`/`deleted`，降级后被删除的导入会话可能重新出现。迁移失败会记录日志并在下次启动时重试，不阻塞 API 可用性。

Codex 的原生 `thread/tokenUsage/updated` 通知会在 Provider 边界规范化为 `usage.updated`，避免原生字段名与凭证脱敏规则冲突。`payload.usage.last` 是最近一次模型调用，`payload.usage.cumulative` 是当前原生 thread 的累计值；两者都使用 `total`、`input`、`output`、`cacheRead`、`cacheWrite` 和 `reasoningOutput` 数值字段，`payload.contextWindow` 是模型上下文窗口。Pi 的逐回复统计继续位于 assistant `message.completed` 的 `payload.message.usage`。Claude Code 的逐次调用统计同样位于主线程 assistant `message.completed` 的 `payload.message.usage`（Anthropic 字段，缓存读写不含在 `input_tokens` 内），它描述当前上下文占用；每个 `result` 帧发出一条带 `aggregation: "sum"` 的 `usage.updated`，`payload.usage`（Anthropic 字段）与 `payload.costUsd` 是本 turn 截至此刻所有模型调用（含子代理和同一 turn 内的后续调用）的累计，由进程级 `modelUsage` / `total_cost_usd` 的差值得出；它替换该 turn 先前的逐次调用用量记录，只用于用量统计，不代表上下文占用，`payload.contextWindow` 取自主模型的 `modelUsage[model].contextWindow`。与同一 turn（或运行时作用域）上一条同类型事件 payload 完全相同的 `usage.updated` / `quota.updated` 不再重复写入 journal，`/v2/providers/quota` 的快照仍每次刷新。

## 外部 API（API Key）

除了设备端口 7345，daemon 还可以在另一个端口提供面向脚本、CI 和其他服务的 REST + SSE 接口。这个端口用后端签发的 API key 鉴权，不需要设备配对，也不使用 transport v2，可以调用全局范围内已接入的 Agent。它与设备端口共用同一个会话管理器，但用 key 创建的会话与设备会话互相隔离。

外部 API 是“Agent 调用”功能的 CLI 入口。它的管理入口在桌面端/Web 端的 设置 → “Agent 调用”：开关监听与端口、签发和管理 API key、设置全局允许的 Agent 与模型，分别通过 `/v2/agent-calling*` 与 `/v2/api-keys` 设备路由完成（见下文）。后端 TUI 只显示 API 监听状态，不再提供管理功能，也没有 `api-key` 命令行子命令。

### 开启与配置

外部 API 默认关闭。开启方式有四种：在 设置 → “Agent 调用” 中打开（`PUT /v2/agent-calling/api-listener`，写入 `config.toml` 的 `[api] enabled`/`port`），在 `config.toml` 中设置 `[api] enabled = true`，启动时加 `--enable-api`，或设置环境变量 `TODEX_AGENTD_API_ENABLED=true`。监听配置在下次（重新）启动服务时生效；设置页只能修改开关和端口，`host` 与 `allow_plaintext_remote` 只能在配置文件或命令行中修改。

| 配置 | 命令行参数 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| 开启外部 API | `--enable-api` | `TODEX_AGENTD_API_ENABLED` | `false` |
| 监听主机 | `--api-host` | `TODEX_AGENTD_API_HOST` | `127.0.0.1` |
| 监听端口 | `--api-port` | `TODEX_AGENTD_API_PORT` | `7346` |
| 允许非回环明文监听 | 无 | `TODEX_AGENTD_API_ALLOW_PLAINTEXT_REMOTE` | `false` |

```toml
[api]
enabled = true
host = "127.0.0.1"
port = 7346
# allow_plaintext_remote = true   # 仅在可信 TLS 反向代理之后使用
```

启动时会检查以下几点：
- API 端口不能与主端口相同。
- API key 以 bearer 凭据的形式在明文 HTTP 中传输，因此 `api.host` 不是回环地址时必须同时设置 `allow_plaintext_remote = true`，否则 daemon 拒绝启动；设置后每次启动都会记录一条警告。远程访问应在前面放一个终止 TLS 的反向代理。
- `daemon start` 会预先检查 API 端口是否被占用。

daemon 运行时，`daemon status` 会多输出一行 `API listening: http://<host>:<port>/api/v1`。外部 API 不设 CORS，浏览器跨域调用会失败。

### API key 管理

key 的格式是 `tdx_<16 位十六进制 id>_<43 字符 base64url secret>`，只在创建时完整显示一次。`<data_dir>/api-keys.json`（0600）只保存 secret 的哈希，以及由 secret 派生的历史接收方公钥（见 [history-encryption.md §3.5](history-encryption.md)），文件本身不足以鉴权或解密。

daemon 按文件修改时间重新加载 key，修改无需重启就会生效。每把 key 的属性如下：

- `scopes.agents`：允许使用的 Agent 列表，省略表示全部。
- `scopes.workspaces`：允许使用的 workspace 绝对路径列表（包含子目录），省略表示全部 workspace 根目录。
- `approval`：审批策略，取值为 `ask`（默认）、`auto-approve` 或 `reject`。
- `expiresAt`：过期时间，Unix 毫秒，可选。

最多可以有 256 把有效 key。被吊销的 key 仍保留在列表中，状态显示为 `revoked`。吊销或过期后，这把 key 的请求立即返回 401，已打开的 SSE 流在几秒内以 `error` 事件结束，正在运行的 turn 在约 10 秒内被取消。会话历史会保留。

**管理入口**：桌面端/Web 端 设置 → “Agent 调用”，通过以下设备端 REST（端口 7345，需要设备签名）：


  - `GET /v2/api-keys`：返回 `{"keys":[…]}`。每项包含 `id`、`name`、`prefix`、`status`、`scopes`、`approval`、`createdAt`、`expiresAt`、`lastUsedAt`、`revokedAt`，不含任何哈希或密钥。
  - `POST /v2/api-keys`：请求体为 `{"name","scopes"?,"approval"?,"expiresAt"?}`。返回 201，响应中的 `key` 字段是完整 key，只返回这一次。
  - `PATCH /v2/api-keys/{id}`：请求体为 `{"name"?,"scopes"?,"approval"?,"expiresAt"?}`，其中 `expiresAt: null` 表示取消过期时间。
  - `DELETE /v2/api-keys/{id}`：吊销。返回 `{"id","revoked"}`；不存在的 id 返回 404。
  - `GET /v2/api-keys/listener`：返回 `{"enabled","host","port"}`（进程启动时的配置）。监听的完整状态见 `GET /v2/agent-calling` 的 `apiListener`。

### 全局范围与 Agent 调用设置

`$DATA_DIR/agent-calling.json`（0600，原子替换）保存“Agent 调用”的全局设置，CLI 入口（外部 API）与 MCP 入口（`todex_agents`）共用：

```json
{
  "scope": { "agents": null, "models": {} },
  "mcp": { "enabled": false, "approval": "ask" }
}
```

- `scope.agents`：`null` 表示全部 Agent；数组（Agent id，如 `"codex"`、`"claude-code"`）表示只允许这些，可以为空。
- `scope.models`：Agent id → 允许的模型 id 数组（非空）。不在表中的 Agent 可以使用它的全部模型。空数组会被拒绝（400）。
- `mcp.enabled` / `mcp.approval`：MCP 入口的开关与审批策略，`approval` 取值与 API key 相同（`ask`、`auto-approve`、`reject`），用于 Agent 调用产生的子会话。
- 文件不存在时即为上面的默认值（全部 Agent、全部模型、MCP 关闭），已有的外部 API 用法不受影响；默认值不会写入文件，直到客户端保存。
- 启动时宽松读取：忽略未知字段，丢弃未知的 Agent id（`scope.agents` 中的项与 `scope.models` 的键），未知的 `mcp.approval` 按 `ask` 处理，因此较新版本写入的文件仍可读取。文件无法使用（无法读取、超过 1 MiB、不是合法 JSON、字段类型不对或范围无效）时不会阻止后端启动：记录一条不含文件内容的警告，按上面的默认值运行（MCP 入口关闭）；内容有问题的文件改名为 `agent-calling.json.invalid-<Unix 秒>` 保留，不会被之后的保存覆盖。客户端的 `PUT` 仍严格校验（见下文）。

外部 API 的有效范围是全局范围与 key 自身 `scopes` 的交集：
- Agent 必须同时在全局 `scope.agents` 与 key 的 `scopes.agents` 内，否则 403 `UNAUTHORIZED`。`GET /api/v1/agents` 只列出两者都允许的 Agent，每项的 `models` 也只含全局允许的模型；`GET /api/v1/agents/{agent}/models` 同样按全局范围过滤。
- turn 指定的 `model` 必须在该 Agent 的允许列表内，否则 403 `UNAUTHORIZED`（`runs` 在创建会话前检查，被拒时不会留下会话）。
- turn 未指定模型时：该 Agent 不受模型限制则由 Agent 使用默认模型；受限制时使用实时模型目录中的默认模型（若被允许），否则使用允许列表中的第一个模型。受限制时解析出的模型会随该 turn 明确发出，所以在已有会话中不指定 `model` 的 turn 会切换到这个默认/第一个允许的模型，即使会话之前使用的是另一个（同样允许的）模型；要保持模型不变，请每个 turn 都指定 `model`。
- 给出 `reasoningEffort` 时，它必须在所选模型（未指定则为默认模型）的 `supportedReasoningEfforts` 内（该列表非空时），否则 403 `UNAUTHORIZED`。检查需要实时读取模型目录；读取失败时不做该检查，交给 Agent 处理。

设备路由（端口 7345）：

- `GET /v2/agent-calling`（需要设备认证）返回：

  ```json
  {
    "scope": { "agents": null, "models": {} },
    "mcp": { "enabled": false, "approval": "ask", "injected": false },
    "apiListener": {
      "enabled": true, "host": "127.0.0.1", "port": 7346,
      "listening": true, "baseUrl": "http://127.0.0.1:7346/api/v1",
      "pendingRestart": false
    },
    "limits": { "maxConcurrentCalls": 4, "maxWaitSecs": 1200, "defaultWaitSecs": 300 }
  }
  ```

  `mcp.injected` 表示按当前设置会注入 MCP 服务（`mcp.enabled` 且至少允许一个 Agent）。`apiListener.listening` 与 `baseUrl` 描述本进程实际运行的监听（未监听时 `baseUrl` 为 `null`）；`enabled`、`port` 是已保存的配置；`pendingRestart` 表示已保存的配置与正在运行的监听不同，需重启后端生效。`limits` 是 MCP 入口的限制：每个父会话最多同时运行的子调用数，以及等待时间的上限与默认值（秒）。
- `PUT /v2/agent-calling`（需要设备签名，未开启设备认证的本地部署返回 403）：请求体为 `{"scope"?: {"agents"?, "models"?}, "mcp"?: {"enabled"?, "approval"?}}`。给出 `scope` 时整体替换范围（其中省略的字段取默认值）；`mcp` 中省略的字段保持不变。未知字段、未知 Agent id、空模型数组返回 400 `INVALID_REQUEST`。返回与 GET 相同的结构。
- `PUT /v2/agent-calling/api-listener`（需要设备签名）：请求体为 `{"enabled"?: bool, "port"?: number}`，写入 `config.toml` 的 `[api]`，下次启动后端时生效。只写入请求中给出的字段：未给出的字段（其当前值可能来自 `--enable-api`、`TODEX_AGENTD_API_ENABLED`/`TODEX_AGENTD_API_PORT` 等命令行或环境变量覆盖）不会写入 `config.toml`；`pendingRestart` 仍与正在运行的监听比较。`port` 必须在 1–65535 之间且不能等于主端口；开启后的配置还须能通过启动检查（例如非回环 `host` 需要 `allow_plaintext_remote`）；否则返回 400 `INVALID_REQUEST`。返回与 GET 相同的结构。

修改设置会写一条审计事件 `agent_calling.settings.audit`。

### 鉴权、隔离与 workspace 范围

**鉴权：**
- 请求头为 `Authorization: Bearer <key>`，也可以用 `X-API-Key: <key>`。
- 缺少 key、key 错误、已吊销或已过期，一律返回 401 `UNAUTHENTICATED`，不区分具体原因。
- 同一来源地址 60 秒内鉴权失败 30 次后返回 429 `RATE_LIMITED`，响应带 `Retry-After`。

**隔离：**
- 每把 key 创建的会话属于 owner `apikey:<id>`。key 之间互相看不到对方的会话，也看不到设备会话；设备端同样看不到 API 会话。访问别人的会话返回 404。

**scope：**
- workspace 必须是位于 `workspace_roots` 内的已有目录，否则返回 403 `WORKSPACE_PATH_OUTSIDE_ROOT` 或 404 `WORKSPACE_PATH_NOT_FOUND`。
- Agent 或 workspace 超出 scope 时返回 403 `UNAUTHORIZED`；在已有会话上发起 turn 时按 key 当前的 scope 重新检查会话的 workspace，因此收窄 `scopes` 后，scope 外会话的新 turn 同样返回 403，正在运行的 scope 外 turn（agent 或 workspace 越界）在 `PATCH` 返回前、或 key 由其他进程修改后约 10 秒内被取消。`scopes.workspaces` 省略时（scope 为全部），key 可使用 `workspace_roots` 内的任意目录。
- 全局范围（见上文）与 key 的 scope 同时生效：Agent、模型或思考强度不被允许时同样返回 403 `UNAUTHORIZED`。

**能力限制：**
- API 会话不注入 TodeX 自带的 agent 工具（SSH、桌面/Computer Use、agent 浏览器），只使用 Agent 的原生能力。
- turn 请求不接受 `permissionMode`、`sandboxMode`、`approvalPolicy`、`permissionProfile`、`workMode`，统一使用 Agent 的默认值，由 key 的审批策略处理权限请求。带有这些字段的请求返回 422。
- 首版只支持文本 prompt 和 skills 引用。请求体上限 4 MiB。

### 审批策略

| `approval` | 行为 |
| --- | --- |
| `ask` | Agent 请求权限时推送 `permission.requested` 事件，由调用方 `POST …/permissions/{permissionId}` 回答；超时或取消时的处理与设备端相同 |
| `auto-approve` | 后端立即选择 `allow_once` 选项；没有该选项的请求（例如提问）仍交给调用方回答 |
| `reject` | 后端立即依次尝试 `reject_once`、`reject_always`、`abort_turn` 选项 |

带 `allowedDeviceIds` 的请求只能由指定设备回答，后端从不代 API key 批准这类请求：`auto-approve` 下按拒绝处理，`ask` 下 API 调用方也无权回答（403）。无论哪种策略，`permission.requested` 和 `permission.resolved` 都会写入历史；由策略自动作答时，记录的作答方为 `apikey-policy:<id>`。

### 接口（Base URL `http://127.0.0.1:7346`）

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/v1/health` | 无需鉴权，返回 `{"ok":true,"version"}` |
| GET | `/api/v1/me` | 当前 key 的摘要，字段与 `/v2/api-keys` 的列表项相同 |
| GET | `/api/v1/agents` | 按全局范围与 key 的 scope 过滤后的 Agent 快照，返回 `{"agents":[…]}`，每项结构同 `/v2/providers`，`models` 只含全局允许的模型 |
| GET | `/api/v1/agents/{agent}/models?workspace=` | 实时模型列表（只含全局允许的模型），返回 `{"agent","models"}` |
| GET | `/api/v1/workspaces` | 可用的 workspace，返回 `{"workspaces":[{"path","scope":"root"\|"key"}]}` |
| POST | `/api/v1/conversations` | 请求体为 `{"agent","workspace","title"?,"profile"?}`，返回 201 和会话 |
| GET | `/api/v1/conversations` | 返回 `{"conversations":[…]}` |
| GET / DELETE | `/api/v1/conversations/{id}` | 查看或删除会话 |
| POST | `/api/v1/conversations/{id}/turns` | 请求体为 `{"text","model"?,"reasoningEffort"?,"skills"?:[{"resourceId","name"?}],"clientRequestId"?}`。默认返回 202 `{"conversationId","turnId"}`；带 `Accept: text/event-stream` 时直接以 SSE 推送本 turn 的事件，终止事件后关闭，响应头带 `X-Todex-Conversation-Id` 和 `X-Todex-Turn-Id`。已有 turn 在运行时返回 409 |
| GET | `/api/v1/conversations/{id}/events?after=&limit=` | 明文事件分页。`after` 不包含在结果内，默认 0；`limit` 默认 200，最大 1000。返回 `{"conversationId","events","nextSequence","hasMore"}` |
| GET | `/api/v1/conversations/{id}/events/stream?after=` | SSE：先推送 `after` 之后的历史，再持续推送实时事件；支持 `Last-Event-ID` 续传 |
| POST | `/api/v1/conversations/{id}/cancel` | 请求体可选，为 `{"turnId"?}`。返回 `{"conversationId","cancelled","turnId","activeTurnId"?}` |
| POST | `/api/v1/conversations/{id}/permissions/{permissionId}` | 请求体为 `{"outcome","optionId"?,"data"?}`，取值同 v2；返回 `{"conversationId","permissionId","accepted":true}` |
| POST | `/api/v1/runs` | 一次性调用，见下文 |

**会话对象**与 v2 manifest 的区别：
- `title` 是解密后的标题。
- 增加 `agent` 字段（等于 `provider`）。
- 去掉 `titleEnc` 和 `ownerId`。

**事件对象**与 v2 的 `ConversationEvent` 结构相同（`sequence`、`eventId`、`type`、`payload` 等），但 `payload` 是**明文**：
- 后端用调用方提供的 key 在内存中解密，见 [history-encryption.md §3.5](history-encryption.md)。
- 少数无法用这把 key 解开的事件（例如签发 key 之前写入的内容），`payload` 为 `{"encrypted":true}` 加上信封字段。
- 流式文本片段在写入历史时会合并，所以回放得到的 `message.delta` 可能比实时推送时更长。

**SSE 格式：**
- 每条事件的格式为 `id: <sequence>`、`event: <事件 type>`、`data: <事件 JSON>`。
- 每 15 秒发送一行 `: ` 注释作为 keepalive。
- 流内出错，或 key 被吊销、过期时，会发送 `event: error`（`data` 为 `{"code","message"}`），随后关闭流。
- 断线后带上 `Last-Event-ID`（或 `?after=`）重连即可续传，不会重复或遗漏事件。

**`POST /api/v1/runs`**：在一次请求中创建会话并发送一个 turn。

请求体为 `{"agent","workspace","text","model"?,"reasoningEffort"?,"title"?,"profile"?,"stream"?:false,"timeoutSecs"?}`。
- `stream: true` 时，响应与上面带 SSE 的 turn 相同。
- 否则等待 turn 结束后返回 `{"conversationId","turnId","status","output","error"}`：
  - `status` 取值为 `completed`、`failed`、`cancelled`、`interrupted`。
  - `output` 是主 Agent 本 turn 的助手文本拼接，不含 subagent 和思考内容。
- `timeoutSecs` 默认 600，范围 1–3600。超时后后端会取消该 turn，返回 504 `{"code":"RUN_TIMEOUT","conversationId","turnId","output"}`，其中 `output` 是超时前已收到的文本。

**错误格式**与 v2 相同，为 `{"code","message"}`：

| 状态码 | 场景 |
| --- | --- |
| 400 | 参数错误 |
| 401 | 鉴权失败 |
| 403 | 超出 scope（全局范围或 key 的 scope，包括模型与思考强度），或 workspace 位于 `workspace_roots` 之外 |
| 404 | 会话不存在或不属于该 key |
| 409 | 已有 turn 在运行 |
| 422 | 请求体包含不允许的字段 |
| 429 | 鉴权失败次数过多 |
| 503 | Agent 不可用 |
| 504 | `runs` 超时 |

**示例**（key 在 设置 → “Agent 调用” 中创建，完整 key 只显示一次，保存到环境变量 `TODEX_API_KEY`）：

```bash
KEY=$TODEX_API_KEY
curl -s -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"agent":"codex","workspace":"'"$HOME"'/projects/app","text":"总结 README"}' \
  http://127.0.0.1:7346/api/v1/runs
curl -N -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"agent":"claude-code","workspace":"'"$HOME"'/projects/app","text":"列出 TODO","stream":true}' \
  http://127.0.0.1:7346/api/v1/runs
```

## HTTP 接口

### 传输策略与 REST 隧道

`GET /v2/transport-policy`（无需签名，`Cache-Control: no-store`）返回 `{"requiredProtocol": "none" | "x25519" | "ml-kem-768", "transportVersion": 2, "sealedRevision": 2}`。客户端只用它检查与已固定协议是否冲突、后端是否支持 sealed revision 2（不满足即拒绝连接，不回落），不会因此降级为明文；协议与公钥本身只来自设备配对（见下）。

`POST /v2/sealed` 是 transport v2 的 REST 隧道（无需外层签名）：外层 header 为 `Content-Type: application/vnd.todex.sealed`、`X-Todex-Transport: 2`、`X-Todex-Encryption`、`X-Todex-Client-Key`（x25519）或 `X-Todex-Kem-Ciphertext`（ml-kem-768）、`X-Todex-Request-Nonce`、`X-Todex-Sealed-Revision: 2`（缺失或不是 `2` 时，读取 body 前即返回明文 426 `PROTOCOL_UPGRADE_REQUIRED`）；外层 body 是用 `k_up` 封装的记录流，明文为一个完整的内层请求（JSON head + body）。内层请求经同一个 router 执行，照常做设备签名校验（签名覆盖内层方法、路径、query 与 body）和各路由的 body 上限；内层 head 一解出就先做与 body 无关的检查：公开路由（`/health`、`/v2/version`、`/v2/transport-policy`、`/v2/device-pairing/*`）无需凭证但内层 body 上限 64 KiB，其余路径须带已登记设备的有效签名 header（匿名部署则做回环 `Host`/`Origin` 检查），否则不再读取外层 body，直接返回加密的内层 `401`（匿名检查为 `403`）；通过检查的隧道请求最多 32 个同时缓冲与处理，公开路由另有独立的 4 个名额，其余排队，排队超过 10 秒返回加密的内层 `503 TRANSPORT_BUSY`（内层 `retry-after: 1`；保证未执行、签名 nonce 未被占用，客户端无论 method 都可用新签名重试一次）；请求开始 30 秒内必须解出内层 head，否则返回明文 `400 TRANSPORT_CRYPTO_FAILED`；外层 body 两块之间最多间隔 30 秒，内层 head 解出前超时为明文 400，解出后为加密的内层 `408 REQUEST_TIMEOUT`；内层只保留 `content-type`、`accept` 与四个设备签名 header，`Host`/`Origin` 与对端地址取自外层请求。成功时外层固定 `200`、`Content-Type: application/vnd.todex.sealed; r=2`，body 先是 32 字节原始 `response_nonce`（每个响应新生成，加密的内层错误同样如此），随后是用 `k_down = HKDF-Expand(prk, "todex.transport.v2/rest/down" || response_nonce, 32)` 流式封装的内层响应（状态码、header 与 body，下载等流式响应逐记录转发；为保证只有最后一条记录带 final 标记，封装器总是扣住最新一块，直到下一块到达或 body 结束才发出，因此流式响应在客户端会晚一块到达）。外层无法打开（header、密钥材料、认证失败、截断、超过 32 MiB 加记录开销、嵌套 `/v2/sealed`、内层 head 超时）时返回 `400 TRANSPORT_CRYPTO_FAILED`，不含细节。客户端只对 `200` 且 content type 带 `r=2` 的响应解密；不足 32 字节即结束视为截断；其他响应一律按未认证的普通 API 错误处理；不带 `r=2` 的 sealed 响应说明后端版本过旧，提示更新后端。格式与限制见 [transport-v2.md](transport-v2.md)。

### 设备配对（v3：先承诺后揭示）

配对路由无需签名，始终可直接访问（不经隧道），body 上限 2 KiB，响应 `Cache-Control: no-store`：

1. `POST /v2/device-pairing/create` `{clientCommitment, transportBinding: 1, deviceNameBinding: 1, deviceName, devicePublicKey}` → `{requestId, serverPublicKey, expiresAt, pollIntervalMs, transportProtocol, transportPublicKey}`。`clientCommitment = base64url(SHA256(LP("todex.device-pairing.v3/commit") || client_public || client_nonce))`，`client_public` 为临时 X25519 公钥、`client_nonce` 为 32 字节随机数。`transportProtocol` 是服务端配置的 `pairing_encryption`（`none` / `x25519` / `ml-kem-768`），`transportPublicKey` 是 v2 握手此刻使用的静态公钥（base64url 无填充；`none` 时为空串），二者写入 transcript、由验证码认证。仍提交 `clientPublicKey` 的配对 v2 客户端，或缺少 `transportBinding` / `deviceNameBinding`（都必须是 JSON 整数 `1`）的客户端，得到 426 `PROTOCOL_UPGRADE_REQUIRED`。`deviceName` 按 UTF-8 原样写入 transcript（`LP(utf8(device_name))` 接在传输字段之后），由验证码认证。客户端先去掉首尾空白、为空时填默认名；服务端不再 trim、不再替换为 "Unknown device"，名称须为 1–80 个 Unicode 标量、不含 Cc 控制字符与 bidi 控制字符（U+200E/F、U+202A–202E、U+2066–2069）、首尾不是空白（Unicode `White_Space`），否则返回 400 `INVALID_REQUEST`；通过校验后原样保存。小阶（weak）Ed25519 设备公钥同样返回 400。
2. `POST /v2/device-pairing/reveal` `{requestId, clientPublicKey, clientNonce}` → `{"status":"pending"}`。服务端常量时间比较承诺，派生 v3 材料后才把验证码交给 TUI；承诺不符返回 401 但保留该申请，重复 reveal 返回 409 `CONFLICT`。
3. `POST /v2/device-pairing/poll` / `cancel` `{requestId, proof}` 与之前相同，proof 改用 v3 材料；reveal 之前没有可用 proof（401）。批准后 `poll` 返回用 wrap key、以完整 v3 transcript 为 AAD 加密的 `{deviceId, transportProtocol, transportPublicKey}`；客户端确认两个传输字段与 `create` 响应完全一致后才保存公钥。若 `create` 之后加密密钥被重置，已批准的申请返回 `expired` 且不登记设备。

准入限制（均无需认证）：按来源计数，IPv4 按地址、IPv6 按 /64（IPv4-mapped 视为 IPv4）。`create` 每来源每分钟 4 次、全局每分钟 24 次；`create` 后 30 秒内必须 `reveal`，否则丢弃；每来源未完成（未 reveal 或待审批）的申请最多 2 个，超出返回 429 `PAIRING_BUSY`；全局活动申请达到 16 个时先淘汰最早一个未 reveal 的申请，全部已 reveal 才返回 429 `PAIRING_BUSY`；每个申请每分钟最多 8 次 `reveal`；`poll` / `cancel` 每个申请每秒最多 4 次，且 `reveal` / `poll` / `cancel` 全局每秒 1024 次作为最后防线；频率超限返回 429 `RATE_LIMITED` 并带 `Retry-After`。客户端在 `expiresAt` 之前 `poll` 遇到 429、5xx 或网络错误时退避（翻倍，最多 5 秒）继续，只在 404、`expired`、`rejected`、401 或用户取消时结束。

transcript、HKDF 标签与验证码格式见 [transport-v2.md](transport-v2.md#device-pairing-v3-commit-then-reveal) 与 [设备验证](device-verification.md)。

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

该端点与 `/health` 一样不需要认证，供 daemon 自检、客户端连接卡片轮询和客户端版本一致性检测（开发构建 `DEV0.0.0`/`0.0.0` 不参与比较）。匿名调用只返回 `name`、`version`、`deployment`、`historyEncryption`、`terminalReplay`、`terminalSnapshot`；`data_dir`、`workspace_root`、`workspace_roots`、`pathMappings` 仅在请求带有效设备签名（与其他接口相同的签名方式）时返回，`enable_auth = false` 时仅在请求满足上述本地 `Host`/`Origin` 条件时返回。签名无效或 nonce 重放不会报错，只按匿名处理，便于配对前探测。

响应字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | Cargo 包名 |
| `version` | string | 构建时注入的应用版本；开发构建为 `DEV0.0.0` |
| `deployment` | string | `"host"`（默认）或 `"container"`（`TODEX_AGENTD_DEPLOYMENT` / `server.deployment`）。匿名也返回，客户端在配对前据此显示批准方法（容器上用 `todex-agentd pair approve`）；只说明部署类型，不含镜像细节。旧后端没有该字段，按 `"host"` 处理 |
| `data_dir` | string | 当前数据目录（仅鉴权后） |
| `workspace_root` | string | 主 workspace 根目录（根列表第一项，仅鉴权后） |
| `workspace_roots` | string[] | 全部已配置的 workspace 根目录（仅鉴权后） |
| `scratch_workspace_root` | string | 无工作区对话目录的父目录 `$DATA_DIR/workspace`（仅鉴权后）；不在 `workspace_roots` 中，客户端可在首次创建这类对话前用它做模型/命令发现 |
| `pathMappings` | object[] | 客户端路径与后端路径的对应关系 `[{hostPath, backendPath}]`（`TODEX_AGENTD_PATH_MAPPINGS` / `[[workspace.path_mappings]]`，仅鉴权后；未配置时省略）。`backendPath` 位于某个 workspace 根目录内；客户端用它把本机选择的目录换成后端路径，把后端路径换回本机路径打开。后端自身不转换任何路径 |
| `historyEncryption` | number | 后端支持的历史加密版本（当前 `1`），见「历史加密密钥」 |
| `terminalReplay` | number | 终端输出序号与 `terminal.attach` / `terminal.clear` 的协议版本（当前 `1`），见「终端」；旧后端没有该字段 |
| `terminalSnapshot` | number | `terminal.attach` 无法衔接时发送屏幕快照（当前 `1`），见「终端」；旧后端没有该字段 |

## 本机 IPC 与本地管理 API

daemon 默认另开一个本机 IPC 监听：Unix socket（默认 `<data_dir>/agentd.sock`）或 Windows 命名管道（`\\.\pipe\todex-agentd-<hash16>`），只有 daemon 的同一用户能连接（Unix 校验对端 uid，Windows 管道 DACL 只授予当前用户与 SYSTEM 并拒绝远程客户端）。配置（`[ipc] enabled`/`path`，`TODEX_AGENTD_IPC_ENABLED`/`TODEX_AGENTD_IPC_PATH`）、默认路径规则与信任模型见 [BUILD_RUN.md](BUILD_RUN.md#本机-ipc)。实际路径记录在 `daemon.json` 的 `ipc_path` 和 `daemon status --format json` 的 `ipcPath` 中。`network_enabled = false` 时 daemon 只经 IPC 服务客户端，TCP 上只剩内部 Agent 工具端口（见 [BUILD_RUN.md](BUILD_RUN.md#本机-ipc)）。

IPC 上是普通 HTTP/1.1（含 WebSocket 升级），客户端发送 `Host: localhost`。它提供与 TCP 监听相同的全部路由，对端视为回环：不强制 transport v2，匿名部署的 `Host`/`Origin` 检查照常进行，设备签名要求不变。经 IPC 的 `POST /v2/device-pairing/create` 在待批准列表中记为 `peerAddress: "ipc"`，并使用独立的频率限制桶。

另外只在 IPC 上提供本地管理 API（TCP 上这些路径一律 404）。这些路由不需要设备签名：调用者已通过同用户检查，本来就能直接读写数据目录。响应均为 JSON；错误为 HTTP 4xx/5xx 加 `{"error":{"code":"...","message":"..."}}`，`code` 为小写（`invalid_config`、`invalid_request`、`managed_by_image`、`code_mismatch`、`not_found`、`conflict`、`internal`）。

| 方法与路径 | 说明 |
| --- | --- |
| `GET /v2/local-admin/status` | `{version, deployment: "host"\|"container", dataDir, pid, startedAt (RFC 3339), listen: {host, port}, networkEnabled, connectUrls: [{url: "ws://h:p/v2/ws", interface\|null}], advertiseUrl\|null, pairingLink（二维码 JSON 字符串）, transportFingerprint\|null（pairing_encryption = "none" 时为 null）, pairingEncryption, authEnabled, deviceCount, pendingPairingCount, api: {enabled, host, port, listening: "h:p"\|null}, workspaceRoots, ipcPath, autoUpdate（实际生效值）, configEditable}`。`listen.port` 为实际绑定端口；`networkEnabled: false` 时 `listen` 为已配置但未绑定的地址，`connectUrls` 为 `[]`，`pairingLink` 为 `""` |
| `GET /v2/local-admin/config` | `{host, port, networkEnabled, pairingEncryption, workspaceRoots, autoUpdate, editable, restartRequired}`：已保存（`config.toml` + 环境变量）的设置；`restartRequired` 表示保存的网络开关/host/port/加密方式/根目录与运行中服务不同 |
| `PUT /v2/local-admin/config` | body 为 `{host, port, networkEnabled, pairingEncryption, workspaceRoots, autoUpdate}` 的任意子集（未知字段报错）。`networkEnabled: false` 要求 `[ipc]` 开启，写入顶层 `network_enabled`。校验与 TUI 相同：host 为 IP 字面量；port 为 1–65535，API 开启时不得等于 `api.port`；host 与加密方式须满足 `enable_auth`/回环规则；根目录须为已存在目录，规范化、去重，至少一个，第一个为主根。校验失败 400 `invalid_config`；容器部署 409 `managed_by_image`。通过后写入 `config.toml`（与 TUI 相同的键，`autoUpdate` 写 `[update] auto_update` 并立即对运行中的 daemon 生效）；其余设置在重启后生效。响应同 GET |
| `GET /v2/local-admin/pairing-requests` | 等待决定的配对申请 `[{requestId, verificationCode, deviceName, deviceId\|null, expiresAt（毫秒）, peerAddress, transportFingerprint\|null}]`，与 `pair list --format json` 相同 |
| `POST /v2/local-admin/pairing-requests/{requestId}/decision` | body `{approved, verificationCode}`。验证码须与该申请一致（不区分大小写，连字符可省），否则 409 `code_mismatch`；申请不存在、已过期或已决定 404 `not_found`。成功 `{ok: true}` |
| `GET /v2/local-admin/devices` | `[{deviceId, name, publicKey, pairedAt, lastSeenAt\|null}]` |
| `DELETE /v2/local-admin/devices/{deviceId}` | 吊销设备（同 `devices revoke`），`{ok: true}`；不存在 404 `not_found` |
| `POST /v2/local-admin/reset` | body `{target: "devices"\|"encryption"}`：吊销全部设备，或重新生成传输密钥（同 TUI 重置）。`{ok: true, restartRequired: true}` |

## Workspace 缓存同步

工作区清单由后端持久化到 `$TODEX_AGENTD_DATA_DIR/workspaces.json`。手机和桌面端的本地存储只作为离线缓存；连接成功后会拉取当前身份的后端快照。工作区 ID 由后端根据规范化路径稳定生成，并与对话 manifest 的 `workspaceId` 共用，从而让不同设备恢复同一工作区内的对话。

后端会把 `workspace_roots` 作为移动端可用工作区的权限边界。`PUT /v2/workspaces`、`/v2/workspace/entries`、本地 Codex 启动和本地终端启动都会拒绝所有根目录之外的目录；目录必须存在且是目录。

`$DATA_DIR/workspace`（`scratch_workspace_root`）是额外的访问根：创建对话、Provider 发现、文件、Git 和终端等按路径访问的接口都接受它及其子目录。它不出现在 `workspace_roots`、`/v2/workspace/directories` 的浏览根列表中，也不能注册为 `/v2/workspaces` 记录。

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

`PUT` 请求体使用同样的 `workspaces` 数组。后端会校验 `name`、`path`、路径存在性和根目录边界，按当前认证身份和规范化路径合并记录，并返回后端生成的稳定 ID。它不会接受客户端伪造的租户，也不会持久化设备本地的 `threadId` 和 `localAdapterState`。可选的 `icon`、`iconColor`、`ringStyle` 是客户端的侧栏展示偏好（图标键名、`#rrggbb` 颜色与状态环样式键名），后端只做透传持久化，不校验取值。可选的 `groupId`、`groupName` 表示侧栏分组：共享同一 `groupId` 的工作区组成一组，`groupName` 是复制到每个成员上的组名；后端只做去首尾空白并截断到 64 个字符，空值视为不在分组中。分组没有独立存储，跟随工作区记录整体合并，因此成员离组时客户端直接省略这两个字段即可。可选的 `tags`（工作区自身标签）与 `groupTags`（分组标签，像 `groupName` 一样复制到每个成员上）是字符串数组：后端去掉控制字符和首尾空白、每个标签截断到 32 个字符、丢弃空值与不区分大小写的重复项（保留首次出现的写法），每个列表最多保留 16 个；空列表在响应中省略。它们同样随记录整体合并，省略即表示没有标签，因此不认识这两个字段的旧客户端保存工作区时会清空标签。

校验失败的记录（例如目录已被删除或位于 `workspace_roots` 之外）不再让整个请求报错：它们会被跳过并在响应的 `rejected` 数组中回报，每项包含客户端提交的 `id`、`name`、`path`、错误 `code`（如 `WORKSPACE_PATH_NOT_FOUND`）和 `message`；即使请求中所有记录都被拒绝也按同样方式返回 200。`GET` 和 `PUT` 响应都会重新校验已持久化的记录：目录后来被删除的存量记录同样只出现在 `rejected` 中，不再阻塞同步；客户端可据此把失效工作区灰显并在路径恢复后自动复原。加载已持久化的快照时同样跳过失效路径并记录告警，因此目录被删除后 daemon 仍可正常启动。要彻底移除一条失效工作区记录，需要显式调用 `DELETE /v2/workspaces/{workspaceId}`；该操作允许目录已不存在的记录被删除。

工作区没有单独的信任状态：`workspace_roots` 就是执行边界。位于根目录（或 scratch 根）内的已有目录可以直接用于 Provider 模型/命令发现、prompt、MCP 调用、Git 写操作、本地终端和本地 Codex 启动，无需先做信任决定；每次启动前后端都会重新校验目录仍存在且未逃出根目录。因此 `workspace_roots` 应只包含允许 Agent 以 daemon 用户权限运行的目录。

`DELETE /v2/workspaces/{workspaceId}` 取消当前 owner 在该工作区内的活动 turn，并从目录中移除该工作区，但不会删除已有对话历史；响应为 `{ "workspaceId", "deleted", "cancelledTurns" }`。

早期版本按 owner 与路径记录工作区信任（`GET`/`PUT /v2/workspaces/{workspaceId}/trust`，未信任时返回 403 `WORKSPACE_TRUST_REQUIRED`）。该机制已移除：这两个路由不再存在（返回 404），daemon 启动时删除遗留的 `$DATA_DIR/workspace-trust.json`。

## 任务看板同步

任务看板的任务由后端持久化到 `$TODEX_AGENTD_DATA_DIR/kanban_tasks.json`，按认证 owner 隔离。与 workspace 清单一样，各客户端的本地存储只是离线缓存：连接后端后拉取快照、按 `updatedAt` 合并，本地变更经防抖后推送。任务通过 `workspaceId` 挂在工作区上，工作区 ID 与 `/v2/workspaces` 返回的稳定 ID 一致，因此不同设备恢复的是同一份任务列表。

```http
GET /v2/kanban/tasks
PUT /v2/kanban/tasks
```

`GET` 响应：

```json
{
  "tasks": [
    {
      "id": "task-m9k2x1-ab34cd",
      "tenantId": "local",
      "workspaceId": "ws_1f45e78a20d5a33556417b12",
      "title": "Ship the release",
      "description": "Tag and publish",
      "dueDate": "2026-10-01",
      "status": "planned",
      "conversationId": "conv_abc",
      "conversationIds": ["conv_abc", "conv_def"],
      "sortOrder": 0,
      "parentId": "task-m9k1a0-ef56gh",
      "priority": "high",
      "schedule": {
        "id": "sched-m9k3a0-x1y2z3",
        "at": "2026-10-10T09:30",
        "action": "start",
        "text": "任务：Ship the release",
        "provider": "codex",
        "model": "gpt-5.5",
        "permissionMode": "auto",
        "workMode": "implement",
        "status": "done",
        "firedAt": 1700000003000,
        "resultConversationId": "conv_def",
        "turnId": "turn_123"
      },
      "createdAt": 1700000000000,
      "updatedAt": 1700000001000,
      "deletedAt": null
    }
  ],
  "updatedAt": 1700000002000,
  "timeZone": { "name": "Asia/Shanghai", "offsetMinutes": 480 }
}
```

`conversationIds` 是任务名下新建的对话（顺序即显示顺序），`conversationId` 镜像第一项供只认单字段的旧客户端使用；推送时字段缺失表示客户端不认识它，后端保留已存的列表，显式的空数组才表示清空。`sortOrder` 缺失时同样保留原值。

`parentId` 指向同一 owner 下的父任务（子任务行，后端不做级联或存在性校验，每个子任务独立拥有状态、定时和对话）；`priority` 取 `none`、`low`、`medium`、`high`、`urgent`，`none` 表示显式清除。两个字段缺失时保留已存值（旧客户端）。

### 任务定时

`schedule` 由后端调度器执行，客户端无需在线。`at` 是**后端本地时区**的墙上时间 `YYYY-MM-DDTHH:MM`，响应中的 `timeZone` 给出该时区（`name` 为 IANA 名称，平台未报告时省略；`offsetMinutes` 为当前 UTC 偏移）。DST 跳过的时刻顺延一小时，重复的时刻取第一次。

- `action: "start"`：在任务所在工作区用 `provider`/`providerProfile`（缺省为 `[agent] default_agent`）新建对话并发送 `text`；`"send"`：把 `text` 发到 `conversationId`（后端对话 ID），对话正在运行时进入后端追加队列。`model`、`reasoningEffort`、`permissionMode`、`workMode` 是设定时由客户端快照的发送参数。
- `status`：客户端只写 `pending`（新建，`id` 由客户端生成）或 `cancelled`（取消，不得删除字段，缺失会被视为不认识该字段的旧客户端）。`running`、`done`、`failed` 只由后端写入，客户端以更新的 `updatedAt` 提交同一 `id` 的旧副本也不会回滚它们；后端写入时把 `updatedAt` 推到最新。
- 调度器约每 15 秒按墙上时间检查一次（机器休眠后仍会补跑）。执行前先置 `running` 并持久化；新建的对话 ID 在发送前写入 `resultConversationId` 并追加到 `conversationIds`。prompt 以 `kanban-<scheduleId>` 作为幂等键，后端重启后重跑不会重复发送；若重启时尚未记录新建的对话，则置 `failed` 而不冒险重复建对话。
- 结果：`done` 带 `resultConversationId`，`turnId` 缺失表示已排在运行中的 turn 之后；`failed` 带 `error`。后端停机超过 24 小时才到点的计划不再执行，记为 `failed`（`missed: …`）。发送遵循普通 prompt 的前提：工作区目录仍可用、存在历史接收者（否则 `HISTORY_KEY_REQUIRED`）、对话不是只读的旧明文历史。

`PUT` 请求体使用同样的 `tasks` 数组，是合并而非全量替换：后端按 `(tenantId, id)` upsert，`updatedAt` 较新的记录胜出，同毫秒时采用提交方记录。`tenantId` 一律由后端覆盖为当前认证 owner，客户端伪造无效。

删除以墓碑同步：客户端把记录标记为非空的 `deletedAt`（Unix 毫秒）后再推送，`GET`/`PUT` 响应会继续携带墓碑直到其超过 30 天保留期被清理。其他端拉取到墓碑后同样隐藏该任务；比墓碑更旧的写入无法复活任务。接口不提供单独的 `DELETE`，避免抹掉墓碑后被旧端重新上传。

后端校验与前端一致：`title` 非空且不超过 200 字符，`description` 不超过 2000 字符，`status` 为 `planned`、`in-progress` 或 `done`，`dueDate` 为 `YYYY-MM-DD`，`parentId` 不超过 128 字节且不等于任务自身 `id`，`priority` 为 `none`、`low`、`medium`、`high` 或 `urgent`；每个 owner 最多 500 条未删除任务，超出时整批拒绝（`INVALID_REQUEST`）。

### Workspace 目录浏览

```http
GET /v2/workspace/directories
GET /v2/workspace/directories?path=/home/user/projects/demo
```

响应：

```json
{
  "root": "/home/user/projects",
  "roots": ["/home/user/projects", "/srv/repos"],
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

`path` 为空时从主 workspace 根目录（`workspace_roots` 第一项）开始，也可以指向任一已配置根目录或其子目录。`root` 是包含 `current` 的那个根目录，`roots` 列出全部生效的根目录，`parent` 只允许向上走到所在根的边界。返回值只包含可进入的子目录，会跳过隐藏目录、文件，以及 canonical path 落在所有根目录之外的目录或符号链接。

```http
POST /v2/workspace/directories
```

请求体 `{ "parent": "/home/user/projects", "name": "demo" }`，在 `parent` 下新建一级目录，响应与 `GET` 相同，`current` 为新目录的规范路径。`parent` 必须是任一根目录或其子目录；`name` 去除首尾空白后必须是单个路径组件，否则返回 `400 INVALID_REQUEST`：非空、不超过 255 字节（按 UTF-8 字节计）、不是 `.`/`..`，在所有平台上都不含 `/ \ : * ? " < > |`、控制字符或 Unicode 格式字符（类别 Cf，如双向控制符 U+202A–U+202E、U+2066–U+2069，零宽字符 U+200B–U+200F、U+FEFF）；Windows 上还拒绝设备名（`CON`、`PRN`、`AUX`、`NUL`、`COM1`–`COM9`、`LPT1`–`LPT9` 及 `COM¹²³`/`LPT¹²³`，不区分大小写，带不带扩展名都算）和以 `.` 结尾的名称。以 `.` 开头的隐藏目录可以创建，但不会出现在目录列表中。从不创建多级目录，也从不覆盖：同名条目（包括悬空符号链接）已存在时返回 `409 CONFLICT`，并发创建同名目录只有一个成功。创建后若新目录的规范路径校验失败，会删除该目录并返回错误。其他错误均返回 `{ "code", "message" }`：请求体不是合法 JSON、字段缺失或类型不对、含未知字段、`Content-Type` 不是 `application/json` 时返回 `400 INVALID_REQUEST`；`parent` 不存在（或创建时已被删除）返回 `404 WORKSPACE_PATH_NOT_FOUND`；`parent` 不是目录、名称超出文件系统长度限制或被文件系统拒绝时返回 `400 INVALID_REQUEST`；没有写权限返回 `403 UNAUTHORIZED`；`parent` 在根目录之外返回 `403 WORKSPACE_PATH_OUTSIDE_ROOT`；磁盘已满返回 `507 STORAGE_LOW`。

### 文件 `@` 引用建议

```http
GET /v2/workspace/entries?cwd=/home/user/projects/demo&query=routes&limit=40
```

响应返回 `entries` 数组（`name`、`path`、`kind`）。`query` 为相对路径片段，支持递归匹配（跳过 `node_modules`、`target`、`.git` 等大目录，且不进入符号链接）；以 `/` 结尾时按目录直接列出，被列出的目录解析符号链接后必须仍在某个 `workspace_roots` 根目录内，否则返回空列表。

### 文件预览

```http
GET /v2/workspace/file?path=/home/user/projects/demo/README.md
```

路径必须是 `workspace_roots` 内的绝对路径且指向文件。文本超过 1 MiB、图片超过 8 MiB 拒绝预览。响应包含 `name`、`path`、`mimeType`、`sizeBytes`；任何可解码为 UTF-8 的非图片文件都附带 `text` 内容（`mimeType` 仍按扩展名白名单分类），图片附带 `dataUrl`。保存（`PUT` 文本写回）仅限 `text/*` 与 `application/json` 类型。

### Git 扫描与操作

Git HTTP API 只接受 `workspace_roots` 内的现有目录。服务端会递归检查该目录及最多两层子目录中的仓库，并返回与桌面端 Git 面板兼容的摘要：

```http
GET /v2/git/scan?workspacePath=/home/user/projects/demo
```

响应：

```json
{
  "repositories": [
    {
      "path": "/home/user/projects/demo",
      "name": "demo",
      "branch": "main",
      "files": [{ "path": "src/main.rs", "status": " M" }],
      "additions": 3,
      "deletions": 1,
      "initialEligible": false,
      "filesTruncated": false
    }
  ]
}
```

没有 Git 仓库的 workspace 仍会返回一个 `initialEligible: true`、`branch: "UNINITIALIZED"` 的占位摘要。`files` 和用于未跟踪行数统计的路径各自最多处理 2,000 项；单个命令的 stdout/stderr 各自最多读取 4 MiB，某个仓库的输出超限时只在该仓库摘要上返回 `error`（`branch: "UNKNOWN"`），其余仓库照常返回。扫描最多并发执行 2 个请求，排队超过 2 秒返回 `409 CONFLICT`，单个扫描请求总计超过 30 秒返回 `GIT_COMMAND_TIMED_OUT`。

单个变更文件的统一 diff 通过只读接口获取：

```http
GET /v2/git/diff?workspacePath=/home/user/projects/demo&path=src/main.rs
```

`path` 为仓库相对路径，禁止绝对路径和 `..`。响应对照 `HEAD`（无首个提交时为空树）返回 `repositoryPath`、`path`、统一格式的 `diff` 文本与 `truncated`；未跟踪文件与 `/dev/null` 对比返回新增内容并标记 `untracked: true`，未变更的已跟踪文件返回空 `diff`。响应文本最多 1 MiB，超出时 `truncated` 为真。

省略 `path` 时返回整个仓库工作区的 diff：

```http
GET /v2/git/diff?workspacePath=/home/user/projects/demo
```

```json
{
  "repositoryPath": "/home/user/projects/demo",
  "path": "",
  "diff": "diff --git a/src/main.rs b/src/main.rs\n…",
  "truncated": false,
  "untracked": false,
  "base": "3f2a…（完整 40 或 64 位）",
  "baseRef": "upstream"
}
```

对比基准 `base` 按以下顺序确定，`baseRef` 标明来源：当前分支有上游时为 `git merge-base HEAD @{upstream}`（`"upstream"`，因此包含尚未推送的本地提交）；没有上游、分离 HEAD 或与上游没有共同祖先时为 `HEAD`（`"head"`）；尚无首个提交时为空树（`"empty"`，`base` 为空树 id）。`diff` 包含已跟踪文件的已暂存与未暂存改动（`--find-renames`），随后是未跟踪且未被忽略的文件（按新增文件与 `/dev/null` 对比，文件名为仓库相对路径；超过 4 MiB 的文件按二进制文件显示，不读取内容；符号链接和仓库外的路径跳过）。同样禁用 external diff/textconv 与颜色，执行只读接口的配置检查；文本最多 1 MiB，超出或未跟踪文件超过 2,000 个时 `truncated` 为真。整仓 diff 单次请求超过 30 秒返回 `GIT_COMMAND_TIMED_OUT`。`base`/`baseRef` 只在整仓模式下出现；`path` 为空字符串，`untracked` 恒为 `false`。

当前仓库的轻量状态摘要（客户端轮询用）：

```http
GET /v2/git/status?workspacePath=/home/user/projects/demo
```

与扫描不同，子目录会解析到其所在仓库，且不检查子仓库。响应：

```json
{
  "repositoryPath": "/home/user/projects/demo",
  "initialized": true,
  "branch": "main",
  "worktreeKind": "main",
  "changedFiles": 2,
  "additions": 3,
  "deletions": 1,
  "statsTruncated": false,
  "upstream": "origin/main",
  "ahead": 2,
  "behind": 0
}
```

`branch` 在分离 HEAD 时为 `null`；`worktreeKind` 为 `main` 或 `linked`。`upstream` 是当前分支的上游（无上游或分离 HEAD 时为 `null`）。有上游时 `ahead`/`behind` 为相对上游领先/落后的提交数；无上游时 `ahead` 为不在任何远端跟踪分支上的提交数、`behind` 为 `null`；仓库没有任何远端或尚无首个提交时三者都为 `null`。数字只反映本地已知的远端跟踪引用，服务端不会为此执行 fetch。

当前分支的提交历史按页读取：

```http
GET /v2/git/log?workspacePath=/home/user/projects/demo&skip=0&limit=5
```

`skip` 缺省为 0、最大 100,000；`limit` 缺省为 5、取值 1–50，越界返回 `400`。子目录同样解析到所在仓库。响应按 `git log` 默认顺序（最新在前）：

```json
{
  "repositoryPath": "/home/user/projects/demo",
  "initialized": true,
  "commits": [
    {
      "sha": "3f2a…（完整 40 或 64 位）",
      "subject": "fix: 修复登录跳转",
      "authorName": "Alice",
      "authoredAt": 1727760000,
      "pushed": false
    }
  ],
  "hasMore": true
}
```

`authoredAt` 为 Unix 秒；`subject` 最多 512 bytes、`authorName` 最多 256 bytes。`pushed` 与状态摘要使用同一比较基准（上游，或无上游时的任意远端）：`false` 表示尚未推送；仓库没有远端，或未推送提交超过 10,000 个而无法判定时为 `null`。尚无首个提交的仓库返回空 `commits`。状态、diff 与提交历史都是只读接口，与扫描共享 2 个并发名额（排队超过 2 秒返回 `409 CONFLICT`），单次请求超过 10 秒返回 `GIT_COMMAND_TIMED_OUT`，并执行与状态摘要相同的元数据与仓库级可执行配置检查。只读接口（scan、status、log、diff、workspace）的配置检查只拒绝读取时可能运行程序的配置（clean/smudge/process filter、askpass、credential helper、`core.sshCommand`、`core.gitProxy`、GPG program、remote helper），不检查远端 URL 与 URL 重写：远端指向本地路径（如 `/tmp/x.git`）或 `file://` 的仓库照常显示状态、历史与 diff。

仓库变更只能通过固定动作执行，不能传递任意 Git 子命令或参数：

```http
POST /v2/git/run
Content-Type: application/json

{
  "workspacePath": "/home/user/projects/demo",
  "action": "commit-push",
  "message": "同步移动端改动",
  "includeUnstaged": true
}
```

`action` 仅支持 `initial`、`commit`、`commit-push` 和 `push`。写操作的 `workspacePath` 必须是仓库根目录本身，不能用仓库内的任意子目录隐式选中祖先仓库；`initial` 只允许用于未初始化目录或尚无首个提交的仓库。`message` 仅用于提交动作，超过 512 bytes 或包含 NUL/不允许的控制字符会被拒绝；缺省时服务端生成安全的提交说明。`includeUnstaged` 缺省为 `true`，为真时先执行固定的 `git add -A`。成功响应：

```json
{
  "repositoryPath": "/home/user/projects/demo",
  "action": "commit-push",
  "output": "..."
}
```

`output` 是 Git 的 stdout/stderr 文本；Git 没有输出时为空字符串，客户端自行显示本地化的完成提示。`/v2/git/operation` 的成功响应同理。

#### 工作区快照、Pull Request 与固定 Git 操作

Git 操作面板先读取工作区的分支与 worktree 快照：

```http
GET /v2/git/workspace?workspacePath=/home/user/projects/demo
```

```json
{
  "repositoryPath": "/home/user/projects/demo",
  "initialized": true,
  "currentBranch": "main",
  "branches": [
    { "name": "main", "current": true, "remote": false, "worktreePath": "/home/user/projects/demo" },
    { "name": "feature", "current": false, "remote": false, "worktreePath": "/home/user/projects/demo-feature" },
    { "name": "origin/main", "current": false, "remote": true }
  ],
  "worktrees": [
    { "path": "/home/user/projects/demo", "branch": "main", "current": true, "main": true, "locked": false, "dirty": false, "accessible": true },
    { "path": "/home/user/projects/demo-feature", "branch": "feature", "current": false, "main": false, "locked": false, "dirty": false, "accessible": true }
  ],
  "dirty": false
}
```

`workspacePath` 必须是仓库根目录本身：目录不在任何仓库中时返回 `initialized: false`、`currentBranch: ""`、空的 `branches` / `worktrees` 与 `dirty: false`；指向仓库的子目录返回 `400 INVALID_REQUEST`。`currentBranch` 在分离 HEAD 时为空字符串。`branches` 依次列出 `refs/heads`（`remote: false`）与 `refs/remotes`（`remote: true`，名称带远端前缀，如 `origin/main`）中的分支，跳过 `origin/HEAD` 这类符号引用；`current` 只会出现在本地分支上；`worktreePath` 只在本地分支被某个 worktree 检出时出现。`worktrees` 按 `git worktree list` 顺序返回，第一项为主 worktree（`main: true`），分离 HEAD 的 worktree `branch` 为空字符串；`current` 标出 `workspacePath` 本身；`accessible` 为 `false` 表示该 worktree 不在 `workspace_roots` 内或其 Git 元数据没有通过校验；worktree 的 `dirty` 把被忽略的文件也算作改动（删除 worktree 会一并删除它们），不可访问的 worktree 恒为 `dirty: true`。顶层 `dirty` 按普通 Git 语义统计已跟踪改动与未忽略的未跟踪文件。引用超过 2,000 个或 worktree 超过 128 个时返回 `400 INVALID_REQUEST`。

当前分支对应的 GitHub Pull Request 摘要：

```http
GET /v2/git/pull-request?workspacePath=/home/user/projects/demo
```

```json
{
  "repositoryPath": "/home/user/projects/demo",
  "initialized": true,
  "branch": "feature",
  "pullRequest": {
    "number": 12,
    "title": "Add feature",
    "url": "https://github.com/owner/demo/pull/12",
    "state": "open",
    "draft": false,
    "headRef": "feature",
    "baseRef": "main",
    "headSha": "3f2a…（完整 40 位）",
    "mergeable": "mergeable",
    "mergeState": "clean",
    "autoMergeMethod": "squash",
    "reviews": { "approved": 1, "changesRequested": 0, "commented": 2 },
    "checks": { "passing": 5, "failing": 0, "pending": 1 }
  }
}
```

`workspacePath` 的要求与快照相同；不是仓库时返回 `initialized: false`、`branch: ""`、`pullRequest: null`，分离 HEAD 时 `branch` 为空字符串、`pullRequest` 为 `null`。否则服务端解析当前分支的上游（必须是某个远端的 `refs/heads/*` 分支），远端 URL 须为 `https://<host>/<owner>/<repo>`、`ssh://git@<host>/<owner>/<repo>` 或 `git@<host>:<owner>/<repo>` 形式（可带 `.git`）；没有上游、上游不是分支或 URL 不是这些形式时返回 `400 INVALID_REQUEST`。查询通过 daemon 主机上的 GitHub CLI（`gh api --hostname <host>`，只继承 `PATH`、`HOME`、`GH_CONFIG_DIR`、`GH_TOKEN`、`GITHUB_TOKEN`、`GH_ENTERPRISE_TOKEN`、`GITHUB_ENTERPRISE_TOKEN`、代理与证书等白名单环境变量，`GH_PROMPT_DISABLED=1`）进行，`gh` 失败返回 `500 GIT_PROCESS_ERROR`。服务端在 `head=<owner>:<上游分支>` 最近的 30 个 PR（任意状态）中优先取打开的，否则取最新的一个；没有时 `pullRequest` 为 `null`。字段：`title` 最多 512 bytes；`state` 为 `open`、`closed` 或 `merged`；`mergeable` 为 `mergeable`、`unmergeable` 或 `unknown`（GitHub 仍在计算）；`mergeState` 原样透传 GitHub 的 `mergeable_state`（如 `clean`、`blocked`、`behind`、`dirty`、`unstable`、`unknown`）；`autoMergeMethod`（`merge` / `squash` / `rebase`）只在已开启自动合并时出现；`reviews` 只统计每位评审者最新一次已提交的评审；`checks` 合计 head 提交的 check run（未完成计 `pending`，结论为 `success` / `neutral` / `skipped` / `stale` 计 `passing`，其余结论计 `failing`）与 commit status（`success` / `pending` / 其余分别计入 `passing` / `pending` / `failing`）。

快照与 PR 摘要都是只读接口，与扫描共享 2 个并发名额（排队超过 2 秒返回 `409 CONFLICT`），单次请求超过 30 秒返回 `GIT_COMMAND_TIMED_OUT`，并执行与状态摘要相同的仓库元数据检查。

分支、worktree 与 PR 的变更只能通过固定动作执行：

```http
POST /v2/git/operation
Content-Type: application/json

{
  "workspacePath": "/home/user/projects/demo",
  "operation": { "action": "create-worktree", "path": "/home/user/projects/demo-feature", "branchName": "feature", "startPoint": "main" }
}
```

成功响应与 `/v2/git/run` 相同：`{ "repositoryPath", "action", "output" }`，`action` 回显动作名。请求体与 `operation` 都拒绝未知字段，`operation` 只能带所选动作的字段；JSON 无法按下表解析时返回 `400 INVALID_REQUEST`（`invalid Git operation: …`）。`action` 共 14 个：

| `action` | 字段 | 行为与前置条件 |
| --- | --- | --- |
| `init` | 无 | 仅用于不在任何仓库中的目录，执行 `git init --template=`；已初始化返回 `400`，目录下已有无效的 `.git` 元数据也返回 `400`。 |
| `push` | 无 | 把 `HEAD` 推到当前分支上游的同名远端分支（`HEAD:<上游 ref>`，不 force、不推标签）。当前分支须有分支上游；远端名只能含字母、数字、`.`、`_`、`-`、`/`。 |
| `create-branch` | `branchName`，`startPoint`? | 从 `startPoint`（缺省 `HEAD`，先解析为提交 id）创建分支，不切换。 |
| `switch-branch` | `branchName` | 工作区有改动或该分支已在其他 worktree 检出时返回 `400`；`git switch --no-guess`，不会从远端分支隐式创建本地分支。 |
| `create-worktree` | `path`，`branchName`，`startPoint`? | 新建分支并在 `path` 检出新 worktree。`path` 须为不含 `.` / `..` 的绝对路径且尚不存在，最近的已有上级目录须在 `workspace_roots` 内，缺失的中间目录逐级创建。 |
| `remove-worktree` | `path` | 只删除登记在本仓库、可访问、未锁定、干净（含被忽略文件）且不是主 worktree 或 `workspacePath` 本身的 linked worktree，其余情况返回 `400`。 |
| `create-pr` | `title`，`body`，`baseBranch`，`draft`，`repository` | 全部必填。`title` 非空、最多 256 bytes、不含控制字符；`body` 最多 60,000 bytes，除换行、回车与制表符外不含控制字符；`repository` 为 `[host/]owner/repo`（缺省 host 为 `github.com`），必须与当前分支上游的仓库一致（fork 工作流交给 Agent）；`baseBranch` 须是合法分支名且不同于上游分支；远端上游分支须已指向本地 `HEAD`（先推送）。已有同一 head/base 的打开 PR 时直接返回其 URL，否则创建（`draft` 决定是否为草稿）。`output` 为 PR URL。 |
| `close-pr` | 无 | 关闭打开的 PR。 |
| `reopen-pr` | 无 | 重新打开已关闭（未合并）的 PR。 |
| `draft-pr` | 无 | 把打开的 PR 转为草稿。 |
| `ready-pr` | 无 | 把草稿 PR 标为可评审。 |
| `merge-pr` | `method`，`headSha` | `method` 为 `merge`、`squash` 或 `rebase`；`headSha` 须与 PR 当前 head 一致（不区分大小写），否则返回 `400`（`PR head changed; refresh before merging`）；合并请求绑定该 sha。 |
| `enable-pr-auto-merge` | `method` | 为打开且尚未开启自动合并的 PR 开启自动合并，`method` 同上。 |
| `disable-pr-auto-merge` | 无 | 关闭已开启的自动合并。 |

`branchName` 最多 240 bytes，不能以 `-` 开头、不能含 `@` 或控制字符，并须通过 `git check-ref-format --branch`；`startPoint` 同样限制长度与字符，无法解析为提交时返回 `422 GIT_COMMAND_FAILED`。PR 动作作用于 `GET /v2/git/pull-request` 选中的那个 PR（没有时返回 `400`），每次写入前重新读取 PR 状态，状态不满足前置条件（例如 PR 未打开、已是草稿、已合并的 PR 不能重开）时返回 `400` 且不写入；`create-pr` 以外的 PR 动作 `output` 为一行说明加 PR URL（如 `Closed PR #12\nhttps://github.com/owner/demo/pull/12`）。

所有动作都要求 `workspacePath` 是仓库根目录本身（子目录返回 `400`），除 `init` 外目录不是仓库时返回 `404 GIT_REPOSITORY_NOT_FOUND`；非 Unix 平台返回 `501 UNSUPPORTED`。该租户有处于运行或等待权限状态的会话、且其工作目录位于 `workspacePath`（或 `remove-worktree` 的目标）之内时返回 `409 CONFLICT`，要求等待 Agent 结束。变更与 `/v2/git/run` 共用一把写锁，排队超过 2 秒返回 `409 CONFLICT`；整个动作超过 120 秒返回 `502 GIT_PARTIAL_SUCCESS`。本地 Git 命令以非零状态退出且可以确定没有改变状态时返回 `422 GIT_COMMAND_FAILED`（push 的判定见下文）；结果可能已生效时返回 `502 GIT_PARTIAL_SUCCESS`，包括 PR 写请求失败、超时或 GitHub 响应没有确认新状态——客户端应刷新状态后再决定，不能自动重试。GitHub GraphQL 返回顶层错误表示变更被拒绝而未生效，返回 `500 GIT_PROCESS_ERROR`；写入之前的 `gh` 查询失败同样返回 `500 GIT_PROCESS_ERROR`。

Git 进程不经过 shell，stdin 关闭并设置 `GIT_TERMINAL_PROMPT=0`；子进程只继承必要的基础运行环境，不继承 `TODEX_AGENTD_*` 或 `GIT_DIR`/`GIT_WORK_TREE` 等路径覆盖变量。所有命令显式关闭 fsmonitor 与 untracked cache，差异统计禁用 external diff/textconv。写操作会再次确认 worktree、git-dir 和 git-common-dir 全部位于 `workspace_roots` 内，拒绝写入相关 Git 元数据树中的符号链接和 object alternates，并使用 daemon 管理的空 hooks 目录、关闭 commit/push 签名和 `ext`/`file` transport；local 与 worktree 级配置都会展开 `include` 后检查，包含 clean/process filter、askpass、local credential helper、`core.sshCommand`、自定义 GPG program、自定义 remote helper 或仓库级 URL 重写时直接拒绝。推送（`push`、`commit-push` 及工作区 Git 操作的 push）固定传入 `-c submodule.recurse=false` 与 `--recurse-submodules=no`，不会递归推送子模块；仓库级配置含 `push.recurseSubmodules` 或 `submodule.recurse`（要求递归推送）时拒绝推送；`git submodule init` 写入的 `submodule.<name>.*` 等普通条目不影响推送（`INVALID_REQUEST`，提示移除该设置或在终端推送），提交等其他写操作不受影响。上游远端名只接受 `[A-Za-z0-9._/-]` 字符，不能以 `-` 开头，按 `/` 分隔的每一段不能为空或以 `.` 开头（允许 `upstream/fork`，拒绝 `..`）。所有写操作都拒绝选择自定义 transport 的远端 URL（`<helper>::…`、未知 scheme）与仓库级 URL 重写。会联系远端的操作（`push`、`commit-push` 及工作区 Git 操作的 push）还要求仓库的每个远端只使用 HTTP(S)、SSH、Git URL 与 SCP-like SSH 写法：远端指向本地路径或 `file://` 时返回 `400 INVALID_REQUEST`，信息为 `remote '<name>' points to a local path; TodeX does not push to local-path remotes (push from a terminal, or point the remote at an http(s), ssh or git URL)`；`commit-push` 在提交之前就做此检查，因此不会只提交不推送。不联系远端的写操作（`initial`、`commit`、init、创建/切换分支、创建/删除 worktree）在这类仓库中照常可用，只读接口同样不受此限制。daemon 账户自己的全局 credential/SSH 配置仍可用于正常的非本地远端推送。同一 daemon 内的 Git 写操作串行执行，排队超过 2 秒返回 `409 CONFLICT`，单次写请求总计超过 120 秒返回 `GIT_PARTIAL_SUCCESS`，提醒客户端状态可能已改变、刷新后再决定后续动作。每条命令 15 秒超时；超时、输出超限或外层请求取消都会终止 Git 进程组并回收直接子进程。完整进程组清理依赖 Unix，因此非 Unix 平台的 Git 写 API 返回 `501 UNSUPPORTED`；Git 扫描仍可使用。

会改变仓库的 Git 请求（`/v2/git/run` 与 `/v2/git/operation`）通过认证、schema 与路径校验并进入 Git 执行阶段后，会尝试写入 `$TODEX_AGENTD_DATA_DIR/audit/audit.jsonl` 的 `git.audit` 事件；只读接口（scan、status、log、workspace、diff、pull-request）不写审计。审计记录只包含动作、路径、结果码和输出长度，不记录提交说明。无效 token、无效 JSON/action 和路径校验失败发生在审计事件创建之前。Git 副作用完成后的审计 I/O 失败会记录 daemon 警告，但不会把已经成功的操作伪装成失败响应。`audit.jsonl`（0600）由 daemon 常驻句柄追加并逐条 fsync，超过 16 MiB 时轮转为 `audit.jsonl.1`…`.3`（最多保留 3 份旧文件；先把活动文件移开再依次后移旧文件，轮转失败时记录 warn 并继续追加到活动文件，约 60 秒后重试，不会让写入失败或删除旧轮转文件）；审计记录只写文件，不进入 WebSocket 事件流。终端审计（`terminal.audit`，每条 `terminal.start` / `input` / `resize` / `stop` / `status` 一条）写入独立的 `audit-terminal.jsonl`，按同样规则独立轮转与计额，无法挤掉 `audit.jsonl` 中的 Git、Codex 等审计；允许的 `terminal.input` / `terminal.resize` 记录批量写入，最多每 250 ms fsync 一次；批量写入失败时记录 warn，未写入的记录按原顺序放回队首（积压上限 10000 条，超出时丢弃最旧的并记录 warn），此后每条批量记录对应的终端操作都会被拒绝，并每 250 ms 重试一次写入；直到某次重试或一次逐条写入成功后才恢复（fail-closed 最多滞后一个批次间隔）。其余终端审计记录仍逐条 fsync 且 fail-closed（写入失败则拒绝该终端操作），写入前先写出排队的批量记录以保持顺序。

Git API 使用以下错误码（均为统一 JSON 错误 envelope 的 `code` 字段）：`GIT_UNAVAILABLE`、`GIT_REPOSITORY_NOT_FOUND`、`GIT_COMMAND_FAILED`、`GIT_PARTIAL_SUCCESS`、`GIT_COMMAND_TIMED_OUT`、`GIT_OUTPUT_LIMIT_EXCEEDED`、`GIT_PROCESS_ERROR`、`GIT_SCAN_LIMIT_EXCEEDED`、`UNSUPPORTED`。`GIT_COMMAND_FAILED`（HTTP 422）表示 Git 命令以非零状态退出且可以确定没有改变状态：本地操作（init、创建/切换分支、创建/删除 worktree 等）失败，或 push 确定没有更新任何远端 ref——push 以 `--porcelain` 运行，只有被拒绝（`!`）或已是最新（`=`）的 ref 行，或在与远端交换 ref 之前就失败（主机密钥校验失败、`Permission denied (publickey)`、无法解析主机、连接被拒绝/超时、认证失败、仓库不存在等）；主机密钥校验失败时 `detail` 末尾附带提示 `Connect to the host once in a terminal to confirm its host key.`。`GIT_PARTIAL_SUCCESS`（HTTP 502）表示 `commit-push` 已创建本地提交但后续 push 失败，或写请求触及总时限、Git 进程被终止或超时、push 结果无法判定（例如部分 ref 已更新，或传输中途断开）而无法证明仓库/远端完全未变；客户端必须刷新仓库状态且不得自动重试提交。路径越界仍返回通用的 `WORKSPACE_PATH_OUTSIDE_ROOT`。

### 浏览器代理

```http
POST /v2/browser/fetch
{"url": "https://example.com/page"}
```

仅允许指向本后端 loopback 的 `http`/`https` URL；禁用系统代理，最多跟随 3 次且每一跳仍必须是 loopback。响应返回最终 `url`、实际 `status`、`contentType`、`body`（≤2 MiB）。

```http
POST /v2/browser/proxy
Content-Type: application/json

{"method": "GET", "url": "http://127.0.0.1:5241/src/main.tsx?t=1", "headers": [["cookie", "a=b"], ["accept", "*/*"]], "body": "<base64，可选>"}
```

桌面端浏览器预览把页面的每个 loopback 请求转发到这里（后端在远程主机时经 `/v2/sealed` 隧道），由后端在本机 loopback 上执行并把响应流式返回。认证与 `/v2/browser/fetch` 相同（设备签名）。因为隧道只转发内层请求的 `content-type`/`accept`，页面自己的请求头放在 body 的 `headers` 中。

- `url` 与 `/v2/browser/fetch` 使用同一 loopback 规则（`localhost`、`127.0.0.1`、`[::1]`，无用户名密码），并且不能是 daemon 自己的端口；否则 400 `INVALID_REQUEST`。
- `method` 为任意合法 HTTP token 方法（`GET`、`HEAD`、`POST`、`PUT`、`PATCH`、`DELETE`、`OPTIONS`…），`CONNECT` 与非法方法返回 400 `INVALID_REQUEST`。
- `headers` 为 `[name, value]` 数组，可重复；丢弃 hop-by-hop 头（`connection`、`keep-alive`、`proxy-*`、`te`、`trailer`、`transfer-encoding`、`upgrade`）、`host`、`content-length` 与所有 `x-todex-*`，其余（`cookie`、`authorization`、`origin`、`referer`、`range`、`if-none-match` 等）原样转发。非法头名或头值返回 400。
- `body` 为标准 base64（带 padding），解码后最多 16 MiB；本路由的请求体上限相应放宽到约 29 MiB。
- 不使用系统代理、不跟随重定向（3xx 原样返回，由预览浏览器自己跟随）、连接超时 10 秒、无总超时、不自动解压（`content-encoding` 原样透传）。
- 成功时内层 HTTP 状态恒为 `200`（与上游状态无关），并带：
  - `x-todex-proxy-status`：上游状态码；
  - `x-todex-proxy-headers`：上游响应头（去掉 hop-by-hop 头）组成的 `[[name, value], …]` JSON 数组的 base64url（无 padding）编码；重复头（如多个 `set-cookie`）保持为独立条目，值按 UTF-8 宽松解码。编码后超过 48 KiB 时返回 502 `BROWSER_TARGET_UNREACHABLE`；
  - `content-type: application/octet-stream` 与 `cache-control: no-store`（只描述外层包装，上游的头都在 `x-todex-proxy-headers` 中）。
  - body 为上游响应体，边收边转发（不受 2 MiB 限制）；上游中途断开时响应体随之中断。
- 无法从上游得到响应（连接被拒、超时、DNS 失败、连接重置等）时返回 502 `BROWSER_TARGET_UNREACHABLE`，`message` 含目标 `host:port`。

## SSH 主机与远程连接

管理**运行 todex-agentd 的机器**上的 SSH 主机、密钥与远程文件，并把选定主机开放给 Agent。所有 `/v2/ssh/*`、`/v2/ftp/*`、`/v2/remote/*` 接口都需要设备签名。TodeX 从不保存密码或密钥口令，从不写入 `~/.ssh/config`，私钥内容不会出现在任何响应或日志中。

### 主机清单

- 自动发现：递归解析 `~/.ssh/config`（跟随 `Include`，相对路径按 `~/.ssh` 解析、glob 按字典序展开），列出具体的 `Host` 别名；通配符、`!` 否定模式与 `Match` 块只参与连接参数解析，不单独列出。
- TodeX 自管主机与 FTP 站点保存在 `$DATA_DIR/ssh/store.json`，SSH 主机渲染为 `$DATA_DIR/ssh/hosts.conf`（均为 0600）。主机的 `authMethod` 记在 `store.json` 顶层的 `authMethods`（别名 → `password`，`key` 不记录），不写进主机记录，因此回退到 v2.3.0/v2.3.1 的 daemon 仍能读取该文件并启动（这些主机在旧版本中按密钥主机处理）；v2.3.1 写在主机记录里的 `authMethod` 在加载时迁移到 `authMethods` 并重写文件（重写失败只记警告），无法识别的认证方式按 `key` 处理。`store.json` 中的 `connection` 读取时缺失字段取默认值、忽略未知字段，越界值收敛到合法范围（`PUT /v2/ssh/settings` 仍严格校验）。
- 所有 ssh 调用都使用生成的 `-F $DATA_DIR/ssh/ssh_config`，依次 `Include` 自管主机、`~/.ssh/config`、TodeX 连接默认值（`$DATA_DIR/ssh/defaults.conf`）与系统 `ssh_config`，所以密钥、ssh-agent、ProxyJump、`known_hosts` 行为与用户 shell 中的 `ssh` 一致；有效参数由 `ssh -G` 解析（缓存 30 秒）。Unix 上同一主机的终端、SFTP 与 Agent 命令通过 `ControlMaster`（`$DATA_DIR/ssh/cm/%C`，空闲 5 分钟关闭）复用一次登录。
- 带密码的 ssh 调用（连接测试、终端、SFTP）由 askpass 辅助程序（`todex-agentd ssh-askpass`）应答：daemon 把密码写入一次性文件 `$DATA_DIR/ssh/askpass/<随机名>`（目录 0700、文件 0600），ssh 子进程环境中只有该文件路径 `TODEX_SSH_ASKPASS_FILE`，密码不进入任何进程环境或命令行。辅助程序只回答密码与密钥口令提示（主机密钥确认等其他提示一律拒绝）；回答密码提示时读取并立即删除该文件，口令提示不删除。连接测试在 ssh 退出后、SFTP 在握手完成后删除该文件；终端的 ssh 可能成为长期共享 master，文件最迟在终端启动 120 秒后删除。daemon 启动时清理残留的密码文件。密码不能包含 NUL、CR 或 LF，且最长 4096 字节，否则返回 400 `INVALID_REQUEST`（错误信息不含密码）；带密码时总是重新运行 `ssh -G` 检查 ProxyJump/ProxyCommand，不使用上述缓存。

| 接口 | 说明 |
| --- | --- |
| `GET /v2/ssh/hosts` | `{ hosts: SshHost[], ftpSites: FtpSite[], localTerminal }`。`localTerminal`：daemon 在 Linux/macOS 上为 `true`，表示可以打开 `Local` 终端（见「SSH 终端」）；旧 daemon 没有该字段。`SshHost`：`alias`、`source`（`sshConfig` 只读 / `managed`）、`sourcePath?`、`agentAccess`、`resolved?`（`hostName`、`user`、`port`、`proxyJump`、`identityFiles`）、`resolveError?`、`managed?`（可编辑定义）。 |
| `POST /v2/ssh/hosts` | 新建自管主机 `{ alias, hostName, user?, port?, identityFile?, proxyJump?, options?: [{key, value}], authMethod? }`。`authMethod` 为 `key`（默认，省略时即此值）或 `password`：`password` 主机渲染 `PreferredAuthentications keyboard-interactive,password` 与 `PubkeyAuthentication no` 并忽略 `identityFile`；密码本身从不保存。别名已存在于 `~/.ssh/config` 或自管列表返回 409；拒绝换行、引号、以 `-` 开头的值以及 `Host`/`Match`/`Include`/`LocalCommand`/`PermitLocalCommand`/`KnownHostsCommand` 选项。 |
| `POST /v2/ssh/hosts/import` | `{ text }` 粘贴 `ssh_config` 片段，返回 `{ hosts, errors }`；通配符、`Match`、`Include` 只报告不导入。 |
| `PUT` / `DELETE /v2/ssh/hosts/{alias}` | 修改或删除自管主机（`~/.ssh/config` 中的主机只读）。 |
| `PUT /v2/ssh/hosts/{alias}/agent-access` | `{ enabled }`，默认关闭，见「Agent SSH 工具」。 |
| `POST /v2/ssh/hosts/{alias}/test` | 以 BatchMode、严格主机密钥校验执行一次 `exit 0`；可选请求体 `{ password? }` 时改为只应答一次密码提示（askpass，一次性密码文件见上文；ProxyJump/ProxyCommand 主机返回 400），且以 `ControlMaster=no`、`ControlPath=none` 单独建立连接，既不复用已有的共享 master（否则任意密码都会显示成功），也不会成为新的 master。请求体可省略或为空白（与 `{}` 相同；不检查 `Content-Type`），`password` 为 `null` 或空字符串时按无密码测试；含其他字段、不是 JSON 或类型不符时返回 400 `INVALID_REQUEST`（`{ code, message }`，不回显请求内容）。返回 `{ ok, durationMs, failure?, detail? }`；`failure` 为 `hostKeyUnverified`、`hostKeyChanged`、`authenticationFailed`、`unreachable`、`timedOut`、`other`。 |
| `POST /v2/ssh/hosts/{alias}/disconnect` | `ssh -O exit` 关闭共享 master，返回 `{ disconnected }`。 |
| `POST /v2/ssh/hosts/{alias}/login` | 应答 Agent 的登录请求（`ssh.login.requested`，见「Agent SSH 工具」）。请求体 `{ requestId, password?, saved? }`：带 `password` 时用它登录（askpass 一次性密码文件，ProxyJump/ProxyCommand 主机返回 400），以 `ControlMaster=auto`、`ControlPersist=300` 运行一次 `exit 0`，登录后的连接留作共享 master（已有 master 时直接复用）；省略 `password`（或为空字符串）表示拒绝该请求。`saved` 表示密码来自客户端保存的密码，仅记录在 `ssh.login.resolved` 中。返回 `{ status: "connected" }`（同一主机的所有待处理请求一并结束）、`{ status: "declined" }`，或密码无效时 `{ status: "failed", failure, detail? }`——此时请求保持待处理，可再次应答直到超时。请求已结束、不存在或不属于该主机时返回 404；请求体缺少 `requestId`、含其他字段或不是 JSON 时返回 400 `INVALID_REQUEST`（不回显请求内容）。 |
| `GET` / `PUT /v2/ssh/agent-local` | Agent 本机终端 `{ enabled }`，默认 `false`；`PUT` 需提供完整请求体，多余字段或类型不符返回 400 `INVALID_REQUEST`。保存在 `store.json` 顶层的 `agentLocal`（无法解析按关闭）。见「Agent SSH 工具」中的 `Local`。旧 daemon 上此路由为 404。 |
| `GET` / `PUT /v2/ssh/agent-output-filter` | Agent 输出隐私过滤 `{ enabled, categories: { privateKey, publicKey, credential, bankCard, idCard, proxyCredential, urlToken, highEntropy, uuid, email, phone, publicIp }, customKeys: string[] }`。默认：前六类为 `true`，`urlToken`、`highEntropy`、`uuid`、`email`、`phone`、`publicIp` 为激进选项、默认 `false`，`customKeys` 为空。`PUT` 必须带 `enabled`（布尔）与 `categories`（对象）；`categories` 中省略的类别与省略的 `customKeys` 保持当前保存的值（旧客户端只发送前五类时不会重置新开关）；未知字段、未知类别或非布尔值返回 400 `INVALID_REQUEST`。`customKeys` 最多 32 个，每个 1–64 个 `[A-Za-z0-9_.-]` 字符，否则 400；按不区分大小写去重。返回保存后的完整设置。保存在 `store.json` 顶层的 `agentOutputFilter`（读取时缺失的类别取上述默认值、非布尔值与未知类别忽略、无效的 `customKeys` 项丢弃，整体无法解析按默认值）；从下一次 `ssh_exec` 起生效，无需重启会话。规则见「Agent SSH 工具」。旧 daemon 上此路由为 404，客户端据此隐藏设置；客户端只显示与发送 `GET` 返回中出现的类别与字段。 |
| `GET` / `PUT /v2/ssh/settings` | 连接设置 `{ serverAliveEnabled, serverAliveInterval, serverAliveCountMax }`，默认 `{ false, 15, 3 }`；`PUT` 需提供全部字段（间隔 5–600 秒、次数 1–10，越界返回 400），返回保存后的设置。开启时渲染为 `defaults.conf` 中的 `Host *` 块，位于 `~/.ssh/config` 之后，因此用户为某主机写的 `ServerAlive*` 优先。只影响之后启动的 ssh 进程；已建立的共享 master 保持原参数，直到断开或空闲关闭。保存时（主机、Agent access 与 FTP 站点的修改相同）先写 `store.json`，再写 `hosts.conf`、`defaults.conf` 与 `-F` 包装配置；任一写入失败则恢复原有文件与内存中的状态并返回错误，因此 `GET` 与 ssh 实际读取的配置一致。 |
| `POST /v2/ftp/sites`、`PUT` / `DELETE /v2/ftp/sites/{id}` | FTP 站点 `{ name, protocol: "ftp"|"ftps", host, port?（默认 21）, user?, initialDirectory? }`，不含密码。 |

### 端口转发

手动端口转发（`ssh -L` / `-R` / `-D`）在**运行 todex-agentd 的机器**上运行。配置保存在 `store.json` 顶层可选字段 `forwards`（缺失为空；旧版本 daemon 忽略该字段照常启动，加载时逐条校验，无效条目只记警告并丢弃），运行状态只在内存中：转发从不自动启动（daemon 重启后也一样），daemon 退出时全部停止。

- 每个运行中的转发是一个独立的 ssh 进程：`-F` 包装配置、`-N`、`ExitOnForwardFailure=yes`、`ControlMaster=no`、`ControlPath=none`（既不复用也不成为共享 master，停止转发不影响其他会话）、`StrictHostKeyChecking=yes`、`ConnectTimeout=15`；无密码时 BatchMode，带密码时只应答一次密码提示（askpass 一次性密码文件，见「主机清单」；ProxyJump/ProxyCommand 主机返回 400）。ssh 运行在独立进程组中，停止时连同 ProxyCommand 一起结束。
- 就绪判定：ssh 以 `LogLevel=DEBUG1` 运行（日志不含密码），本地/动态转发以监听成功的日志行为准（日志措辞不同的 OpenSSH 版本在登录 2 秒后探测本地监听端口），远程转发以服务器确认（`remote forward success`）为准；20 秒内已登录但没有确认时也按 `running` 处理。启动前先检查本地监听地址是否已被占用，被占用时不登录直接失败。之后 ssh 进程退出（网络断开、服务器关闭连接等）时状态变为 `failed` 并带原因。
- `SshForwardInput`：`{ alias, kind: "local"|"remote"|"dynamic", bindAddress?, bindPort, targetHost?, targetPort?, label? }`。`alias` 必须在主机清单中（无效别名 400，不存在 404）。`local`/`dynamic` 的 `bindAddress` **只能**是 `127.0.0.1`、`::1` 或 `localhost`（默认 `127.0.0.1`，保存为显式值），不会监听其他网卡；`remote` 的 `bindAddress` 是服务器上的地址（主机名/IP 字符或 `*`），省略时由服务器决定（通常是服务器的回环地址，受其 `GatewayPorts` 约束）。`bindPort`、`targetPort` 为 1–65535；`local`/`remote` 必须提供 `targetHost`（字母、数字与 `.-_:%`，不以 `-` 开头，≤ 253 字符；IPv6 地址自动加方括号）与 `targetPort`，`dynamic` 不得提供二者；`label` ≤ 80 个字符且不含控制字符。校验失败返回 400，最多保存 256 条。
- `SshForward` = 输入字段 + `id`、`status`（`stopped`/`starting`/`running`/`failed`）、`failure?`（同连接测试的 `failure`）、`error?`（ssh 错误输出摘要，不含密码）、`startedAt?`（RFC 3339）。
- 主机的 ssh_config（包括 `~/.ssh/config`）若为该别名设置了 `LocalForward`、`RemoteForward` 或 `DynamicForward`，ssh 会把它们一并加到转发进程里（`ClearAllForwardings` 又会清掉本转发自身），因此启动前以 `ssh -G` 检查，发现时返回 200、`status: "failed"`、`failure: "other"`，`error` 说明原因并建议删除这些配置或改用内部 IP 池。
- 运行中的转发与 IP 池 master 的 pid（Linux 上连同进程启动时间）记录在 `$DATA_DIR/ssh/children.json`（0600）；daemon 被强杀后，下次启动时只结束仍在运行、且命令行是带 TodeX `-F` 包装配置与转发/master 参数的 ssh 进程，其他进程不受影响。
- 回环监听不是用户隔离：同一台机器上的其他本地用户也能连接 `127.0.0.1`/`::1` 上的转发端口（SOCKS 代理同理）。

| 接口 | 说明 |
| --- | --- |
| `GET /v2/ssh/forwards` | `{ forwards: SshForward[] }`。 |
| `POST /v2/ssh/forwards` | 请求体 `SshForwardInput`，返回 `{ forward }`（`status: "stopped"`）。 |
| `PUT /v2/ssh/forwards/{id}` | 请求体 `SshForwardInput`，返回 `{ forward }`；`starting`/`running` 时返回 409，需先停止。 |
| `DELETE /v2/ssh/forwards/{id}` | 先停止再删除，返回 `{ deleted: true }`。 |
| `POST /v2/ssh/forwards/{id}/start` | 可选请求体 `{ password? }`（解析规则同 `/hosts/{alias}/test`），等待就绪或失败（最多约 20 秒）后返回 `{ forward }`。登录失败、主机密钥问题、端口被占用、服务器拒绝远程转发等运行时失败返回 200，`status: "failed"` 并带 `failure`、`error`。已有 32 个转发在运行，或另一个运行中的转发（或 TodeX 自身的监听端口）占用了相同的本地地址与端口时返回 409。已在运行时直接返回当前状态。 |
| `POST /v2/ssh/forwards/{id}/stop` | 停止并返回 `{ forward }`。 |

删除自管主机时会先停止并删除它的转发；主机改名时转发跟随新别名。

### 内部 IP 池（按需转发）

给主机分配一个独立的回环地址（如 `127.77.0.2`），本机程序连接 `该地址:端口` 即被转发到该主机上的 `127.0.0.1:端口`，无需逐个配置端口转发。仅 Unix（Linux、macOS）支持；其他平台 `supported: false`，启用时返回 501。配置保存在 `store.json` 顶层可选字段 `ipPool`（缺失取默认值；读取时宽松处理：无效的 `cidr` 回退默认值，`idleMinutes` 收敛到 1–1440，无效或冲突的分配丢弃并记警告；旧版本 daemon 忽略该字段），运行状态只在内存中。

- `SshIpPoolConfig`：`{ enabled, cidr, idleMinutes }`，默认 `{ false, "127.77.0.0/24", 15 }`。`cidr` 必须在 `127.0.0.0/8` 内、前缀长度 16–30、主机位为 0，且不得包含 `127.0.0.1`；网络地址与广播地址不可分配。`idleMinutes` 为 1–1440。
- 端口模式：`auto` 使用内置的常用端口表（`autoPorts`：`80, 443, 1433, 3000-3010, 3306, 4000, 4200, 5000-5010, 5173, 5432, 5601, 6379, 8000-8100, 8443, 8888, 9000-9100, 9200, 11434, 15672, 27017`，共 240 个）；`manual` 使用 `ports`（`"8080"` 或 `"7000-7010"`，去重后保存规范形式，每个地址最多 4096 个端口，不能为空）。`auto` 模式下请求中的 `ports` 被忽略，保存为 `[]`。所有分配的监听总数上限为 200 000 与「打开文件数硬限制 − 4096」中的较小值，超出返回 409。
- 监听：启用后，每个分配在 `ip:端口` 上为端口集合中的每个端口监听（只在回环地址上；daemon 重启后若已启用会自动恢复）。端口已被占用或无权限（例如 Linux 上未放开的 1024 以下端口）时跳过并记入 `skippedPorts`（最多列出 100 个，`skippedPortCount` 为总数），修改端口或重新应用配置时重试。地址不可用（`EADDRNOTAVAIL`）时停止该地址的监听并报告 `bindError: { kind: "addressUnavailable", message }`：**macOS** 只有 `127.0.0.1` 默认可用，需先执行 `sudo ifconfig lo0 alias <ip>`（重启后失效）；Linux 上整个 `127.0.0.0/8` 默认可用。其他监听错误为 `kind: "other"`。修改端口、模式、启用状态或 `cidr` 时增量增删监听，已建立的连接不受影响。池启用且有分配时，打开文件数的软限制按需提高到「监听总数 + 4096」（不超过硬限制，从不降低）。
- 控制套接字目录：默认 `$DATA_DIR/ssh/pool`；路径加上套接字名（`/<ip>`，以及 OpenSSH 先绑定的 `<路径>.<16 位随机字符>` 临时名）超出 unix 套接字路径上限时，改用 `$XDG_RUNTIME_DIR/tdxp-<哈希>`（macOS 为每用户的 `$TMPDIR`），都不可用时在 `/tmp` 下新建随机名目录（`mkdtemp`）。所用目录必须是真实目录（不是符号链接）、属于运行 daemon 的用户且权限为 0700（本用户的目录权限过宽时收紧为 0700），否则不启用池：分配报告 `bindError: { kind: "other" }` 说明原因，`connect` 返回 `state: "failed"`。目录在池首次运行时（启用且有分配，包括 daemon 启动时恢复监听）才选择和创建；此时对其中残留的控制套接字先执行 `ssh -O exit`（结束被强杀的 daemon 留下的 master），再删除。
- 连接：第一个连接到来时为该分配启动一个专用 master（`ssh -N -o ControlMaster=yes -o ControlPersist=no -o ControlPath=<控制套接字> -o ClearAllForwardings=yes`，BatchMode、严格主机密钥校验），master 启动期间到达的连接排队等待同一个 master；之后每个连接运行一个 `ssh -S <控制套接字> -o ControlMaster=no -W 127.0.0.1:<端口> -- <alias>` 复用该登录（找不到 master 时不会另行登录）。同时运行的 `-W` 进程最多 512 个，超出时新连接立即关闭。`-W` 进程退出后连接在短暂排空后关闭（即使客户端仍保持连接）；任一方向出现 I/O 错误时立即关闭。`activeConnections` 为 0 持续 `idleMinutes` 后以 `ssh -O exit` 关闭 master，状态回到 `idle`；下一个连接重新登录。修改 `idleMinutes` 立即作用于已连接的 master。
- 密码主机（`authMethod: "password"`）从不自动登录：没有 master 时新连接直接关闭，状态为 `needsLogin`，需要调用 `connect` 并提供密码（ProxyJump/ProxyCommand 主机不支持密码，返回 400）。密钥主机自动登录失败时：认证失败为 `needsLogin`，其他为 `failed`，15 秒内的新连接直接关闭而不重复尝试。
- `SshIpAssignment`：`{ ip, alias, portMode, ports, listeningPorts, skippedPorts, skippedPortCount, bindError?, state, activeConnections, connectedAt?, lastUsedAt?, failure?, error? }`，`state` 为 `disabled`（池未启用）、`idle`、`connecting`、`connected`、`needsLogin`、`failed`。
- `SshIpPool` = `SshIpPoolConfig` + `supported`、`unsupportedReason?`、`autoPorts`、`manualPortLimit`（4096）、`assignments`。
- 信任模型：池启用期间，本机上的任何进程（包括其他本地用户的进程）都能连接分配的地址与端口，从而触发对密钥主机的自动登录，并访问远程主机 `127.0.0.1` 上列出的端口。这与 `ssh -L` 的回环监听相同，但范围更大：`auto` 模式一次开放全部 240 个常用端口。只为信任的主机启用；密码主机只在显式 `connect` 后可达，`disconnect` 或空闲关闭后重新需要登录。

| 接口 | 说明 |
| --- | --- |
| `GET /v2/ssh/ip-pool` | 返回 `SshIpPool`。 |
| `PUT /v2/ssh/ip-pool` | 请求体为完整的 `SshIpPoolConfig`，返回 `SshIpPool`。新的 `cidr` 不再包含某个已有分配（或使其成为网络/广播地址）时返回 409；不支持的平台启用时返回 501。停用时关闭所有监听与 master。 |
| `POST /v2/ssh/ip-pool/assignments` | 请求体 `{ ip?, alias, portMode, ports? }`，返回 `{ assignment }`。省略 `ip` 时取 `cidr` 中第一个空闲地址；每台主机一个地址、每个地址一台主机，重复返回 409；`alias` 必须在主机清单中。 |
| `PUT /v2/ssh/ip-pool/assignments/{ip}` | 请求体 `{ portMode, ports? }`，返回 `{ assignment }`；监听增量增删，已有连接继续。 |
| `DELETE /v2/ssh/ip-pool/assignments/{ip}` | 关闭监听与 master 后删除，返回 `{ deleted: true }`。 |
| `POST /v2/ssh/ip-pool/assignments/{ip}/connect` | 可选请求体 `{ password? }`（解析规则同 `/hosts/{alias}/test`），立即建立专用 master（密码主机必须如此），返回 `{ assignment }`；登录失败返回 200，`state` 为 `needsLogin`/`failed` 并带 `failure`、`error`。池未启用时返回 409。 |
| `POST /v2/ssh/ip-pool/assignments/{ip}/disconnect` | 关闭 master（监听保留），返回 `{ assignment }`。 |

不存在的或格式错误的 `{ip}` 返回 404。删除自管主机时先关闭并删除它的分配；主机改名时分配跟随新别名，并关闭旧 master。

### SSH 密钥

管理后端用户 `~/.ssh` 下的密钥。密钥解析、生成与加密均在进程内完成（不调用 `ssh-keygen`，口令不会出现在进程参数中）。

- `GET /v2/ssh/keys` → `{ sshDirectory, agentAvailable, keys: SshKey[] }`
  - 按内容识别私钥（OpenSSH / PEM），`<name>` 与 `<name>.pub` 配对；跳过 `config`、`known_hosts*`、`authorized_keys*`、目录、socket、超过 64 KiB 的文件及指向 `~/.ssh` 之外的符号链接。
  - `SshKey`：`name`、`privateKeyPath?`、`publicKeyPath?`、`algorithm`（如 `ssh-ed25519`）、`bits?`、`fingerprint?`（`SHA256:…`；仅无 `.pub` 的旧式 PEM 私钥缺省）、`comment?`、`encrypted?`、`loadedInAgent`、`publicKey?`（完整 OpenSSH 公钥行）、`usedBy`（`IdentityFile` 指向该密钥的主机别名）。
  - `agentAvailable`：Unix 上未设置 `SSH_AUTH_SOCK` 或 `ssh-add -L` 失败时为 `false`。
- `POST /v2/ssh/keys/import` `{ name, privateKey, publicKey?, passphrase? }` → `{ key }`。由系统 `ssh-keygen -y` 校验（系统 ssh 能加载的格式都可导入，含 OpenSSH 与传统 PEM），私钥按提交内容（CRLF 转为 LF）写入，公钥取 `ssh-keygen` 推导结果；`publicKey` 若提供必须匹配。加密私钥需提供 `passphrase`，或提供 `publicKey`（以 `ssh-keygen -l` 指纹比对）。`passphrase` 经 askpass 一次性 owner-only 文件传给 `ssh-keygen`，不进入 argv/环境变量，也不会保存；系统找不到 `ssh-keygen` 时返回 `UNSUPPORTED`。
- `POST /v2/ssh/keys/generate` `{ name, algorithm?, comment?, passphrase? }` → `{ key }`。`algorithm`：`ed25519`（默认）、`rsa`（4096 位）、`ecdsa`（P-256）。
- 名称为 `[A-Za-z0-9._-]{1,64}`，不得以 `.`/`-` 开头、以 `.pub` 结尾或与 OpenSSH 文件同名。写入 `~/.ssh/<name>`（0600）与 `<name>.pub`（0644），`~/.ssh` 不存在时以 0700 创建；任一文件已存在返回 409，从不覆盖。请求体上限 64 KiB。

### SSH 终端

`terminal.start`（见「统一 WebSocket 命令面」）可带 `ssh: { host }`：后端在 PTY 中运行 `ssh -tt <alias>`，工作目录为用户主目录，环境变量清空后只保留白名单（不继承 `TODEX_AGENTD_*`）。此时忽略 `cwd` 与 `workspaceId`，也不做 workspace 根目录检查，只要求别名存在于主机清单。主机密钥确认与密码提示直接在终端里完成；也可带 `ssh: { host, password }`，由 askpass 应答一次密码提示（经一次性密码文件，见「主机清单」；不写日志），此时未知主机密钥直接拒绝而不询问（客户端可不带密码重试以确认指纹）；ProxyJump/ProxyCommand 主机忽略该密码，照常在终端中提示；登录后的共享连接可被 SFTP 与 Agent 命令复用。`terminal.started` 事件额外带 `ssh: { host }`，其余 `terminal.*` 命令与事件不变。

`ssh: { host: "Local" }`（不区分大小写）是本机终端：后端运行 `ssh -tt localhost`，以 daemon 的用户登录 daemon 所在机器，需要本机 sshd（macOS 需开启「远程登录」）。它不要求主机清单中存在该别名，清单中同名的主机在终端里被它遮蔽（与 Agent 的 `Local` 一致）；`password` 被忽略，在终端中登录；其余同上，`terminal.started` 带 `ssh: { host: "Local" }`。仅 Linux/macOS daemon 支持，其他平台返回 `INVALID_REQUEST`。

### 远程文件（SFTP / FTP）

内存会话：空闲 5 分钟自动关闭，最多 16 个，daemon 重启即失效；密码只用于本次连接，从不保存。

- `GET /v2/remote/connections` → `{ connections: RemoteConnection[] }`（`id`、`kind`、`host?`、`siteId?`、`label`、`homeDirectory`、`openedAt`、`lastUsedAt`）。
- `POST /v2/remote/connections`，`{ kind: "sftp", host, password? }` 或 `{ kind: "ftp", siteId, password? }` → `{ connection }`。SFTP 走系统 ssh（`ssh -s … sftp`，复用 TodeX 的 `-F` 配置与共享连接）；带密码时通过一次性的 `SSH_ASKPASS` 辅助进程（`todex-agentd ssh-askpass`，只回答密码/口令提示，经一次性密码文件，见「主机清单」）登录，且不会成为长期 master。FTP 使用被动模式，`ftps` 为显式 TLS（系统 webpki 根证书）。
- `DELETE /v2/remote/connections/{id}` → `{ closed: true }`。
- `GET …/{id}/entries?path=<绝对路径>`（省略时为 home）→ `{ path, parent?, entries, truncated }`，目录在前，最多 5000 条；条目含 `name`、`path`、`kind`（`file`/`directory`/`symlink`）、`sizeBytes?`、`modifiedAt?`、`permissions?`。
- `GET …/{id}/file?path=` 与 `/v2/workspace/file` 返回结构相同（文本 1 MiB / 图片 8 MiB）；`PUT …/{id}/file` `{ path, text, expectedText }`，内容不一致返回 409。SFTP 以临时文件加 rename 保存并保留权限，FTP 原地覆盖。
- `POST …/{id}/mkdir` `{ path }`、`…/rename` `{ from, to }`（目标存在返回 409）、`…/delete` `{ path }`（仅文件、符号链接或空目录，从不递归）→ `{ ok: true }`。
- 上传：`PUT …/{id}/upload?path=&offset=&overwrite=`，body 为原始字节（`application/octet-stream`，参与设备签名），每块 ≤ 8 MiB，单文件 ≤ 100 MiB；`offset=0` 新建（已存在且未带 `overwrite=true` 返回 409），其余块的 `offset` 必须等于远端当前大小，返回 `{ sizeBytes }`。中断的上传会留下部分文件。
- 下载：`GET …/{id}/download?path=` 流式返回原始字节（带 `Content-Length` 与 `Content-Disposition`），≤ 100 MiB。
- 路径必须是绝对 POSIX 路径，`.`/`..` 按字面规范化，禁止 NUL 与换行。

### Agent SSH 工具（MCP）

只要至少一台 SSH 主机开启了 Agent access，或开启了 Agent 本机终端（`GET /v2/ssh/agent-local`），TodeX 就会在会话启动时为 Codex、Claude Code、ACP 系列（OpenCode/Devin/Grok/DeepSeek Harness/omp/自定义 ACP）、Pi 与 Antigravity（全局静态条目，见上文）注入名为 `todex_ssh` 的 stdio MCP 服务器（`todex-agentd agent-mcp-bridge`，旧名 `ssh-mcp-bridge` 仍可用；环境变量 `TODEX_AGENT_MCP_URL`、`TODEX_AGENT_MCP_TOKEN`，桥也接受旧名 `TODEX_SSH_MCP_*`）；Claude Code 的配置写入 `$DATA_DIR/agent-mcp/` 下的 0600 文件，令牌不出现在命令行。同一会话注入多个 TodeX MCP 服务器时，它们共用一个会话令牌，各自走自己的端点；Codex 每个服务器一个 `mcp_servers.<name>` 覆盖，Claude Code 写入同一个配置文件（每个服务器带 `timeout`）并以逗号分隔列在 `--allowedTools`，ACP 列在同一个 `mcpServers` 数组。Pi 通过 `-e $DATA_DIR/agent-mcp/todex-pi-mcp.mjs` 扩展注入：扩展从环境变量 `TODEX_AGENT_MCP_PI_SERVERS`（启动后即从 `process.env` 删除）读取启用的服务器，为每个服务器启动同一个桥，并把其工具直接注册为 Pi 工具（不依赖 Pi 自身的 MCP 适配器）。没有开启的主机时，Provider 启动参数与以前完全一致；旧 codex_gateway 不注入。已运行的 Codex/ACP 进程要到下一次会话启动才会看到新开启的工具；关闭 Agent access 立即生效。

- 端点：`/internal/agent-mcp/ssh`（MCP Streamable HTTP）。不走设备签名，只接受回环地址、不带 `Origin` 头、携带会话级 `Authorization: Bearer <token>` 的请求；非回环或带 `Origin` 返回 403，令牌缺失或无效返回 401。令牌只存在内存，删除会话或重启 daemon 后失效。Claude Code 的配置文件在该会话再没有打开的 MCP 会话（桥正常退出时发出 `DELETE`）且没有更新的启动等待读取时删除；桥被强杀时留到会话删除。按会话的令牌与授权只用于区分会话、避免 Agent 未经询问就行动，**不是**同一 OS 用户下的安全边界：以同一用户运行的程序可以从 Provider 进程环境或配置文件读到令牌。
- 协议细节（`todex_ssh`、`todex_desktop` 与 `todex_agents` 相同）：调用不存在的工具返回 JSON-RPC 错误 `-32602`（`Unknown tool: <name>`）；参数无效、功能关闭、拒绝、超时等都是 `isError: true` 的工具结果（文本以 `CODE: ` 开头）。服务器不声明 prompts/resources；客户端仍调用 `prompts/list`、`resources/list`、`resources/templates/list` 时返回带 `ttlMs: 0`、`cacheScope: "private"` 的空列表。
- 权限模式：有副作用的工具（`ssh_exec`、`browser_open`、`browser_navigate`、`browser_act`、`computer_act`）跟随会话当前轮次的有效权限模式（每轮 `turn.started` 的 `effectivePermissions`，在调用时读取；轮次成功开始后才生效，轮次结束——完成、失败或取消——后清除，此时没有运行中的轮次，按 `ask` 处理）：`workMode: "plan"` 时一律拒绝（`PLAN_MODE: …`，只读工具照常可用）；`permissionMode: "ask"` 或 `"accept-edits"`（它只放行 Provider 自己的文件编辑，TodeX 工具不属于编辑）时每次调用发出与 Provider 工具审批相同的 `permission.requested { kind: "tool", details: { tool_name: "mcp__<server>__<tool>", server, tool, summary, input } }`（任意已配对设备可答；`input` 不含输入的文字；`ssh_exec` 的 `input` 带 `stdinBytes` 与 `stdinPreview`——stdin 前 4 KiB 的预览，按下文隐私过滤的「私钥」「密码与令牌」与「代理节点凭据」三类脱敏（不受 Agent 输出过滤开关影响），超出时以 `… (truncated, N bytes)` 标明；`summary` 中的命令最多 200 个字符，被截断时以 `… (truncated, N chars)` 标明总长），选项 `allow_once`、`allow_always`（本会话内该工具不再询问）、`reject_once`；`full-access` 不额外审批；`auto` 同样不额外审批，唯一例外是在 TodeX 主机上执行的 `ssh_exec`（`host: "Local"`）：它不受 Provider 自身沙箱约束，因此按上述 `ask` 规则逐次审批（含长度上限，`allow_always` 仍可在本会话内免询问）（Antigravity 的 `auto` turn 在账号不提供 `agy` 的 Auto 时、Claude Code 的 `auto` turn 在其换成 `default` 或 `acceptEdits` 时改按 `ask` 审批，以 `auto_mode_unavailable` 事件为准，见上文）。只支持 `ask` 的 Provider（Grok Build、自定义 ACP）因此总是逐次审批；Grok Build 在轮次中批准计划后，本轮剩余部分仍按计划模式处理，下一轮才放开。
- 防刷屏：任何 TodeX 工具的提示（首次授权、按应用批准、敏感确认、ask 模式审批）被拒绝或超时后，同一会话的同一提示 30 秒内不再弹出，直接返回 `DECLINED`（说明不会再问）。工具调用被取消时等待立即结束；主机原生对话框无法撤回，会留在屏幕上，下一次相同提示等待这同一个对话框而不是再弹一个，若在无人等待时被回答，60 秒内的下一次相同提示直接采用该回答。只放行一次的确认（敏感操作、无法识别目标应用的 Computer Use 操作）例外：提示按动作内容（工具与参数的摘要）区分，30 秒退避也只对同一动作生效；它们的回答从不与其他调用共享，也不留给之后的调用——同一动作的旧对话框还在屏幕上时，新调用先等它被回答，再弹出自己的对话框。
- `ssh_list_hosts {}` → `{ hosts: [{ alias, hostName?, user?, port?, local?, description? }] }`，只列出开启了 Agent access 的主机；开启本机终端时第一项为 `{ alias: "Local", local: true, description }`。
- 本机终端 `Local`：`ssh_exec` 的 `host` 为 `Local`（不区分大小写，事件中统一记为 `Local`）时，在 daemon 所在机器上以用户的登录 shell 运行命令（Unix `$SHELL -l -c`，未设置时 `/bin/sh`；Windows `%COMSPEC% /D /S /C`），环境只保留与其他外部命令相同的白名单变量（不含 `TODEX_*`），由登录 shell 补全。工作目录默认是会话的工作区，相对 `cwd` 按工作区解析，绝对路径直接使用；`ssh.exec.started` 的 `cwd` 记录实际目录。权限模式、审批、超时、输出上限、并发与隐私过滤都与 SSH 主机相同；不经过 ssh，也没有登录等待，任何退出码（含 255）都是正常结果，只有被信号终止、超时或无法启动时返回 `isError: true`。开启时，别名与 `Local` 不区分大小写相同的 SSH 主机不再对 Agent 可用。关闭立即生效（调用返回 `isError: true`）；开启要到下一次会话启动才注入。工具说明只客观描述 `Local`，不引导 Agent 优先使用，是否使用由用户在提示中决定。
- `ssh_exec { host, command, cwd?, stdin?, timeoutSec? }` → `{ stdout, stderr, exitCode, truncated, durationMs, redacted?, note? }`（输出经隐私过滤，见下文）。审批只取决于上述权限模式（`auto`/`full-access` 下无需审批）；`ask` 模式下 `command` 或 `stdin` 超过 4096 bytes 时不发起审批、直接返回 `isError: true`，提示 Agent 先把脚本以文件形式上传到主机（例如请用户复制过去）再用短命令运行（审批与事件只记录命令与 stdin 各前 4096 bytes，更长的内容无法完整审阅）；以 BatchMode、严格主机密钥校验运行；`cwd` 按 POSIX shell 引用后 `cd`；默认超时 60 秒，最长 600 秒；stdout/stderr 各最多 256 KiB，超出截断并置 `truncated: true`；同一主机最多 4 条并发命令。非零退出码是正常结果；ssh 失败（退出码 255）、超时或无法启动时返回 `isError: true`，带 `failure` 与处理提示。ACP 类 Agent 若自身处于“询问”权限模式，仍可能针对 MCP 工具弹出其原生权限请求。
- 隐私过滤：Agent 收到的 `stdout`/`stderr`（含 ssh 失败结果中的输出）按 `GET /v2/ssh/agent-output-filter` 的设置脱敏后才返回，命中内容替换为 `[REDACTED:<类型>]`；有替换时结果另带 `redacted: true` 与说明占位符含义的 `note`。`ssh.exec.output` 事件（用户的实时视图）保留原文，`ssh.exec.completed` 带 `redactions: { <类别>: 次数 }`（仅在有替换时出现，只含次数大于 0 的类别）与 `redactedValues: [{ stream: "stdout"|"stderr", category, label, text }]`（被隐藏的原文，供用户的输出视图遮盖显示；每个流内去重，最多 256 个、合计 64 KiB 文本，超出的不列出；这些原文本已在同一会话的 `ssh.exec.output` 事件中，返回给 Agent 的结果不含此字段）。这是尽力而为的过滤，不是安全边界：Agent 可以用编码、分段等方式读到原文。类别：
  - `privateKey`：PEM/OpenSSH/PGP 私钥块（保留 BEGIN/END 行，只替换内容；被截断、缺少 END 行的块一直替换到不再像密钥内容的行为止；也识别 JSON 中以 `\n` 转义的块）与 age 私钥 → `[REDACTED:private_key]`、`[REDACTED:age-secret-key]`。
  - `publicKey`：`ssh-ed25519`/`ssh-rsa`/`ecdsa-sha2-*`/`sk-*`（含证书）行的密钥数据（保留算法与注释，`known_hosts`、`authorized_keys` 同样适用）、`PUBLIC KEY` 与 `CERTIFICATE` 块、RFC 4716 块 → `[REDACTED:public_key]`。
  - `credential`：内嵌的 gitleaks v8.30.1 默认规则（`src/privacy_filter/gitleaks.toml`，MIT；关键词预筛、正则、熵阈值、全局与规则白名单，与 gitleaks 一致；只按路径匹配的规则不适用，`kubernetes-secret-yaml` 除外）→ `[REDACTED:<规则 id>]`，只替换秘密本身、保留键名；另加键名像密钥的赋值（`KEY=value`、`key: value`、JSON、`export`，以及「密码/口令/密钥/私钥/令牌」；也包括整键为 `auth`、`authentication`、`basic-auth`、`htpasswd` 与含 `psk`/`preshared-key` 的键）→ `[REDACTED:credential]`，跳过空值、占位符（`${VAR}`、`<…>`、`****` 等）、7 位以内的纯数字、认证方式名（`basic`、`md5` 等）以及描述密钥的元数据键（`*_file`、`*_type(s)`、`max_tokens` 等）；这类键下的列表同样处理：流式列表 `key: [a, b]` 与 YAML 块列表（键行无值，其下缩进不小于键的 `- item` 行，例如 mihomo 的 `authentication:` 下的 `- user:pass`；列表项为映射时不处理）；URL 查询中的 `token`、`access_token`、`key`、`sig`、`signature`、`auth`、`secret`、`password`、`X-Amz-Signature` 等参数值（到下一个 `&` 为止）→ `[REDACTED:url-secret]`；任意位置解码后为可打印 `user:pass` 的 `Basic <base64>` → `[REDACTED:basic-auth]`；URL 中的密码（`scheme://user:pass@`）→ `[REDACTED:url-password]`；`Bearer <令牌>` → `[REDACTED:bearer-token]`；`xai-`/`sk-` 前缀密钥 → `[REDACTED:api-key]`；crypt(3) 密码哈希（`/etc/shadow`）→ `[REDACTED:password-hash]`。
  - `bankCard`：13–19 位卡号（连续或每 4 位以空格/短横分组，Amex 为 4-6-5），需通过 Luhn 校验且落在发卡机构号段与长度内；未通过 Luhn 的 62 开头银联卡号只在前 40 字节内出现「卡号/银行卡/账号/card/account」等词时才算 → `[REDACTED:bank_card ****<后 4 位>]`。IBAN 需国家长度正确并通过 MOD 97 → `[REDACTED:iban]`。前后紧邻字母、数字或小数点的数字串（哈希、浮点数、更长的编号）以及以空格相连的数字列不算。
  - `idCard`：常用国家/地区的身份证件号，标签标明种类。校验强或格式独特的单独即可命中：中国居民身份证（18 位，出生日期合法且校验码正确，`cn_id`）、香港身份证（`A123456(3)`，`hk_id`）、台湾身份证（`tw_id`）、美国 SSN（`123-45-6789` 形式且区段合法，`us_ssn`）、英国 National Insurance number（`uk_nino`）、新加坡 NRIC/FIN（`sg_nric`）、韩国居民登录号（`900101-1234567`，日期合法，`kr_rrn`）、西班牙 DNI/NIE（`es_dni`）、带格式的巴西 CPF（`529.982.247-25`，`br_cpf`）。随机数字也容易通过校验的，须在号码前 40 字节内出现对应词（ASCII 词按整词、不区分大小写）才命中：澳大利亚 TFN（`tfn`/`tax file`，`au_tfn`）与 Medicare（`medicare`，`au_medicare`）、加拿大 SIN（`sin`/`social insurance`，`ca_sin`）、印度 Aadhaar（Verhoeff，`in_aadhaar`）与 PAN（`in_pan`）、日本 My Number（`マイナンバー`/`個人番号`，`jp_my_number`）、无短横的 SSN、无格式的 CPF、不带括号的香港身份证，以及护照号（`passport`/`护照`/`パスポート` 等，`passport`）→ `[REDACTED:<种类>]`。
  - `proxyCredential`：代理/VPN 节点凭据。键名（区分大小写，只认 `:` 赋值，因此 fstab/blkid 的 `UUID=`、nmcli 的 `connection.uuid` 不算）`uuid` → `[REDACTED:uuid]`，`short-id`/`shortId`/`shortIds` 等 → `[REDACTED:short-id]`，`auth-str`、`decryption`（服务端 VLESS Encryption 私有材料；`none` 不算）→ `[REDACTED:proxy-credential]`，支持标量、流式列表与块列表；分享链接 `vless://`、`vmess://`、`trojan://`、`ss://`、`ssr://`、`hysteria2://`/`hy2://`、`tuic://`、`anytls://`、`socks5://` 等的用户信息（`@` 之前）、整段 base64 的链接主体，以及查询中的 `sid`、`password`、`obfs-password`、`auth`、`uuid`、`psk` → `[REDACTED:share-link]`（主机、端口、`pbk` 与节点名保留）。输出中出现代理配置标记（行首 `proxies:`/`proxy-providers:`/`proxy-groups:`、JSON `"outbounds"`/`"inbounds"`、或 `vless`/`vmess`/`trojan`/`hysteria`/`tuic`/`shadowsocks` 字样）时，另处理 Xray/V2Ray 的 `"id": "<UUID>"` → `[REDACTED:uuid]`，以及 http(s)/ws(s)/ftp URL 路径与查询中像令牌的片段（16 位以上且含字母与数字的十六进制，或 20 位以上同时含大小写与数字的高熵 base64url）→ `[REDACTED:subscription-token]`。公钥（`public-key`、客户端 `encryption`）不处理。
  - 以下为激进选项，默认关闭，只按形态匹配，会误伤非秘密内容：
    - `urlToken`：同上的 URL 令牌片段，但不要求代理配置上下文（会隐藏链接中的提交哈希、摘要）→ `[REDACTED:url-token]`。
    - `highEntropy`：独立的 40 位以上十六进制串（熵 ≥ 3.0），或 64 位以上同时含大小写与数字、熵 ≥ 4.5 的 base64/base64url 串 → `[REDACTED:high-entropy]`（会隐藏 git 哈希、sha256 校验值）。
    - `uuid`：任意 8-4-4-4-12 UUID → `[REDACTED:uuid]`。
    - `email`：电子邮件地址 → `[REDACTED:email]`。
    - `phone`：电话号码 → `[REDACTED:phone]`。包括：任何 `+<国家码>` 国际格式（共 8–15 位数字，可用空格、短横、点与括号分组）；中国大陆手机号（11 位 `1[3-9]` 开头，可带 `+86`，可按 3-4-4 分组）；美国/加拿大（`(415) 555-2671`、`415-555-2671`、`1-800-…`，区号不为 N11）；澳大利亚（`04xx xxx xxx` 手机、`(02) xxxx xxxx` 座机、1300/1800）；英国（`07xxx xxxxxx`、`020 xxxx xxxx` 等）；法国（`06 12 34 56 78`）；日本、韩国、台湾带短横的号码（`090-1234-5678`、`010-1234-5678`、`0912-345-678`）；其他国家紧跟 `phone`/`tel`/`mobile`/`电话`/`手机`/`電話` 等词之后的 7–15 位号码。
    - `publicIp`：公网 IPv4（排除私有、回环、链路本地、100.64/10、文档、基准测试、组播与保留段）与全球单播 IPv6（排除 2001:db8::/32）→ `[REDACTED:public-ip]`。
  - `customKeys`：用户自定义的键名（整键匹配、不区分大小写），其值按 `credential` 的赋值与列表规则处理但不做长度、纯数字限制 → `[REDACTED:custom-key]`，计入 `credential` 次数；与类别开关无关，只要过滤开启即生效。
- 登录等待：`ssh_exec` 以 BatchMode 运行，复用共享 master 或使用密钥/ssh-agent。ssh 认证失败（`authenticationFailed`）且主机可以用密码登录（支持连接共享、不经 ProxyJump/ProxyCommand）时，不直接失败，而是向本会话发出 `ssh.login.requested` 并等待最多 120 秒：客户端经 `POST /v2/ssh/hosts/{alias}/login` 提供密码后建立新的共享 master，命令在其上重新运行一次（第一次的 stderr 仍会出现在 `ssh.exec.output` 中）；等待期间若其他途径（如 TodeX 终端）已建立 master（每 2 秒以 `ssh -O check` 检查），也直接继续。同一会话对同一主机同时只有一个请求，并发命令共同等待；被拒绝或无人应答后，本会话 30 秒内不再为该主机询问，直接返回失败。未能登录时返回原来的 ssh 失败结果，`hint` 说明原因（拒绝、超时、刚被拒绝）。Provider 的单次工具超时相应增加 120 秒。
- 会话事件（以 `execId` 关联同一次调用，并发调用互不混淆）：
  - `ssh.exec.started { execId, host, command, cwd?, approvedBy?, turnId? }`：`approvedBy` 是 ask 模式下批准这次调用的设备 id。
  - `ssh.exec.output { execId, stream: "stdout"|"stderr", data, turnId? }`：实时输出，按 100 ms 或 16 KiB 合并为一条事件，UTF-8 多字节字符不会被拆开；每次调用每个流最多记录 64 KiB（Agent 收到的结果仍最多 256 KiB），超出部分不再记录。
  - `ssh.exec.completed { execId, host, exitCode?, durationMs, failure?, truncated, outputTruncated, redactions?, redactedValues?, turnId? }`：`truncated` 指返回给 Agent 的结果被截断，`outputTruncated` 指事件中记录的输出被截断，`redactions`、`redactedValues` 见上文「隐私过滤」。
  - `ssh.login.requested { requestId, host, execId, expiresAt, turnId? }`：Agent 命令在等待登录；`expiresAt` 为 RFC 3339 时间，过期或已有对应的 `ssh.login.resolved` 后客户端不应再提示。
  - `ssh.login.resolved { requestId, host, outcome, remembered, turnId? }`：`outcome` 为 `connected`、`declined`、`timedOut` 或 `cancelled`（等待的工具调用被取消）；`remembered` 表示登录使用了客户端保存的密码。
  - 桌面端与 Web 在当前会话中为每个 `execId` 打开一个只读侧边栏标签；其他客户端可忽略这些事件。

### Agent 浏览器（MCP `todex_desktop` 的 `browser_*` 工具）

浏览器由 daemon 在**自己所在的主机**上运行（`src/agent_browser`）：一个有界面的 Chrome for Testing（每个后端版本固定一个版本，目前 155.0.8059.39，支持 macOS arm64/x64、Linux x64/arm64、Windows x64；首次使用时下载并按源码中的 SHA-256 校验。下载只设连接与读取超时、不设总超时；`chromium/.install.lock` 文件锁让 daemon 与 `todex-agentd browser install` 不会同时下载，另一个进程在装时会等待其结果），因此 `localhost` 指后端主机。客户端只观看（实时画面、截图、操作记录）并处理授权。桌面端不再作为执行端；旧桌面端发来的 `executor.register` 仍返回成功（`retired: true`）但不再使用，`tunnel.open` 一律以 `tunnel.close` 拒绝。

- 开关：`GET /v2/agent-desktop` → `{ enabled, computerEnabled, computer?, browser?: { available, reason?, host, chromium: { version, installed, downloading, progress?, error?, overridden } }, executors: [] }`；`PUT /v2/agent-desktop { enabled }`。默认关闭，存于 `$DATA_DIR/agent-desktop.json`；打开时后台开始下载 Chromium。打开后，会话启动时与 `todex_ssh` 一样注入 `todex_desktop`（端点 `/internal/agent-mcp/desktop`，同一会话令牌与鉴权规则）。已运行的 Provider 要到下次启动才看到新工具；关闭立即生效（调用返回错误，并撤销所有会话授权、关闭标签）。旧 daemon 上此路由为 404，客户端据此隐藏设置。
- 工具：`browser_open {url}`、`browser_navigate {url | action: back|forward|reload}`、`browser_snapshot {screenshot?}`（URL、标题与带 `[ref=eN]` 的无障碍树，可附 JPEG）、`browser_act {action: click|type|press|scroll|select|hover|wait, ref?, text?, key?, deltaY?, ms?}`、`browser_close {}`。`browser_open`、`browser_navigate`、`browser_act` 算有副作用（见「权限模式」：`browser_open` 会加载页面，已有标签时等同导航），`browser_snapshot`、`browser_close` 不算。顶层地址只允许 `localhost`、`127.0.0.0/8`、`[::1]` 的 http(s)（规则集中在 `src/agent_browser/policy.rs`）；`data:`、`file:`、`blob:`、`javascript:` 与远程主机一律不能成为顶层页面，重定向的每一跳同样检查；iframe 与子资源不受限。
- 工具发现与引导：5 个 `browser_*` 工具带 `_meta`：`"anthropic/alwaysLoad": true` 与 `"anthropic/searchHint"`，Claude Code 因此不把它们延迟到工具搜索之后，会话一开始即可直接调用（其他客户端忽略这些键；`computer_*` 不带）。服务器 `instructions` 与 `browser_open` 的描述要求 Agent 查看本机页面时优先用这组工具而非 chrome-devtools、Playwright 等其他浏览器自动化，改动可在本机运行的 Web UI 后在此检查结果，必要时先在后台启动 dev server（需要密钥或新安装时跳过检查并说明原因）。
- 等待与截图：`browser_open` 与 `browser_navigate` 在文档加载完成（最多 10 秒）后，再等页面画出内容（两帧动画后有文字或图片、表单等元素，最多 5 秒）才返回，脚本渲染的单页应用不会被截到空白；Agent 应在它返回后再调用 `browser_snapshot`，不要与之并行。截图前与导航、操作前一样先恢复被最小化的窗口；窗口不绘制（仍最小化或被隐藏）时截图在 10 秒后返回 `NOT_RENDERING`，不带截图的 `browser_snapshot` 仍然可用。CDP 调用超时返回 `TIMEOUT`。
- 不可信内容：页面来的文字（快照的 URL、标题与无障碍树，其他浏览器工具结果里的 URL/标题）包在 `<untrusted_page_content>…</untrusted_page_content>` 中，内容里的每个 `<` 都转成 `&lt;`，因此无法开启或关闭任何边界标签。
- 运行方式：每个浏览器资料一个 Chromium 进程（同时最多 2 个，没有标签且 10 分钟没有工具调用时关闭），每个会话一个后台窗口（最多 4 个标签），打开时不抢焦点；无人观看且 10 分钟没有工具调用的标签会关闭，下次工具调用自动在原地址重新打开（之前的 `ref` 失效，需重新 `browser_snapshot`）；重新打开会再次加载该页面，所以 Plan 模式下不重开（返回 `PLAN_MODE`，地址保留到下一次非 Plan 的调用）；4 个标签的上限在打开前即占位，并发打开不会超出；同一会话的打开、关闭与闲置关闭串行（并发打开只建一个标签；撤销会等进行中的打开结束再关闭它；打开被取消或中途失败时已创建的窗口会被关闭、占位归还）；启动 Chromium 只占用该资料自己的锁，不阻塞其他资料和会话（运行中加启动中的 Chromium 合计不超过 2 个），删除资料会等其启动结束，之后不会再重建资料目录；CDP 在 macOS/Linux 走 `--remote-debugging-pipe`（fd 3/4），Windows 走匿名管道（`--remote-debugging-pipe --remote-debugging-io-pipes=<读句柄>,<写句柄>`，只在 spawn 瞬间把浏览器端句柄设为可继承），并把以挂起状态创建的 Chromium 放进 kill-on-close 的 Job Object，daemon 退出时整棵进程树一并结束；Linux 上有 X 显示（含 Wayland 会话的 XWayland）时以 `--ozone-platform=x11` 启动，因为 Wayland 窗口被最小化后应用无法自行恢复，页面停止绘制，截图与实时画面都会卡住；没有图形会话的 Linux 用 Xvfb（未安装时 `UNAVAILABLE`），由 `-displayfd` 选显示号，并用随机 MIT-MAGIC-COOKIE 的 Xauthority（`$DATA_DIR/agent-browser/x11/<uuid>/`，仅所有者可读，显示关闭即删除）限制客户端。Chromium 与 Xvfb 的 stderr 尾部（16 KiB，已脱敏）和退出状态会附在启动失败信息里。顶层导航与重定向只允许本机回环（`Fetch` 拦截，被拦截的导航取消并保留当前页），daemon 自身端口永远拒绝；页面内部的外部子资源不受限；下载与网站权限一律拒绝；JS 对话框自动处理（`alert`/`confirm`/`prompt` 取消，`beforeunload` 接受即允许离开页面，这样标签才能导航和关闭）；页面弹出的新窗口折叠进当前标签（本地地址在当前标签打开，其余拒绝）：新页面经 CDP 自动附加在加载任何内容前暂停，先判定再放行；`window.open(url)` 打开时还不知道地址，此时只放行脚本、拦截它的第一次文档请求并据此折叠（请求本身取消），5 秒内没有导航的弹窗直接关闭。Chromium 还没装好时工具返回 `BROWSER_INSTALLING`。`TODEX_AGENT_BROWSER_PATH` 可指向另一个 Chromium（开发用）；`todex-agentd browser install [--data-dir]` 或 `install.sh --with-browser` 可提前下载。
- 首次授权：会话第一次调用时发出 `permission.requested { kind: "desktop_browser", details: { host } }`，任意已配对设备都能回答，与权限模式无关；最多等待 5 分钟。授权只存在内存中，daemon 重启后重新询问。
- 敏感操作：向密码框输入时返回 `SENSITIVE_ACTION`，daemon 发出 `kind: "desktop_browser_action"` 的单次确认（任意设备可答），批准后带 `confirmed: true` 重试。Agent 不能自行传 `confirmed`。
- 浏览器资料：每个工作区（workspace id，没有时用路径）首次使用时自动创建独立资料（Cookie、存储、缓存），位于 `$DATA_DIR/agent-browser/profiles/<id>`，索引 `$DATA_DIR/agent-browser/profiles.json`。`GET /v2/agent-browser/profiles` → `{ profiles: [{ id, name, createdAt }], workspaces: { <workspace>: <profileId> } }`；`POST /v2/agent-browser/profiles { name }` 新建；`PUT /v2/agent-browser/profiles/{id} { name }` 改名；`DELETE /v2/agent-browser/profiles/{id}` 删除资料及其数据；`PUT /v2/agent-browser/workspaces { workspace, profileId }` 改用另一资料（该工作区已打开的标签会关闭，下次 `browser_open` 在新资料中打开）。`POST /v2/agent-browser/install` 开始下载 Chromium，返回设置对象。
- 实时画面：`/v2/ws` 上发 `{ id, type: "agentBrowser.watch", payload: { conversationId } }`（需能读取该会话）→ `server.result { watching: true }`，随后推送 `{ type: "agentBrowser.frame", payload: { conversationId, seq, mimeType: "image/jpeg", data, width, height } }`（CDP screencast，最多约 15 帧/秒：daemon 每帧在上一帧 66ms 后才确认，Chromium 据此降速编码；画面与导航守卫走不同通道，画面再多也不会让导航拦截丢事件；画面不进入连接的普通发送队列：每个观看有一个“最新帧”槽，未发出的旧帧被新帧覆盖，普通消息优先、但每 16 条普通消息后至少发出一帧；`closed` 不会因拥塞被丢弃），会话没有标签（从未打开、已关闭、被撤销、无人观看而闲置关闭，或 Chromium 崩溃）时推送 `{ conversationId, closed: true }`；标签存在但还没有第一帧时不推送任何消息（不会先发 `closed`），之后标签消失时才发 `closed`；`agentBrowser.unwatch` 停止。每个连接最多 8 个观看；没有观看者时停止录制；画面不写入事件记录。轮询兜底：`GET /v2/conversations/{id}/agent-desktop/frame?capability=browser` → `{ mimeType, dataUrl }`（没有标签时 404）。
- `DELETE /v2/conversations/{id}/agent-desktop?capability=browser`：撤销会话授权并关闭其标签（客户端“停止”按钮）。`GET /v2/conversations/{id}/agent-shots/{shotId}` → `{ shotId, mimeType, dataUrl }`。截图存于 `$DATA_DIR/agent-desktop/shots/`，每会话保留最新 200 张，会话删除时一并删除。
- 会话事件：`desktop.browser.grant { status: "granted"|"revoked", deviceId?, deviceName?, approvedBy?, reason? }`（`deviceId` 为 `host`，`deviceName` 为主机名，`approvedBy` 为实际批准的设备 id）；`desktop.browser.action { actionId, tool, ok, summary, url?, title?, error?, shotId?, deviceId, deviceName, approvedBy?, confirmedBy? }`（`approvedBy`：ask 模式下批准的设备；`confirmedBy`：确认敏感操作的设备）。`desktop.browser.tab { status: "closed", reason: "idle"|"crash"|"user"|"revoked"|"restart" }`：会话的标签不在了（`idle` 无人观看且长时间没有工具调用而关闭，下次工具调用会在原地址重开；`crash` Chromium 退出；`user` `browser_close`、在主机上关掉窗口或更换/删除浏览器资料；`revoked` 撤销了会话的浏览器授权；`restart` 上一个 daemon 停止时标签还开着，启动时补记）。从未打开过标签的会话不产生该事件；客户端据此把标签标为已关闭。daemon 在 `$DATA_DIR/agent-browser/open-tabs.json`（仅会话 id，0600）记录持有标签的会话，用于启动时补记 `restart`，补记后清空。事件不含截图数据与输入的文字；`detail=summary` 回放也保留 `desktop.*` 事件的完整内容。
- 截图交给模型：2026-10 实测 Codex、Claude Code、Grok、Devin 都能把 `browser_snapshot` 的图片交给模型；OpenCode 当次改用 `curl` 读取页面源码，图片可能没有传给模型，此时只能依赖无障碍树文本。ACP 类 Agent 处于询问模式时，仍可能对这些 MCP 工具弹出原生权限请求。
- 设备限定的权限：任何 `permission.requested` 都可能带 `allowedDeviceIds`，客户端应在本机不在列表中时隐藏操作按钮。

### Computer Use（`todex_desktop` 的 `computer_*` 工具）

Computer Use 由 daemon 在**自己所在的主机**上执行（`src/computer`，基于 xa11y：macOS AX、Windows UI Automation、Linux AT-SPI），客户端只做预览和审批。桌面端执行端不再提供 `screen` 能力；旧桌面端登记的 `screen` 会被忽略（浏览器照常可用）。

- 开关：`PUT /v2/agent-desktop` 也接受 `{ computerEnabled }`（字段都可选，至少给一个），默认关闭，只有 `enabled` 也打开时生效。打开后工具列表多出 `computer_observe`、`computer_act`、`computer_done`（已运行的 Provider 要到下次启动才看到；关闭立即生效，并结束屏幕会话）。
- 主机状态：`GET /v2/agent-desktop` 另返回 `computer { supported, available, reason?, host, platform, permissions: { screen, accessibility } }`。`available` 要求系统支持、权限齐全、且 daemon 运行在主机的桌面会话里（能弹出确认框）。`POST /v2/agent-desktop/computer/permissions` 在主机上弹出系统授权提示（macOS：屏幕录制、辅助功能），返回同样的设置对象；可选请求体 `{ "permission": "screen" | "accessibility" }` 只请求其中一项，省略则请求所有缺失的。macOS 对同一身份只弹一次系统提示，之后的请求不再有任何界面，所以请求后仍缺失的权限会同时在主机上打开系统设置的对应页面（`x-apple.systempreferences:…?Privacy_ScreenCapture` / `Privacy_Accessibility`）。macOS 上 `serve`/`tui`/`daemon-run` 启动时会以自身身份重新执行（pid 不变），权限授给 `todex-agentd` 本身而不是启动它的终端；授权按“路径 + 签名身份”记录，所以发布版必须用固定证书签名、安装路径保持不变。
- 应用列表（输入框 `@app:` 提及）：`GET /v2/agent-desktop/computer/apps` → `{ apps: [{ id, name, running }] }`。先列正在运行的应用（前台应用在最前，其余按名称排序），再列已安装的应用（按名称排序），按 `id` 去重（不区分大小写，运行中的优先）；不含 TodeX 自身与永远禁止控制的应用（同截图涂黑的范围），也不含空 `id` 或含空白的 `id`。`id` 可直接作为 `computer_observe`/`computer_act` 的 `app` 参数和 `open_app` 的目标：macOS 为 bundle id（扫描 `/Applications`、`/Applications/Utilities`、`/System/Applications`、`/System/Applications/Utilities`、`~/Applications` 一层的 `.app`）；Windows 为小写可执行文件名（App Paths 注册与开始菜单快捷方式指向的 `.exe`）；Linux 为 desktop id（不含 `NoDisplay`/`Hidden` 条目）。已安装部分缓存约 60 秒；运行中部分每次重新读取，读取失败（如缺少辅助功能权限）或引擎被操作占用超过 2 秒时只返回已安装部分。总开关或 Computer Use 未打开时返回 409（`Computer Use is off`）。客户端选中后插入 `@app:<id> `，智能体按工具说明把该 `id` 直接作为 `app` 传入。
- 首次授权：只能由主机前的人确认——daemon 在主机上弹出原生对话框（macOS NSAlert；300 秒无人确认按拒绝处理），其他设备无法代为批准。事件 `desktop.computer.grant { status: "requested" }`（客户端据此显示“等待主机确认”），随后 `granted`（带 `approvedBy: "host"`）或 `declined`（调用被取消时带 `reason: "cancelled"`）；用户撤销或在主机上按“停止”时为 `revoked`。无人可确认时工具返回 `UNAVAILABLE`。
- 不可信内容：`computer_observe` 的文本先写 daemon 自己的截图尺寸说明，屏幕来的内容（应用名、窗口标题、无障碍树）包在 `<untrusted_screen_content>…</untrusted_screen_content>` 中，转义规则同浏览器。`computer_act` 的结果文本同样把应用名与 ID 包在该标签内；报错信息里引用屏幕应用名的（`TARGET_BLOCKED`、`APP_CONFIRM` 等）整条也包在该标签内。`computer_act` 算有副作用（见「权限模式」），`computer_observe`、`computer_done` 不算。
- 屏幕租约：主机同一时间只服务一个会话，其他会话调用返回 `SCREEN_BUSY`；`computer_done`、停止、撤销或闲置 120 秒后释放（每 15 秒清理一次）。有调用正在运行（含等待主机确认）时租约不算闲置，也不会被其他会话接管；停止、撤销、设置关闭或释放租约会立即中止该会话正在运行的调用（返回 `STOPPED`），已排队的输入不再发出，等待中的主机确认框保留到被回答，撤销之前给出的“允许”作废、之后给出的仍有效。租约期间主机显示“Agent 正在控制”的浮条（不进截图），浮条“停止”或 ⌘⇧⎋（macOS）结束当前会话并撤销其授权。
- 按应用批准：本会话首次操作某应用时，daemon 在主机上弹出原生确认框（与首次授权相同，只能由主机前的人确认，其他设备无法代答，也不再发 `permission.requested`），批准后本会话记住该应用；无法识别目标应用（桌面、菜单栏图标等）时每次都在主机上确认、只放行这一次；向密码框输入时同样在主机上单次确认。Agent 无法自行跳过。主机上有 TodeX 确认框时，所有 `computer_act` 都返回 `TARGET_BLOCKED`（每次注入前、长文本每段输入前都会再查）。主机上没有能弹确认的界面（无桌面会话、Linux 无通知服务与 `kdialog`/`zenity`）返回 `UNAVAILABLE`（不进入拒绝冷却）；确认框本身出错返回 `FAILED`（同样不冷却）。
- 禁止操作的应用（TodeX 自身进程的任何窗口——不论窗口层级、系统认证窗口、凭据存储、系统设置、密码管理器）返回 `TARGET_BLOCKED`；指针动作在真正注入前再次命中检测，命中这些窗口同样拒绝，所有指针路径（点击/右键的 ref 回退、双击、悬停、滚动含默认落点、拖拽起点与终点）注入前都会检查落点所在应用：Agent 给的坐标处已换成别的应用返回 `TARGET_CHANGED`；ref 中心、默认滚动点或拖拽终点落在另一个应用上时，已批准的放行，未批准的按“按应用批准”在主机上询问；平台无法命中检测时沿用目标应用检查。Linux 上确认框所用的 `kdialog`、`zenity` 同样不可控制。`computer_observe` 也遵守此列表：指名观察这些应用返回 `TARGET_BLOCKED`；它们在前台时不返回其无障碍树，窗口列表不含其标题，截图中其可见窗口区域被涂成灰色（平台无法列出窗口时，若它们出现在截图范围内则拒绝截图）。
- 系统快捷键：`key` 拒绝结束/锁定会话、强制退出、打开启动器或命令行的快捷键（返回 `TARGET_BLOCKED`），多按修饰键也算：macOS ⌃⌘Q、⌘⇧Q、⌘⌥Esc、⌘Space；Windows Win+L、Ctrl+Alt+Del、Ctrl+Shift+Esc、Win+R、Win+X；Linux Super+L、Ctrl+Alt+L/Del/Backspace/Esc、Ctrl+Alt+F1…F12、Ctrl+Alt+Shift+PgUp/PgDn、Alt+F2、Alt+Space。⌘Tab/Alt+Tab 不拦。需要切到前台再按键或输入时，注入前确认目标应用确实在前台（最多等 500ms），否则返回 `TARGET_CHANGED`，不会盲打。
- 超时或取消：`computer_act` 超时（30 秒）或被取消后，尚未发出的输入不再发出（返回 `TIMEOUT`/`CANCELLED`）；文字按约 32 字符分段输入，每段前检查。同一时间只有一个调用操作主机，前一个调用占用超过 10 秒仍未结束时返回 `BUSY`；实时画面单次抓取超过 5 秒返回超时错误，不缓存。macOS 经 `AXSelectedText` 写入文字（有 `ref` 或无 `ref` 都一样）后，在约 300ms 内重读输入框的值；始终没变（应用忽略了写入，如 Typora 等 Electron/网页编辑器）则激活应用改用按键输入（`path: "keyboard"`），读不到值（如密码框）时以写入调用的结果为准。按键输入后约 1 秒内值仍没变返回 `TYPE_NOT_APPLIED`（文字含换行、回车或 Tab 时不做此检查，因为提交或换焦点会让值看起来没变）。
- `computer_observe { app?, window?, display?, screenshot?=true }` → 前台（或指定）应用、窗口列表（`id` 来自本次观察）、带 `[ref=eN]` 的无障碍树、显示器、窗口（或整屏）截图；`computer_act { action: click|double_click|right_click|hover|drag|scroll|type|key|wait|open_app|focus_window, ref?, x?, y?, toX?, toY?, text?, keys?, app?, window?, deltaX?, deltaY?, ms? }`。有 `ref` 时尽量在后台送达元素（`path: "background"`，不动指针、不抢焦点；文字直接插入，不受输入法影响），只给坐标时移动指针（`path: "pointer"`，用户正在操作时返回 `USER_ACTIVE`）；无法后台完成的按键/输入会先激活目标应用（`path: "keyboard"`）。`keys` 里的 `cmd` 在 macOS 是 ⌘，其他平台是 Ctrl。
- 实时画面：`GET /v2/conversations/{id}/agent-desktop/frame` → `{ mimeType, dataUrl }`，仅返回给当前持有屏幕租约的会话（否则 404）；最多每 300ms 截一次（宽 960、JPEG），多个观看者共享。与截图一样，TodeX、系统设置、凭据存储和密码管理器的窗口会被涂黑；平台无法列出窗口且可能拍到它们时返回错误而不是画面。客户端在预览可见时轮询，没有观看者时不截图。
- 会话事件：`desktop.computer.session { status: "started"|"ended", deviceId?, deviceName?, reason? }`（reason：`done`、`idle`、`user`、`revoked`；`deviceId` 为 `host`，`deviceName` 为主机名）；`desktop.computer.action { actionId, tool, ok, summary, app?, windowTitle?, path?, error?, shotId?, deviceId, deviceName, approvedBy?, confirmedBy? }`（`approvedBy`：ask 模式下批准的设备；`confirmedBy`：按应用批准或敏感确认，恒为 `host`）。事件不含截图数据与输入的文字。
- `DELETE /v2/conversations/{id}/agent-desktop?capability=screen|browser` 只撤销其中一项；不带参数撤销两项。
- 受控主机（daemon 所在电脑，`GET /v2/agent-desktop` 返回的 `computer { supported, available, reason, platform, permissions }` 反映这些条件）：macOS 14+（需屏幕录制与辅助功能授权；浮条 + ⌘⇧⎋ 停止）；Windows 需在已登录用户的交互式桌面运行（作为服务在 session 0 或桌面锁定时不可用；无需授权；置顶状态窗 + Ctrl+Alt+Shift+Esc 停止；坐标为物理像素，进程按 Per-Monitor-V2 DPI 感知；应用标识为小写可执行文件名，如 `notepad.exe`）；Linux 支持 X11 会话与 KDE Plasma 6.6+ 的 Wayland 会话（其他 Wayland 桌面及更早的 Plasma 返回 `supported: false` 并说明原因；辅助功能指 AT-SPI 的 `org.a11y.Status.IsEnabled`，请求授权时由 daemon 打开；状态与授权确认用桌面通知的“停止”/“允许”/“拒绝”按钮，无通知服务时退回 `kdialog`/`zenity`；X11 没有全局停止快捷键；应用标识为小写可执行文件名；daemon 不链接 libxkbcommon 等图形库，无图形环境的服务器也能正常启动）。Windows 与 Linux 上无元素的 `type` 与 `key` 会先把目标应用切到前台再模拟按键。
- KDE Plasma Wayland（`src/computer/platform/kde_wayland.rs`）：输入走 RemoteDesktop 门户（首次由 KDE 弹窗征得主机同意，恢复令牌存于 `$XDG_STATE_HOME/todex/remote-desktop.token`，0600；会话随屏幕租约打开与关闭），截图走 KWin `ScreenShot2`（daemon 安装隐藏的 `~/.local/share/applications/com.unbaked0692.todex.agentd.desktop` 声明授权，未授权时退回 Screenshot 门户整屏截取），窗口、显示器与激活靠临时 KWin 脚本，空闲时间来自 `ext-idle-notify-v1`（2 秒粒度）。Wayland 原生窗口的 AT-SPI 坐标是窗口内坐标，daemon 按 KWin 窗口位置换算为全局逻辑坐标。Plasma 6.8 之前，向已知非密码框输入非 ASCII 文字或向 Chromium 系应用输入时，改用 Klipper 剪贴板 + Ctrl+V（约 400ms 后恢复原文字剪贴板，Klipper 历史会留下该文字）。停止快捷键 Ctrl+Alt+Shift+Esc 经 KGlobalAccel 注册（冲突时没有快捷键）。`POST .../computer/permissions` 安装截图授权并提前弹出远程控制同意框（`permission: "screen"`）、打开 AT-SPI（`permission: "accessibility"`）；省略则两者都做。daemon 不会写 KDE 的权限库来跳过同意框（只有 CI 的隔离总线这样做）。
- KDE Wayland 真机验证清单（CI 任务 `kde-wayland` 只在无头 KWin 中跑过逻辑，以下需在 Plasma 6.6、6.7、6.8 实机逐项确认）：首次远程控制同意框出现且重启后靠恢复令牌免弹窗；6.6/6.7 上 `ScreenShot2` 授权生效（不再退回门户）；多显示器（含不同缩放）上按坐标与按 ref 点击落点正确；Firefox、Chromium、GTK4 应用的元素坐标与截图对齐；滚动方向（`deltaY` 为正向下）；fcitx5 下中日韩文字输入；Ctrl+Alt+Shift+Esc 停止会话；通知“停止”/“允许”/“拒绝”可用。

### Agent 调用（MCP `todex_agents`）

“Agent 调用”的 MCP 入口：在 TodeX 中运行的 Agent 调用另一个 TodeX Agent（例如 Claude Code 把一项子任务交给 Codex）。与外部 API（CLI 入口，见 [外部 API（API Key）](#外部-apiapi-key)）共用同一调用层和 `agent-calling.json` 中的全局范围（允许的 Agent 与模型）。

- 注入：`agent-calling.json` 的 `mcp.enabled` 为 `true` 且全局范围至少允许一个 Agent（即 `GET /v2/agent-calling` 的 `mcp.injected`）时，与 `todex_ssh` 一样注入名为 `todex_agents` 的服务器（端点 `/internal/agent-mcp/agents`，同样的回环 + 会话令牌守卫；Claude Code 的 `--allowedTools` 带 `mcp__todex_agents`；Antigravity 的全局静态配置也有 `todex_agents` 条目，只在本轮启用时提供工具；Pi 经同一扩展加载）。Provider 的单次工具超时为 1500 秒（ask 模式审批最多 300 秒 + 最长等待 1200 秒）。以下会话不注入：API key 会话（与其他 TodeX MCP 服务器相同）以及 Agent 调用产生的子会话（只允许一层调用）；它们即使拿到令牌调用，也返回 `UNAUTHORIZED`。关闭开关后，已注入的服务器在每次调用时检查设置并返回 `AGENT_CALLING_DISABLED`。
- 工具（字段与 `/api/v1` 对应）：

| 工具 | 对应 `/api/v1` | 输入 | 有副作用 |
|---|---|---|---|
| `agents_list` | `GET /agents`（含模型） | `{}` | 否 |
| `agent_run` | `POST /runs` | `{ agent, text, model?, reasoningEffort?, title?, waitSecs? }` | 是 |
| `agent_turn` | `POST /conversations/{id}/turns` | `{ conversationId, text, model?, reasoningEffort?, waitSecs? }` | 是 |
| `agent_wait` | — | `{ conversationId, turnId?, waitSecs? }` | 否 |
| `agent_cancel` | `POST /conversations/{id}/cancel` | `{ conversationId, turnId? }` | 否（只停止自己的子会话） |

- 结果（结构化 JSON）：
  - `agents_list` → `{ agents: [{ agent, displayName, available, models: [{ id, displayName, isDefault, supportedReasoningEfforts, defaultReasoningEffort? }] }], limits: { maxConcurrentCalls, maxWaitSecs, defaultWaitSecs } }`，按全局范围过滤 Agent 与模型（模型来自 Provider 快照目录）。
  - `agent_run` / `agent_turn` / `agent_wait` → `{ conversationId, turnId, status, output, error }`。`status` 为 `running`、`completed`、`failed`、`cancelled`、`interrupted` 之一（等待权限时仍报 `running`）；`output` 是被调用 Agent 主代理的回复文本（不含子代理与工具输出，最多 256 KiB，超出截断并附 `[output truncated]`）；`error` 在未完成时为 `{ code?, message }`，否则为 `null`。
  - `agent_cancel` → `{ conversationId, cancelled, turnId }`：`cancelled` 表示是否向正在运行的轮次发出了停止（没有运行中的轮次或 `turnId` 不是运行中的轮次时为 `false`）。
- 等待语义：`waitSecs` 默认 300，取值限制在 1–1200。到时轮次仍在运行则返回 `status: "running"` 与目前的输出，子会话**继续运行、不会被取消**；之后用 `agent_wait` 取结果（不给 `turnId` 时等最近一次调用的轮次）。已结束轮次的结果（`status`、`output`、`error`）可被 `agent_wait` 重复读取，直到该子会话开始下一轮或轮次结束 30 分钟后 `output` 从内存中丢弃，之后的 `agent_wait` 只返回状态与空 `output`（父会话事件中仍有记录）。Agent 的工具调用被取消时停止等待，子会话不受影响。
- 限制与错误（工具错误文本为 `CODE: message`）：每个父会话最多 4 个同时运行的调用（`AGENT_CALL_LIMIT`）；Agent、模型不在全局范围内或 `reasoningEffort` 不在模型的 `supportedReasoningEfforts` 中返回 `UNAUTHORIZED`；`agent_turn`/`agent_wait`/`agent_cancel` 只接受调用者自己的子会话，其他会话（包括不存在的）一律 `NOT_FOUND`；子会话仍在运行轮次、或另一调用正在为它启动轮次时 `agent_turn` 返回 `CONFLICT`（同一子会话同时到达的两个 `agent_turn` 只有一个启动）；Plan 模式下 `agent_run`/`agent_turn` 返回 `PLAN_MODE`；创建与 Provider 错误按原错误码透传（如 `PROVIDER_UNAVAILABLE`）。
- 子会话：`agent_run` 以父会话的 owner、父会话的工作区创建新会话（标题为给定的 `title`，缺省 `Agent call · <Agent 显示名>`），manifest 带 `parentConversationId: <父会话 id>`。模型解析同外部 API：该 Agent 受 `scope.models` 限制时，未指定 `model` 的调用取 Provider 默认模型（若允许），否则取第一个允许的模型，并明确发给 Agent——对已有子会话的 `agent_turn` 也一样，可能在会话中途切换模型；不受限制时由 Agent 使用自己的默认模型。子会话首个 prompt 失败时（例如 Provider 报错），刚创建的子会话随即删除，不留下隐藏会话（外部 API `runs` 的行为不变）。
- 权限继承：子轮次的权限模式不超过父会话当前轮次的模式：取被调用 Agent 支持的、不高于父模式（`ask` < `accept-edits` < `auto` < `full-access`）的最高模式（例如 Devin 在父模式 `auto` 下以 `ask` 运行，Codex 在父模式 `accept-edits` 下以 `ask` 运行）；父会话的 `auto` 受 OS 沙箱约束（Codex，`enforcement: "sandbox"`）而被调用 Agent 只按自身策略执行时，上限再降为 `accept-edits`（只有 Claude Code 提供该档，其他降为 `ask`），子会话不会在父会话的沙箱之外自由执行；没有这样的模式时（如 Pi 只支持 `full-access`，而父模式更低）返回 `UNAUTHORIZED`。`workMode` 为 `implement`。`agent_run`/`agent_turn` 本身是有副作用的工具：父会话为 `ask` 时每次调用先发出 `permission.requested`（`input` 带 Agent、模型、标题与任务文本的前 4 KiB）。子会话在该模式下发出的权限请求再按 `agent-calling.json` 的 `mcp.approval` 处理：`ask` 照常交给用户（已配对设备，在子会话中作答）、`auto-approve` 自动批准（设备绑定的请求除外，一律拒绝）、`reject` 自动拒绝。因此设为 `auto-approve` 时，即使父会话处于 `ask` 模式，子会话的权限请求也会被自动批准——这是用户在设置中的明确选择。
- 父会话事件（写入**父会话**的事件记录，照常加密；`turnId` 由后端填为父会话当时运行中的轮次，子会话一律用 `childConversationId`/`childTurnId`）：
  - `agent.call.started { childConversationId, childTurnId, agent, model?, reasoningEffort?, text, title?, status: "running", startedAt }`（`text` 最多 16 KiB；`title` 只在 `agent_run` 时给出）。
  - `agent.call.updated { childConversationId, childTurnId, status: "running" | "waiting_permission" }`：子轮次发出权限请求时为 `waiting_permission`，请求全部答复后回到 `running`。
  - `agent.call.completed { childConversationId, childTurnId, status: "completed", output, durationMs }`。
  - `agent.call.failed { childConversationId, childTurnId, status: "failed" | "interrupted", error: { code?, message }, output?, durationMs? }`。
  - `agent.call.cancelled { childConversationId, childTurnId, status: "cancelled", output?, durationMs? }`。
  - 事件中的 `output` 最多 64 KiB。
- 子会话可见性：`GET /v2/conversations` 默认不列出子会话，加 `?includeChildren=true` 才包含；按 id 读取、订阅、回放、发送 prompt 与删除子会话照常可用（客户端可从父会话的 `agent.call.*` 事件打开子会话）。
- 生命周期：父会话的轮次被取消或中断时，取消它所有运行中的子轮次（随后父会话收到 `agent.call.cancelled`）；调用仍在启动时（创建子会话、等待 CLI 启动）父轮次被取消或中断（或已结束），子轮次一启动就被取消，该调用返回 `status: "cancelled"`；父轮次正常结束时子会话继续运行，下一轮可用 `agent_wait` 取结果。删除父会话（包括过期与历史清理）时先取消运行中的子轮次，再删除它的全部子会话。运行中的调用记录在 `$DATA_DIR/agent-calls.json`（0600，原子替换，只含会话与轮次 id、Agent 与开始时间）；daemon 重启后，对其中残留的每个调用向父会话追加 `agent.call.failed`，`error.code = "DAEMON_RESTARTED"`，然后清空该文件。只有被关闭停止的调用（取消、中断，或关闭时未能读到结束）才留待下次启动报告；关闭期间自行完成或失败的调用照常记录并移出该文件。若子轮次已不在运行却没有收到它的结束事件，调用不会一直停在运行中：按子会话日志里该轮次的结束事件（没有时按子会话状态，否则为 `failed`）结束，`error.code = "TURN_END_UNOBSERVED"`。
- 明文：后端读取子会话的回复时不解密历史，而是在事件加密写入前，在内存中取得所需的最少内容（主代理文本、轮次结束与错误、权限请求 id）；这些内容不写盘、不进日志，轮次结束后即不再读取。

## WebSocket 协议

业务消息都是 JSON 文本。帧格式取决于握手选择的传输（`src/server/ws/codec.rs` 中的 codec，业务分发只接触 JSON）：

- 明文（回环对端）：文本帧即 JSON，二进制帧忽略。
- transport v2（`tv=2`）：升级后服务端先发一条文本 hello `{"type":"todex.transport.hello","version":2,"serverNonce":"<b64url 32B>"}`，客户端收到前不得发送；之后双向都是二进制帧 `u64_be(i) || XChaCha20-Poly1305 密文`。完整规范见 [transport-v2.md](transport-v2.md)。

连接示例：

握手认证与 HTTP 一致：四个 `x-todex-*` header，或等价 query 参数（Electron 原生 WebSocket、浏览器等无法设置 header 的客户端）：`ws://127.0.0.1:7345/v2/ws?device_id=<id>&auth_ts=<unix>&auth_nonce=<b64url>&auth_sig=<b64url>`。签名按 `GET /v2/ws`、完整 canonical query 与空 body 计算；query 中的传输加密参数同样被签名覆盖，因此握手材料无法被中间人替换。注意 query 凭据可能进入反向代理日志，生产环境优先使用 header。

TUI 配对二维码只携带后端地址（`{"kind":"todex-pairing-link","version":2,"serverUrl":"http://host:port"}`，不分片），不携带公钥或任何访问凭据——设备身份与传输公钥一律走 `/v2/device-pairing` 配对流程获取。

- transport v2：`ws://.../v2/ws?tv=2&enc=<x25519|ml-kem-768>&client_nonce=<b64url 32B>&client_key=<b64url>`（ML-KEM-768 用 `ciphertext=<b64url>` 代替 `client_key`），再加设备签名参数。会话密钥由 HKDF-SHA256 从共享秘密与 transcript（含签名凭证里的 `device_id`，关闭认证时为空串、双方 nonce）派生，每个方向独立密钥与严格递增计数器，服务端 nonce 每次连接新鲜生成，重放握手不会得到相同密钥。`tv` 不是 `2` 时升级前返回 426 `PROTOCOL_UPGRADE_REQUIRED`；协议与服务端 `pairing_encryption` 不符返回 403；握手材料畸形、帧解密失败、计数器不连续或 hello 之后收到文本帧时以 close code `4400`、reason `transport crypto failure` 关闭，不带细节。开启认证时，已建立的 `/v2/ws`（不论是否 transport v2）每 5 秒复查一次签名设备是否仍在注册表中（TUI 或 `devices revoke` 在其他进程中吊销同样生效），已被吊销时以 close code `4401`、reason `device revoked` 关闭。
- transport v1（`todex.crypto.v1`，`enc=` 而无 `tv`）已停用：升级前返回 426 `PROTOCOL_UPGRADE_REQUIRED`。不带 `enc` 的明文握手只接受回环对端，且服务端 `pairing_encryption` 必须为 `none`（否则 403）。

兼容端点已移除：`/v1/ws` 与 `/v1/*` HTTP 不再注册，访问返回 404。旧客户端必须升级。

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

设备签名认证当前映射到租户 `local`，认证主体是 `deviceId`（由设备公钥派生）。请求 payload 中的 `tenantId` 必须与认证上下文匹配。鉴权结果会写入 `$TODEX_AGENTD_DATA_DIR/audit/audit.jsonl`（终端操作写入 `audit-terminal.jsonl`；不再广播，客户端从未收到过审计事件），审计记录中的 `token_id`/`principal_id` 即 `deviceId`。

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
2. **本地控制命令**（原 `/v1/ws` 能力）：`terminal.*`（`terminal.start` 可带 `ssh: { host, password? }`，见「SSH 终端」）、`codex.local.*`、`codex.gateway.control`、`codex.mcp.*`、`codex.cloudTask.*`。应答与事件通过按连接隔离的 ServerEvent 流返回（见「事件」一节），连接只会收到自己触碰过的 Codex session / 终端的事件。

单帧上限 8 MiB（聊天附件以 base64 data URL 传输，无出站分片），由 WebSocket 升级层强制：超限消息直接关闭连接，不再返回 `INVALID_REQUEST` 帧。服务端每 30 秒发送 WebSocket Ping，90 秒无入站帧即关闭连接。

### `session.resume`（断线恢复）

连接建立后发送，携带客户端持久化的 Codex session cursor，服务端为仍存在的 session 授予事件可见性并重放 cursor 之后的事件（每 session 最多 80 条，最多 12 个 session）。替代已删除的 transport hello 握手：

```json
{
  "id": "resume-1",
  "type": "session.resume",
  "payload": { "sessionCursors": { "cdxs_local_1": 100 } }
}
```

## 终端（`terminal.*`）

交互式终端运行在 daemon 的 PTY 中，不随 WebSocket 断开而结束；任何一条带 `terminalId` 的 `terminal.*` 命令都会让当前连接开始接收该终端的事件。所有命令都需要 `tenantId`，并写入终端审计（见上文 `audit-terminal.jsonl`）。

| 命令 | payload | 说明 |
|---|---|---|
| `terminal.start` | `terminalId?`、`cwd`、`shell?`、`rows?`、`cols?`、`workspaceId?`、`ssh?` | 启动 shell（或 SSH，见「SSH 终端」）。`terminalId` 已在运行时报错。行列限制在 8–200 × 20–400。 |
| `terminal.input` | `terminalId`、`data` | 写入 PTY。输入队列（256 条）已满时不等待，直接返回错误，提示程序没有读取输入。 |
| `terminal.resize` | `terminalId`、`rows?`、`cols?` | 调整 PTY 窗口。 |
| `terminal.stop` | `terminalId`、`force?` | `force` 杀进程，否则向 shell 写入 `exit`。 |
| `terminal.status` | `terminalId?`、`workspaceId?` | 返回 `terminal.status { terminalId, terminals: [...] }`，只列仍在运行的终端；客户端据此把没有列出的 `terminalId` 视为已结束。 |
| `terminal.attach` | `terminalId`、`since?`、`rows?`、`cols?` | 重新接入终端，见下文。 |
| `terminal.clear` | `terminalId` | 丢弃 daemon 保留的输出（之后的 attach 不再回放），发布 `terminal.cleared { terminalId, seq }`。 |

事件：`terminal.started`、`terminal.output`、`terminal.input.accepted`、`terminal.resized`、`terminal.stopping`、`terminal.exited`、`terminal.error`、`terminal.status`、`terminal.attached`、`terminal.cleared`。终端命令失败时返回的 `error` 事件带 `pane_id` = `terminalId`，payload 额外带 `requestId` 与 `terminalId`（`terminal.start` 未给 id、`terminal.status` 未指定终端时除外）。

**输出合批与序号。** PTY 以 64 KiB 读取；已排队的读取合并为一条 `terminal.output`（单条最多 256 KiB）。距上一批不到 10 ms 又有输出时视为持续输出，再等待最多 4 ms 收集后续读取；间隔更长的输出（交互回显）立即发出，所以大量输出只产生少量事件。每条 `terminal.output` 带 `seq`：同一终端从 1 开始逐条加一。

**保留与 `terminal.attach`。** daemon 为每个运行中的终端保留最近约 2 MiB 输出（按整批淘汰，终端退出后释放）。`terminal.attach` 先按 `rows`/`cols` 调整窗口（尺寸变化时会先发 `terminal.resized`），再发布：

```json
{
  "type": "terminal.attached",
  "pane_id": "term_1",
  "payload": {
    "requestId": "attach-1", "terminalId": "term_1", "running": true,
    "reset": false, "snapshot": false, "seq": 120, "data": "…",
    "cwd": "/repo", "shell": "/bin/zsh", "pid": 4242, "rows": 30, "cols": 100,
    "startedAt": 1792540000000, "lifecycleState": "running", "ssh": null
  }
}
```

- `since` 仍在保留范围内时 `reset: false`，`data` 是 `seq` 在 `(since, seq]` 之间的输出。
- `since` 缺省、早于保留范围或大于当前 `seq` 时 `reset: true`，客户端应先清空终端再写入 `data`：daemon 为每个终端维护一份无头终端镜像（`alacritty_terminal`），此时 `data` 是当前屏幕的快照、`snapshot: true`——主屏与最近 2000 行滚动历史、使用中的备用屏、光标位置与形状、当前样式、滚动区域、模式（应用光标/小键盘、插入、鼠标跟踪与 1005/1006 编码、焦点上报、bracketed paste、自动换行、光标显隐）、Kitty 键盘标志与标题，按 attach 后的行列生成；不含鼠标编码 1015/1016 与快照之前的 OSC 133 标记。镜像从不回应程序的查询。设置环境变量 `TODEX_AGENTD_TERMINAL_SNAPSHOT=0` 时不建镜像，`data` 退回全部保留输出、`snapshot: false`。
- `terminal.clear` 同时让镜像只保留光标所在行（置顶）并丢弃滚动历史，与客户端的清屏一致。
- `terminal.attached` 在保留锁内发布：`seq` 及之前的批次都在它之前发出，之后的批次都在它之后，客户端按 `seq` 去重即可无缝衔接。
- 终端未运行时返回 `{ terminalId, requestId, running: false }` 而不是错误，客户端据此启动新终端。
- 连接落后导致事件流 `EVENT_STREAM_LAGGED` 时，终端输出不会自动补发；客户端应对仍在运行的终端用最后的 `seq` 再次 attach。看到 `seq` 不连续时同理。

`GET /v2/version` 返回 `terminalReplay: 1` 表示支持上述 `seq`、`terminal.attach` 与 `terminal.clear`，`terminalSnapshot: 1` 表示无法衔接时发送屏幕快照；旧 daemon 没有该字段，客户端应回退到 `terminal.status` + `terminal.start`。

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
| `history.encryption.updated` | 历史加密密钥状态变化（见「历史加密密钥」）。 |
| `error` | 通用后端错误事件。 |

## 错误码

| code | 说明 |
| --- | --- |
| `INVALID_REQUEST` | JSON 格式、字段或消息类型不符合当前协议。 |
| `UNAUTHENTICATED` | `enable_auth` 开启时未提供有效设备签名（header 或 query 参数），或设备未注册/已吊销、nonce 重放；配对 `reveal` 承诺不符、`poll`/`cancel` proof 无效。 |
| `AUTH_TIMESTAMP_REJECTED` | 设备签名时间戳与服务端相差超过 300 秒或早于 daemon 启动（HTTP 401）；顶层附带 `serverTime`（Unix 秒），REST 客户端据此校正时钟偏移、重签重试一次。 |
| `RATE_LIMITED` | 请求频率超限（HTTP 429，带 `Retry-After`）：单设备签名 nonce 超过 8192 个/窗口，或配对 `create` / `reveal` / `poll` / `cancel` 频率超限。 |
| `PAIRING_BUSY` | 配对申请已满（HTTP 429）：同一来源已有 2 个未完成申请，或 16 个活动申请都已 reveal。 |
| `TRANSPORT_BUSY` | `/v2/sealed` 隧道 10 秒内没有空闲名额（加密的内层 HTTP 503，`retry-after: 1`）；请求未执行、nonce 未占用，可用新签名重试一次。 |
| `REQUEST_TIMEOUT` | `/v2/sealed` 外层 body 在内层 head 解出后停顿超过 30 秒（加密的内层 HTTP 408）。 |
| `UNAUTHORIZED` | tenant 与认证上下文不匹配；或 `enable_auth = false` 时请求的 `Host`/`Origin` 不是本地来源。 |
| `UNSUPPORTED` | 请求能力不在当前后端支持范围。 |
| `PAYLOAD_TOO_LARGE` | 请求或其读取的资源超过固定的大小上限（HTTP 413），如 Skill bundle 超过 200 个文件或 4 MiB。 |
| `REMOTE_AUTH_FAILED` | SFTP/FTP 登录失败（HTTP 403）：提供密码，或先在终端登录以复用共享连接。 |
| `REMOTE_HOST_KEY_UNVERIFIED` | 远程主机密钥未确认或已变化（HTTP 403）：先在 TodeX 终端中连接一次。 |
| `REMOTE_UNREACHABLE` | 远程主机不可达或连接中断（HTTP 502），会话随之关闭。 |
| `BROWSER_TARGET_UNREACHABLE` | `/v2/browser/proxy` 无法从 loopback 目标得到响应（连接被拒、超时、DNS 失败等，HTTP 502）；`message` 含目标 `host:port`。 |
| `REMOTE_PERMISSION_DENIED` | 远程服务器拒绝该文件操作（HTTP 403）；与 TodeX 设备认证无关。 |
| `REMOTE_OPERATION_FAILED` | 其他远程文件操作失败（HTTP 422）。 |
| `PROTOCOL_UPGRADE_REQUIRED` | 请求使用了已停用的协议（HTTP 426）：非回环对端绕过 transport v2 直接访问、`/v2/ws` 的 `tv` 不是 `2` 或仍用 v1 `enc=`，配对 `create` 仍提交 `clientPublicKey`（配对 v2）或缺少 `transportBinding: 1` / `deviceNameBinding: 1`，或 `/v2/sealed` 缺少 `X-Todex-Sealed-Revision: 2`；需升级客户端。 |
| `TRANSPORT_CRYPTO_FAILED` | `POST /v2/sealed` 的外层无法打开（HTTP 400），不含细节；WebSocket 上对应 close code `4400`。 |
| `CLIENT_UPGRADE_REQUIRED` | 历史已端到端加密，而客户端未声明 `historyEncryption=1`（HTTP 426）；需升级客户端。 |
| `HISTORY_KEY_REQUIRED` | 没有可读取新历史的接收方（HTTP 409）：新建会话、prompt、追加与重试等写入被拒绝，不写任何内容；在客户端登记设备密钥（`history.recipient.register`）后重试。 |
| `CLI_MANAGED_BY_IMAGE` | `POST /v2/providers/{provider}/upgrade` 或 `/install` 的目标 CLI 随容器镜像提供（`managedBy: "image"`，HTTP 409）；更新镜像，而不是在 TodeX 中升级。 |
| `HISTORY_READ_ONLY` | 会话为旧版未加密历史（`legacyPlaintext`），只读（HTTP 409）：可读取、归档与删除，不能再写入。 |
| `HISTORY_ACCESS_REVOKED` | 调用方设备已被封禁历史访问（HTTP 403）：除 `history.encryption.get` 外的 `history.*` 命令均拒绝，需另一台设备调用 `history.device.restore`。 |
| `STORAGE_LOW` | 数据目录所在磁盘可用空间低于 1 GiB，拒绝新 turn，HTTP 507；释放磁盘空间后重试。运行中的 turn 不受影响。 |
| `JOURNAL_FULL` | 已停用（history v3 起会话没有体积上限，服务端不再返回），保留供旧客户端映射。 |
| `EVENT_STREAM_LAGGED` | WebSocket 事件接收端落后，服务端正从 journal 补放；帧无顶层 `id`，`payload.conversationId` 标明会话。 |
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
