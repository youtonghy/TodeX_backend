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
| OpenSSH 客户端 | 无 | `TODEX_AGENTD_SSH_BIN`（`[agent] ssh_bin`） | `ssh`（`ssh-add` 取同目录或 PATH） |
| 默认 agent 名称 | 无 | `TODEX_AGENTD_DEFAULT_AGENT` | `codex` |
| 是否开启认证 | 无 | `TODEX_AGENTD_ENABLE_AUTH` | `true` |
| 历史保留天数 | `--history-retention-days` | `TODEX_AGENTD_HISTORY_RETENTION_DAYS` | 关闭 |

当前 HTTP 层没有实现 TLS 终止，配置 `enable_tls = true` 时服务会拒绝启动，避免产生“已经启用 TLS”的错误安全假设。生产环境应在可信反向代理终止 TLS，且不应直接暴露明文端口。v2 HTTP 和 WebSocket 都使用设备签名认证：每个请求携带 `x-todex-device-id`、`x-todex-auth-ts`、`x-todex-auth-nonce`、`x-todex-auth-sig` 四个 header（无法设置 header 的客户端使用等价 query 参数 `device_id`、`auth_ts`、`auth_nonce`、`auth_sig`），签名覆盖方法、路径、canonical query、时间戳、nonce 与 body 哈希，见 [设备验证](device-verification.md)。conversation 持久化 owner tenant，所有读取、订阅与变更入口都会校验 tenant。

认证策略是 fail-closed 的：`enable_auth = true` 时匿名 `/v2/ws` 握手直接被拒绝（401），不存在“先连上再限制命令”的匿名模式；`enable_auth = false` 的本地部署才会以本地信任模式接受匿名连接。没有可配置的共享凭据：设备只能通过配对流程登记（见 `POST /v2/device-pairing/*`）。

## v2 Conversation API

Provider 标识为 `acp`、`codex`、`pi`、`claude-code`、`grok-build`、`devin`、`opencode`（创建请求也接受 `grok` / `grok_build`、`devin-cli` / `devin_cli` 与 `open-code` / `open_code` 别名）。未指定时使用 `[agent].default_agent`，默认是 `codex`。ACP 必须使用后端 `config.toml` 中预配置的 `providerProfile`；客户端不能提交任意 command、args 或 env。

Provider 子进程只继承运行所需的基础系统环境；ACP 额外使用管理员在 profile 中明确配置的 env。Codex、Pi、Claude Code 和 Grok Build 应先由运行 daemon 的同一系统用户完成原生登录。Grok Build 也可通过白名单传入 `XAI_API_KEY`；daemon 不会启动浏览器/OIDC 交互认证。Grok 的工具授权、提问、计划审批和 MCP elicitation 会转换为 TodeX `permission.requested`，客户端按服务器提供的 `optionId` 和 `kind` 回复。Claude Code 的 `AskUserQuestion` 以 `kind: "user_input"` 的 `permission.requested` 发出（问题 id 为 `q0`、`q1`…，携带 `options`、`multiSelect` 与 `isOther: true`），客户端以 `answer` 回复 `{ "answers": { "q0": { "answers": ["..."] } } }`，后端按问题原文回填到 Claude 的 `updatedInput.answers`（多选以 `, ` 连接，可填自定义文本）；选择 `reject_once`（Skip）则拒绝该工具。Claude 的其他工具授权（`can_use_tool`）以 `kind: "tool"` 发出，details 保留原始请求，并把 `input.command` 与 `decision_reason` 提升为与 Codex 一致的顶层 `command` / `reason`；`decision_reason_type: "safetyCheck"` 表示 Claude Code 内置安全检查（如对可能为空的变量执行 `rm`），即使 `bypassPermissions`（完全访问）也会询问。Claude 的工具事件以 `block.category: "tool"`、`block.id` 为 `tool_use` id 发出，同一调用的开始、参数、进度与结果合并为一张卡片。Pi 只有全自动执行模式：工作区获得 TodeX 信任后以 `--approve` 启动，工具与项目扩展按 daemon 用户权限运行。Pi 没有通用逐工具审批或 OS sandbox，因此 `permissions` capability 为 `false`；extension UI 请求仍可转成交互事件，但不能把它等同于工具授权。

Devin 通过 `devin acp` 接入，每个对话对应一个常驻 ACP 进程。`devin acp` 自身有意不读取本地 CLI 登录态，daemon 会在每个 ACP 进程启动时执行 `authenticate`：优先以 `devin_api_key_env` 指定的环境变量值作为 `_meta.api_key` 无头认证（不会写入日志），未配置时默认回退读取 `devin auth login` 写入的 `~/.local/share/devin/credentials.toml` 中的 `windsurf_api_key`（`TODEX_AGENTD_DEVIN_CLI_CREDENTIALS=false` 可禁用），再否则按其声明的 `devin-browser` 方法走一次浏览器授权。权限模式只提供两档（Devin 没有“自动审批”中间档）：`ask`（手动审批）→`accept-edits`、`full-access`（完全访问）→`bypass`，plan 工作模式→`plan`；存量 `auto` 选择会降级为 `ask` 而不是报错。注意 Devin 原生的 `ask` 会话模式是只读问答，并不等价于“逐项审批”，因此未直接暴露。模型目录与 slash 命令通过真实 ACP 会话探测（探测会话随后调用 `session/delete` 清理，结果按工作区缓存约 5 分钟，避免客户端刷新反复触发认证）。Devin 支持会话内切换模型（`configure` live control）；reasoning effort 经 `configOptions.thought_level` 下发——Devin 只对当前模型暴露该选项且各模型档位集合不同，模型目录探测会逐个模型读取其 `supportedReasoningEfforts`（在同一 ACP 进程内并行开 8 个探测会话分摊，95 个模型约 4 秒；整轮上限 15 秒，超时未探到的模型档位留空，该结果只缓存约 1 分钟，之后的查询再补齐），不暴露该选项的模型（如 `claude-opus-4-6`）视为不支持调整思考强度。

OpenCode 通过 `opencode acp` 接入，每个对话对应一个常驻 ACP 进程（空闲 300 秒回收，daemon 级上限 32 个）。OpenCode 自己管理登录态（`opencode auth login` 写入其本地凭据存储），其声明的 `opencode-login` authMethod 仅指向该终端流程，daemon 因此不发送 ACP `authenticate`；未登录时模型调用错误原样上浮。权限模式提供 `ask`/`auto`/`full-access`：`ask` 把每个 `session/request_permission` 转成 TodeX `permission.requested` 逐条询问；`auto`/`full-access` 由 daemon 客户端侧自动应答（`auto` 选 `allow_once`，`full-access` 优先 `allow_always` 让 OpenCode 记住授权），自动批准同样记录 `permission.requested`/`permission.resolved`（`autoApproved: true`）事件。OpenCode 侧 `opencode.json` 中的 `permission` 配置（或白名单转发的 `OPENCODE_PERMISSION`）可让其原生策略先行放行。plan 工作模式经 `configOptions.mode` 映射到原生 `plan`，implement 显式重置为 `build`，避免上一轮 plan 残留。模型经 `configOptions.model` 选择（`provider/model` 形式），会话内支持 `configure` live control 切换；reasoning effort 经 `configOptions.effort` 下发——OpenCode 只在当前模型有 variant 时暴露该选项，模型目录探测会逐个模型读取其 `supportedReasoningEfforts`。OpenCode 声明 `loadSession` 与 `sessionCapabilities.resume`/`fork`，daemon 恢复对话时优先使用不重放历史的 `session/resume`（不支持时回退 `session/load`），会话分叉经 `session/fork` 完成；`session/list` 暂无消费方，未接入。Usage 来自 `usage_update`（`used`/`size`/`cost`）与 prompt 响应 `result.usage`（含 `cachedReadTokens`）。模型目录与 slash 命令（`available_commands_update`）通过同一次真实 ACP 会话探测获得，探测会话随后调用 `session/close` 清理；结果按工作区缓存约 5 分钟（有模型未应答时约 1 分钟），同时到达的模型与命令查询共用一次探测，发现总时限为 20 秒。OpenCode 的 skills 目录（`.opencode/skills`、`~/.config/opencode/skills`）进入 catalog；MCP 由 OpenCode 自己的 `opencode.json` 管理，不参与 TodeX catalog 解析。

会话分叉（`conversation.fork`）只在 Provider 真实支持时通过 `capabilities.controlActions` 的 `fork` 项暴露：Codex 走 app-server `thread/fork`，Pi 走 RPC `clone`，Grok Build 走 `_x.ai/session/fork`，OpenCode 走 ACP `session/fork`；Claude Code 由 daemon 复制 `~/.claude` 下的会话 transcript 并改写 `sessionId`（等价于 `claude --resume --fork-session`，不消耗额外汇合）。通用 ACP profile 与 Devin 的分叉能力按已安装 agent 动态探测——daemon 在其 `initialize` 响应中检查 `agentCapabilities.sessionCapabilities.fork`，结果缓存约 5 分钟；声明该能力的 agent 自动开放分叉（Devin CLI 目前未声明，升级后无需改动即可启用），未声明时 `fork` 不出现在 `controlActions` 中，`conversation.fork` 直接返回 unsupported 错误。

```http
GET /v2/providers
GET /v2/providers/versions
POST /v2/providers/{provider}/upgrade
POST /v2/providers/{provider}/install
GET /v2/providers/upgrades/{operationId}
GET /v2/providers/models?provider=codex&workspace=/home/user/projects/demo
GET /v2/providers/commands?conversationId={conversationId}
GET /v2/agent-providers?agent={codex|claude-code|grok-build|pi|opencode}
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
GET /v2/conversations/{conversationId}/events?afterSequence=0&limit=200&detail=full
GET /v2/conversations/{conversationId}/events?beforeSequence=500&limit=200&detail=summary
POST /v2/conversations/{conversationId}/prompt
POST /v2/conversations/{conversationId}/cancel
POST /v2/conversations/{conversationId}/runtime/stop
POST /v2/conversations/{conversationId}/permissions/{permissionId}
```

模型目录中的 `contextWindow` 来自 Provider 原生模型元数据；Provider 未公开该值时省略。客户端应结合实时 usage 事件显示上下文占用，不得用静态模型表猜测窗口大小。

`/v2/providers/models` 会实时向指定 Agent 查询模型目录，返回 `source` 与 `fetchedAt`。每个模型包含 `supportedReasoningEfforts`，并可通过 `defaultReasoningEffort` 声明后端当前默认强度。Codex 使用 app-server `model/list`，Pi 使用 RPC `get_available_models` 与 `get_state`，Claude Code 在配置了 `ANTHROPIC_BASE_URL` 时读取 `/v1/models`（与 Claude Code 一致：`ANTHROPIC_AUTH_TOKEN` 作 `Authorization: Bearer` 并优先于以 `x-api-key` 发送的 `ANTHROPIC_API_KEY`；非回环地址必须为 https，否则跳过 HTTP 发现并返回内置别名；单次请求 10 秒超时，不跟随重定向），Grok Build 从 ACP initialize 的原生模型状态读取。查询失败时客户端应保留上一次成功目录，并展示可恢复错误。

`GET /v2/providers/versions` 在 daemon 所在主机读取配置的六个内建 CLI，并从各自官方发布源查询最新版（Codex、Claude Code、OpenCode 查询 npm registry，Pi 查询 `pi.dev`，Devin 读取其自更新 manifest，Grok Build 执行 `grok update --check --json`）。配置的 binary 在 PATH 与官方安装器使用的用户目录（`~/.local/bin`、`~/.grok/bin`、`~/.opencode/bin`、`~/.pi/agent/bin`，Unix）中都找不到时返回 `status: "notInstalled"`、`installed: false`，不带 `error`，`installSupported` 表示可一键安装。结果会短时缓存，单个查询失败不会隐藏其他 CLI。ACP profile 作为外部管理项列出，不执行任意 profile 命令，也不提供升级。`POST /v2/providers/{provider}/upgrade` 仅接受 `codex`、`pi`、`claude-code`、`grok-build`、`devin`、`opencode` 固定标识（OpenCode 执行 `opencode upgrade`），返回异步 operation；客户端使用 operation 查询接口轮询。升级命令不经过 shell、不读取工作区，并在升级后重新调用同一个配置 binary 验证版本。任一 Agent turn、本地 Codex adapter、Provider 发现或另一升级正在启动/运行时会返回 `409 CONFLICT`；升级中的版本查询只返回已有缓存，不会再启动 CLI。CLI 升级在 loopback 部署中也必须携带有效设备签名，以免网页通过跨域请求改变宿主机工具链；已认证的尝试及最终结果会写入审计日志，初始审计记录无法落盘时不会启动升级。

`POST /v2/providers/{provider}/install` 与升级共用 operation、并发闸门与审计（operation 带 `action: "install" | "upgrade"`，审计事件带 `action`），CLI 已安装时返回 `409 CONFLICT`。安装执行各厂商文档中的固定安装脚本（`curl -fsSL <官方 URL> | sh/bash`，URL 为常量，不含请求内容）：Codex `chatgpt.com/codex/install.sh`（`CODEX_NON_INTERACTIVE=1`）、Pi `pi.dev/install.sh`（需要主机已有 Node.js ≥ 22）、Claude Code `claude.ai/install.sh`、Grok Build `x.ai/cli/install.sh`、Devin `cli.devin.ai/install.sh`、OpenCode `opencode.ai/install`。脚本在无控制终端的新会话中运行，交互提示取默认值；部分脚本会按厂商默认行为在 shell rc 中加入 PATH。安装成功与否以脚本结束后配置 binary 能否输出版本为准（Devin 脚本末尾的交互式 `devin setup` 在无终端时失败不影响结果）。Windows 暂不提供一键安装（`installSupported: false`）。

`GET /v2/providers/commands?provider=pi&workspace=/path` 会实时读取 Agent 命令目录。Pi 使用 RPC `get_commands` 返回扩展、Prompt Template 和 Skill；响应失败会作为 Provider 错误返回，成功结果中的 `sourceInfo` 会原样保留。Codex 返回与本机 CLI 版本同步的 TUI 命令适配目录。命令描述包含 `invocation`，客户端应据此选择原生 RPC、桌面动作或 Provider prompt，不要把所有 `/` 输入都当作普通 prompt。 已有会话应使用 `?conversationId=...`：后端先校验 owner，再从会话 manifest 确定 provider 与 workspace，忽略客户端同时提交的对应查询参数。Pi runtime 存在时由同一 worker 发送 `get_commands`，响应含 `catalogSource: "session"` 与 `runtimeId`；尚未启动时使用临时发现进程，返回 `catalogSource: "discovery"`。目录查询总时限为 8 秒。可验证的本地包会附带 `packageName` / `packageVersion`：版本取自实际资源附近的 `package.json`，显式加载的资源还会检查 manifest 的 Pi 入口；无法确认时省略，不从 npm spec 猜测。

`/v2/agent-providers` 实现 cc-switch 同款的多供应商/账户管理，当前覆盖 `codex`、`claude-code`、`grok-build`、`pi`、`opencode`。档案库持久化在 `$TODEX_AGENTD_DATA_DIR/agent-providers.json`（owner-only 0600），`settingsConfig` 是不透明的按 Agent 配置：Claude Code 为完整 `settings.json` 对象（`env` 内含 `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL` 等），Codex 为 `{auth, config}`（分别对应 `~/.codex/auth.json` 与 `config.toml` 文本），Grok Build 同为 `{auth, config}`（`$GROK_HOME/auth.json`——设置 `GROK_AUTH_PATH` 时取该路径——与 `$GROK_HOME/config.toml`，`GROK_HOME` 缺省为 `~/.grok`）：官方订阅档案携带 `grok login` 写入 `auth.json` 的会话（通常经 `import-live` 捕获），API 档案不带 `auth`，由 `[models].default` 指向带 `api_key`（可选 `base_url`、`api_backend`）的 `[model.<id>]`，Grok 会优先使用该密钥而非会话。Pi/OpenCode 为各自 `models.json`/`opencode.json` 中的 provider 节点。

激活（`activate`）改写该 Agent 的全局配置文件，对 TodeX 拉起的会话和终端里直接运行的 CLI 同时生效；运行中的会话不受影响。独占型 Agent（Claude Code、Codex、Grok Build）先把当前 live 配置回填进旧档案再写入新档案，外部编辑与凭据（含 Codex/Grok `auth.json`）因此被保留可恢复；`auth` 缺失的 Codex/Grok 档案仅在回填成功后删除 `auth.json`。Grok 会在后台刷新 `auth.json` 中的会话令牌：`matchesCurrent` 只比较登录账户（各 scope 的 `auth_mode`/`user_id`，API key scope 另比较密钥）与 `config.toml`，令牌刷新不算漂移；编辑或重新激活当前档案且 `auth` 未改动时，沿用 live 中同账户的新令牌，避免写回已轮换的旧令牌。叠加型 Agent（Pi、OpenCode）在保存时即把 provider 节点同步进 live 文件，`activate` 只移动原生默认选中（Pi 写 `settings.json` 的 `defaultProvider`/`defaultModel`；OpenCode 写顶层 `model`，可经请求体 `{"modelId": "..."}` 指定，缺省取首个声明模型）。删除叠加型档案同时移除 live 节点，并清理指向它的默认选中。

`GET /v2/agent-providers/{agent}/export` 导出该 Agent 的全部档案，用于在多台主机间同步：`{format: "todex.agent-providers", version: 1, agent, exportedAt, providers: [{id, name, settingsConfig, websiteUrl?, category?, notes?, icon?, iconColor?, sortIndex?}]}`，**密钥为明文**（其余接口一律打码），不含时间戳与当前选中。`POST /v2/agent-providers/{agent}/import` 接受同一文件：按 `id` 覆盖同名档案、新增其余档案，文件中没有的档案保留，当前选中不变（导入与当前档案同 id 的内容会像编辑一样重写 live 配置；叠加型 Agent 的节点同步进 live 文件）。整份文件先校验（格式与版本、`agent` 与路径一致、id 唯一、名称与大小限制、不得含 `__TODEX_MASKED__`、导入后不超过 100 个档案）再写入。两者与 CLI 升级一样要求有效设备签名，并写入 `agent_providers.transfer.audit` 审计事件。

`GET` 响应中按字段名模式（`api_key`/`token`/`secret`/`password`/`authorization`/`credential`/`bearer`，含 Codex/Grok `config` TOML 内的 `experimental_bearer_token`、`api_key`、Codex `http_headers` 与 Grok `extra_headers` 值及内联表中的同类字段，以及 Grok `auth.json` 各 scope 的 `key`）把密钥替换为 `__TODEX_MASKED__`；写回掩码值表示保留已存密钥，新档案不得携带掩码。`/{id}/models` 由后端携带档案凭据代理请求 `{base}/models`（Claude 为 `/v1/models`；Grok 取 `[models].default` 条目或其 `model_provider` 的 `base_url`/`api_key`，其次 `[endpoints].models_base_url` 与 `auth.json` 的 `xai::api_key`，仅有密钥时默认 `https://api.x.ai/v1`；订阅会话令牌不会被转发；不跟随重定向），客户端不经手真实密钥。`GET` 读取已存档案；`POST` 接受 `{"settingsConfig": {...}}` 用于保存前的预览拉取，掩码值按同 id 的已存档案或叠加型 live 节点还原。`import-live` 对独占型捕获当前 live 为新档案并记为当前；对叠加型把 live 中未托管节点按 `id` 收编。所有 live 写入为原子 owner-only 文件，叠加型编辑带内容 revision 校验——外部并发修改返回 `409 CONFLICT`，客户端应刷新后重试。

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
  ],
  "content": [
    { "type": "text", "text": "同时检查这个截图" },
    { "type": "localImage", "path": "screenshots/error.png" },
    { "type": "file", "path": "src/main.rs", "name": "main.rs" }
  ]
}
```

`skills` 可选。Backend 按 `resourceId` 读取 Skill；Codex 使用原生 `skill` input item，其他 Provider 使用受控文本注入，客户端不要拼接完整 Skill 文件。只带 Skill、不带 `text` 时允许发送。注入成功后会发出 `skill.injected` 事件；用户可见的 `message.created` 仍是原始输入。

`content` 可选，最多 16 项，支持 `text`、`localImage`、内联 `image`（`data` + `mimeType`）和 `file`。本地路径可相对 workspace，也可使用 workspace 内绝对路径；规范化后越界、符号链接逃逸和非普通文件都会拒绝。图片仅对 Codex 和 Pi 开放，允许 PNG/JPEG/GIF/WebP，解码后合计最多 10 MiB；不支持图片的 Provider 返回明确的 `UNSUPPORTED`。Codex 把文件映射为原生 mention，其他 Provider 使用 workspace 相对 `@` 引用。

每个 conversation 同时只允许一个 mutating turn；并发 `conversation.prompt` 返回 `409 CONFLICT`，不会排队。运行中要追加的消息改用后端追加队列（见下文 `conversation.queue.add`），它对所有 Provider 可用。daemon 重启按 journal（而非 manifest 状态）判断未完成 turn：仍打开的 turn 追加 `conversation.interrupted`（已知时携带该 turn 的 `turnId`），状态标记为 `interrupted`，不会通过重放 prompt 猜测恢复；journal 中已结束但 manifest 仍为 `running` 的会话只按 journal 修正状态，不追加事件。原生会话 ID 由 `provider-state.json` 保存，Provider 支持时下一 turn 使用原生 resume。

每个已开始的 turn 都以一个终态事件结束：driver panic 产生 `turn.failed`（`code: "PROVIDER_PANIC"`，其他任务异常为 `PROVIDER_TASK_CANCELLED`）；终态事件写入失败时按 100 ms / 500 ms / 2 s 重试，每次重试前先查 journal 避免重复，全部失败则 manifest 状态置为 `failed`，重启恢复再在 journal 中关闭该 turn。`[agent].provider_idle_timeout_minutes`（默认 60，`0` 关闭）内 Provider 没有任何输出或事件的 turn 会被取消（有待答权限请求时不计时触发），30 秒内未结束则强制中止，最终以 `turn.failed`（`code: "PROVIDER_IDLE_TIMEOUT"`）结束。Provider stdout 中非 JSON 行或超过 4 MiB 的行不再使 turn 失败：每个 turn 最多 20 条记为 `provider.event`（`{ "kind": "invalid_line", "preview" }`，preview 已脱敏且不超过 512 字节；或 `{ "kind": "oversized_line", "bytes" }`），其余只写日志。

journal 不设总容量上限：追加永不因体积被拒绝，进行中的 turn 总能写完，单个会话也不再有体积上限。只有数据目录所在磁盘可用空间低于 1 GiB 时，新 prompt（保存请求快照时）返回 `STORAGE_LOW`（HTTP 507），释放磁盘空间后重试即可；`JOURNAL_FULL` 不再产生。超过 1 MiB − 16 KiB 的事件 payload 在脱敏后截断而非拒绝：最大的字符串按 UTF-8 边界截断并追加 `…[truncated N bytes]`，对象 payload 顶层增加 `truncated`（该键已被占用时为 `_truncated`）映射，记录 JSON pointer → 原始字节数；仅截断字符串仍放不下时，payload 替换为 `{ "truncated": true, "originalBytes": N }` 加上较短的顶层标量字段。

事件回放支持 `detail=summary`（默认 `full`）：summary 模式把只产生折叠过程行的事件（工具调用、思考、状态、进度）的 `payload` 替换为 `{ "detailStub": true, ... }` 占位对象，保留分类、turn 与流身份所需的元数据，因此事件 sequence 与投影出的时间线条目身份保持不变；结果输出、审批、权限、队列、配置、压缩、subagent、memory、extension 及携带用量数据的事件始终完整返回。客户端展开过程组时用同一接口按 `afterSequence`/`limit` 以 `detail=full` 拉取对应序列区间。

HTTP 响应在请求携带 `Accept-Encoding: gzip` 且响应体不小于 1 KiB 时使用 gzip 压缩（图片、gRPC 与 SSE 除外）；压缩只作用于响应体，设备签名覆盖的请求方法、路径、查询与请求体不受影响，WebSocket 升级不压缩。

`beforeSequence=N` 提供反向翻页（与 `afterSequence` 互斥，优先生效）：返回 `sequence <= N` 的最后 `limit` 条（升序），`hasMore` 表示是否还有更早的事件，下一页游标为本页首条 `sequence - 1`。用于长对话自下向上懒加载：首屏用 manifest 的 `lastSequence` 拉取尾页，滚动到顶部再继续向前翻页。

两个方向的回放页（以及 WebSocket 订阅补放的每一页）都同时受 `limit`（上限 1000）与约 8 MiB journal 字节限制，先到先止，但每页至少返回一条事件；因此一页可能少于 `limit` 条，客户端应在 `hasMore` 为 `true` 时继续翻页，而不是按条数判断是否结束。

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

支持 `conversation.subscribe`、`conversation.unsubscribe`、`conversation.create`、`conversation.prompt`、`conversation.queue.add` / `remove` / `clear` / `resume` / `list`、`conversation.cancel`、`conversation.stop`、`conversation.runtime.stop`、`conversation.permission.respond`、`mcp.list`、`mcp.refresh`、`mcp.call`、`server.ping` 和 `history.*`（见下文「历史加密密钥」）。服务端返回 `server.result`、`server.error` 与按 conversation 隔离的 `conversation.event`。订阅会先 replay，再接续实时 sequence；实时广播滞后时会从最后已交付 sequence 自动补放到当前高水位，补放失败则发送错误并移除该订阅，避免静默缺事件。 `conversation.event` 外层新增 `delivery: "live" | "replay"`：首次订阅、序号缺口和广播滞后的补放均为 `replay`，事件日志 payload 不变。客户端只对明确为 `live` 且未处理的事件执行 toast 或编辑器填充等瞬时操作。

每条 socket 最多同时持有 128 个会话订阅，超出后 `conversation.subscribe` 返回 `INVALID_REQUEST`。`conversation.unsubscribe` 的 payload 为 `{ "conversationId": "..." }`，释放该订阅槽位并停止转发任务，幂等且返回 `{ "conversationId", "unsubscribed" }`；订阅任务因补放失败终止时发出的 `server.error` 在 `payload.conversationId` 中携带会话 ID，客户端可据此清理本地订阅记录并重订阅。删除会话（含过期清理）会关闭其广播通道，等同于终止该会话的全部订阅。

订阅补放在该订阅自己的任务中执行，不阻塞读循环：同一订阅的帧顺序不变（补放帧 → 带请求 `id` 的订阅结果 → 实时事件），但其他命令的应答可能穿插其间。每条连接最多同时进行 4 个补放，其余排队。已订阅（含补放进行中）的会话再次 `conversation.subscribe` 立即返回 `{ "conversationId", "subscribed": true, "alreadySubscribed": true }`；补放期间 `conversation.unsubscribe` 会先以 `server.result` `{ "conversationId", "subscribed": false, "cancelled": true }` 应答那条挂起的订阅请求。广播通道只在有订阅者时存在（发布不会创建通道，最后一个订阅者离开即回收），容量 256，滞后部分从 journal 补放；滞后时发出的 `EVENT_STREAM_LAGGED` `server.error` 帧没有顶层 `id`，`payload.conversationId` 标明所属会话。

发送侧有截止时间：单次 socket 发送超过 20 秒，或出站队列持续满 10 秒，连接即被关闭；关闭时最多等待 2 秒让发送任务排空，随后中止。

MCP 真实调用只走 Backend：客户端只发送 `resourceId`、`toolName` 和对象类型的 `arguments`。Catalog JSON 不含 command、URL 或凭据。调用前必须通过权限 broker，默认拒绝；仅 `allow_once` / `allow_always` 会放行。Backend 使用标准 MCP SDK 连接 stdio JSONL 或 Streamable HTTP transport，并对初始化、调用和关闭分别设置时限。

### 后端追加队列

Provider 能力中的 `backendQueue: true` 表示 daemon 为该会话保存追加消息（所有 Provider 都支持；`followUpQueue` 仍只表示 Provider 原生队列）。队列持久化在会话目录的 `queue.json`，保存完整请求（含附件与 Skill），fork 不复制，删除会话一并删除。每个会话最多 32 条、序列化后合计 32 MiB，超出返回 `RESOURCE_EXHAUSTED`。

- `conversation.queue.add`：payload 与 `conversation.prompt` 相同，另加 `itemId`（1–200 字节，省略时用命令 `id`）与可选 `front: true`（插到队首）。入队前按 prompt 的规则校验内容、路径与 Skill，不合法立即拒绝。会话空闲且队列为空时直接开始，结果为 `{ "conversationId", "itemId", "status": "started", "turnId" }`；否则入队，结果为 `{ ..., "status": "queued" }`。同一 `itemId` 已在队列中或已投递时幂等返回其状态，不会重复执行。
- `conversation.queue.remove`（`itemId` 必填，不存在返回 `NOT_FOUND`）、`conversation.queue.clear`、`conversation.queue.resume`、`conversation.queue.list`：payload 为 `{ "conversationId", "itemId"? }`，结果为 `{ "conversationId", "queue": <快照> }`。`resume` 解除暂停，会话空闲时立即开始队首。

快照为 `{ "items": [{ "id", "text", "status": "queued", "queuedAt", "contentCount", "skills" }], "paused", "pauseReason", "pauseMessage", "resumeAt" }`，不含内联图片数据。每次变化都会追加 `followups.updated` 事件（payload 即快照）；懒加载窗口可能不含最近一次该事件，客户端打开会话或重连后应调用 `conversation.queue.list` 取当前快照。

派发规则：turn 以 `turn.completed` 结束（或原生压缩结束）后，daemon 以队列项的 `itemId` 作为 `clientRequestId` 开始队首，成功后移出队列。`turn.failed` / `turn.cancelled` / `turn.interrupted` 使队列暂停（`pauseReason` 为 `turn_failed` / `turn_cancelled` / `turn_interrupted`）；队首无法开始时保留在队首并暂停（`start_failed`，`pauseMessage` 为原因）；daemon 重启后有待发项的队列暂停（`daemon_restarted`）。暂停期间空闲会话仍可直接 `conversation.prompt`，该 turn 完成后队列保持暂停。

额度续写：turn 失败且该 turn 最近一次 `quota.updated` 显示套餐窗口已用尽（Claude `status: "rejected"`，或某窗口 `usedPercent` ≥ 100），daemon 不按普通失败处理，而是在队首插入一条续写项（`id` 为 `rate-limit-continue-<turnId>`，沿用失败请求的模型、思考强度与权限设置，正文为固定英文续写指令，不重复附件与 skills）——队列为空时也插入，已有续写项时不重复插入——并以 `rate_limited` 暂停，`resumeAt` 为窗口重置时间（ISO 8601）。到点后 daemon 自行解除暂停并开始队首（按墙钟时间每 30 秒复核，机器休眠后也会补上），daemon 重启后继续等待。等待期间客户端可移除续写项或 `resume` 提前开始；若期间有 turn 以 `turn.completed` 结束（用户已手动继续），续写项被移除、暂停解除。

### 历史加密密钥（`history.*`）

规格见 [history-encryption.md](history-encryption.md) §3、§7。所有命令经 `/v2/ws` 发送，任一已注册设备都可调用；调用方身份取自连接的设备签名（`deviceId`），不读 payload。命令执行时设备已被吊销则返回 `UNAUTHENTICATED`。payload 拒绝未知字段，缺省 payload 视为 `{}`。

| 命令 | 请求 | 结果 |
| --- | --- | --- |
| `history.encryption.get` | `{}` | `{ "mode": "off"\|"e2e", "epoch", "recipients": [...], "myRid"?, "myAccess": "active"\|"unregistered"\|"revoked", "grants": [...], "revokedDevices": [{ "deviceId", "revokedAt" }] }`；被封禁设备也可调用 |
| `history.encryption.enable` / `disable` | `{}` | 同 get；`enable` 需至少一个未吊销的设备接收方，否则 `INVALID_REQUEST` |
| `history.recipient.register` | `{ "publicKey" }`（X-Wing 公钥 1216 字节，base64url） | `{ "rid" }`；同一公钥幂等；换钥吊销旧 `rid`；已吊销或属于其他接收方的公钥返回 `CONFLICT` |
| `history.recipient.revoke` | `{ "rid" }` | 同 get；未知 `rid` 为 `NOT_FOUND`，已吊销幂等；吊销设备当前的接收方同时封禁该设备 |
| `history.device.restore` | `{ "deviceId" }` | 同 get；解除封禁（不恢复旧公钥，设备须登记新公钥并重新申请授权）；设备未被封禁为 `NOT_FOUND` |
| `history.recovery.set` | `{ "publicKey" }` | `{ "rid" }`；替换（吊销）现有恢复接收方 |
| `history.grant.request` | `{}` | `{ "grantId" }`；同一接收方已有待办时返回原 `grantId` |
| `history.grant.list` | `{}` | `{ "grants": [{ "grantId", "rid", "deviceId", "requestedAt", "status", "updatedAt"?, "publicKey" }] }` |
| `history.grant.dismiss` | `{ "grantId" }` | `{}` |
| `history.keys.list` | `{ "conversationId"?, "cursor"?, "limit"? }`（1–500，默认 500） | `{ "items": [{ "conversationId", "kid" }], "nextCursor"? }` |
| `history.keys.wraps` | `{ "conversationId", "kids": [≤500], "rid"? }`（默认调用方 `rid`） | `{ "wraps": { "<kid>": { "rid", "kemCt", "wrapped" } } }`，无封装的 `kid` 省略 |
| `history.grant.fulfill` | `{ "grantId"?, "rid", "wraps": [{ "conversationId", "kid", "wrapped" }]（≤500）, "complete"? }` | `{ "added" }` |

`recipients[]` 项为 `{ "rid", "kind": "device"\|"recovery", "deviceId", "publicKey", "addedAt", "revokedAt" }`（含已吊销项）。授权状态 `status` 为 `pending`、`fulfilled`、`dismissed`、`revoked`（请求方接收方被吊销或换钥）。接收方集合变化（登记、换钥、吊销、恢复密钥替换、设备被吊销）使 `epoch` 加一；切换 mode 不变。

`history.grant.fulfill`：带 `grantId` 时目标 `rid` 必须是该待办授权的接收方（否则 `UNAUTHORIZED`），授权已结束为 `CONFLICT`；不带 `grantId`（恢复密钥导入）时目标只能是调用方自己的 `rid`。每个 `wrapped.rid` 必须等于 `rid`（`INVALID_REQUEST`），每个 `kid` 必须已存在（`NOT_FOUND`），会话须属于调用方；全部校验通过后才写入。目标已有封装的 `kid` 跳过，因此分批重试安全。最后一批带 `complete: true` 把授权标为 `fulfilled`。

设备封禁（规格见 [history-encryption.md](history-encryption.md) §3.4）：吊销某设备当前的接收方（`history.recipient.revoke`）或在 TUI 吊销设备时，该设备进入 `revokedDevices`，除 `history.encryption.get` 外的所有 `history.*` 命令返回 `HISTORY_ACCESS_REVOKED`（HTTP 403），重新配对同一设备身份也不解除，直到另一台未被封禁的设备调用 `history.device.restore`。被封禁设备仍可订阅、回放会话并收到密文。未开启设备认证时命令吊销不封禁。

状态推送：接收方、授权、模式或封禁列表每次持久化变化后，所有 `/v2/ws` 连接（含被封禁设备）都会收到全局事件 `{ "eventId", "type": "history.encryption.updated", "payload": { "epoch", "mode", "reason", "rid"?, "deviceId"?, "grantId"?, "conversationIds"? } }`，不含任何公钥或封装密钥；`reason` 为 `mode`、`recipient.registered`、`recipient.revoked`、`device.restored`、`device.revoked`（TUI 吊销，daemon 2 秒内推送）、`recovery.set`、`grant.requested`、`grant.dismissed`、`grant.progress`（`conversationIds` 为本批新增封装的会话）、`grant.fulfilled`，各字段含义见 history-encryption.md §7.1。客户端收到后重新读取 `history.encryption.get`。

`e2e` 下客户端须声明能解密历史：`/v2/ws` 握手 query 与 `GET /v2/conversations/{id}/events` query 带 `historyEncryption=1`（握手 query 受设备签名覆盖）。未声明的 `conversation.subscribe` 与 HTTP 回放返回 `CLIENT_UPGRADE_REQUIRED`（HTTP 426）。`/v2/version` 的 `historyEncryption` 字段给出后端支持的版本；旧后端对 `history.*` 返回 `UNSUPPORTED`。`history.encryption.enable` 同时触发已有明文历史的后台加密迁移。

加密历史的线上形状（细节见 [history-encryption.md](history-encryption.md) §5）：

- 事件 `payload` 只剩信封字段（`turnId`、`role`、`status` 等）加 `$enc`。活动分片与实时事件为事件级 `{ "v": 1, "kid", "c", "n", "s"?, "f"? }`：`detail=summary` 带 `s`（无单独摘要时带 `f`），`detail=full`、实时事件与缺口/滞后补放带 `f`，不会同时出现。封存分片为帧级 `{ "v": 1, "kid", "c", "n", "fr": { "s", "f", "i" } }`。
- HTTP 回放页顶层附 `frames: { "<帧 id>": { "kid", "stream", "counter", "c", "ct" } }`（只含所请求 detail 的帧，页内去重）；WebSocket 每条引用帧的 `conversation.event` 消息顶层各自附带它需要的 `frames`；一次订阅补放累计携带超过 16 MiB 帧数据时提前结束并返回 `hasMore: true`，其余走 HTTP 分页。
- 会话 manifest（列表、详情、创建与更新的结果）在加密时不含 `title`，改为 `titleEnc: { "kid", "ct" }`；fork 不继承加密标题（可在 `conversation.fork` 中另给标题）。迁移完成或加密下新建的会话带 `historyEncryptedAt`。
- `conversation.retry` payload 为 `{ "conversationId", "text"?, "content"?, "prompt"? }`：加密时客户端发回从用户 `message.created` 解密得到的 `retryRequest`（`text` 与 `content`），两者均按原请求的 MAC 校验后重放；旧式只带 `prompt` 仅适用于没有内联文本或图片的请求。缺失为 `INVALID_REQUEST`，与原请求不符为 `CONFLICT`；明文会话忽略这些字段。
- 加密时 `conversation.control` 以同一 `requestId` 重试：同一 control 返回原结果（daemon 重启后为 `null`），被拒绝的返回 `Control was rejected (<code>).`。
- 没有未吊销接收方时，新 prompt（含队列投递与重试）返回 `CONFLICT`，进行中的 turn 不受影响。

### Pi 扩展与常驻 runtime

Pi 在回合之间持续读取 RPC stdout，支持后台通知、消息、工具进度、压缩与扩展表单。`provider.runtime` 事件包含 `provider`、`runtimeId`、`scope: "session"`、`status: "ready" | "stopped"`，停止时附带 `reason`。回合成功、纯扩展命令，以及经 `abort` ACK、清队列 ACK 和 idle 状态确认的取消均保留进程；协议失步会关闭进程，不自动重放输入。每个 daemon 最多保留 32 个 Pi runtime，达到上限后拒绝新建，已有会话不会被静默淘汰；没有自动 idle 过期。

`conversation.runtime.stop` 的 payload 为 `{ "conversationId": "..." }`，与 HTTP `/runtime/stop` 等价，关闭 Pi 进程及待答表单，保留会话日志与原生 session 信息。`conversation.cancel` / `conversation.stop` 仅取消当前回合。撤销工作区信任、删除工作区或会话、会话过期与 daemon 关闭也会停止对应 runtime。daemon 重启时补记旧 runtime 的停止事件，并将未答表单标记为取消；session 表单不会把空闲会话改成运行中。

Pi capability 额外声明 `runtimeStop`、`sessionCommands`、`extensionMessages` 与 `extensionUi` 方法列表。标准 RPC 扩展输入 `select`、`confirm`、`input`、`editor` 继续经 permission broker 回答；`permission.requested` / `permission.resolved` 带 `runtimeId` 与 `scope: "session" | "turn"`，请求 details 中也保留上下文。session 表单可跨越回合，回合取消只关闭 turn 表单。

`extension.ui` 保留 Pi 标准字段，并附带 `provider`、`runtimeId` 和生命周期 `scope`：`notify` 使用 `message` / `notifyType`；`setStatus` 使用 `statusKey` / `statusText`；`setWidget` 使用 `widgetKey` / `widgetLines` / `widgetPlacement`；`setTitle` 使用 `title`；编辑器填充方法名为 `set_editor_text`，文本字段为 `text`。省略 `statusText` 或 `widgetLines` 表示清空。状态、widget、标题按键去重，并以 100 ms 合并高频更新，最终值与清空操作会保留；通知与编辑器填充直接交付。

原生 custom message 以 `extension.message` 交付，包含独立 `messageId` 与完整 `message`（`customType`、`content`、`display`、可选 `details`、`timestamp`）。客户端尊重 `display: false`，并以普通文本安全显示可见内容。原生后台消息与工具不携带旧 `turnId`；usage 保留既有 `scope: "message"` 用于计费去重。`ui.custom` 等依赖终端渲染的自定义界面没有通用 RPC 表示，客户端不能宣称支持或自动回退执行。

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
  events.NNNNNN.jsonl   # 已封存段，可选、可多个
  snapshot.json
  provider-state.json
```

journal 存储格式为 history v3（规格见 `docs/history-encryption.md` §4）：按段号排列的封存分片（`events.NNNNNN.seg` + `.idx`，或短暂存在、等待转换的 `events.NNNNNN.jsonl`），随后是唯一可写的 `events.jsonl`；`events.jsonl` 超过 64 MiB 后在下一次追加前被改名为下一个段号，后台任务把它转为 raw DEFLATE 分帧的 `.seg`。新写入的行是短键 v3 记录，旧 v2 行照常读取；WebSocket 与 HTTP 返回的事件形状与 v2 完全相同。sequence 从 1 连续递增；损坏按文件（明文）或按分片（`.seg`）处理，以 `journal.recordLost` 占位，不再坍缩成单个文件。封存时，已有同 id 终态记录的 `message.delta`、`tool.updated`、`subagent.updated` 换成 `journal.compacted` 标记（sequence、eventId、time 不变）。旧 v2 会话在空闲时于后台按体积从大到小迁移，原文件硬链接到会话目录下 `journal-v2-backup/` 保留 7 天；迁移完成的 manifest 带 `storageVersion: 3`，旧版 daemon 无法读取。

`events.jsonl` 是规范事件日志，sequence 从 1 连续递增；每次追加以 fsync 后的 journal 行为唯一提交点。manifest 缓存在内存中：创建、状态变化、元数据更新、强制置状态，以及会改变 manifest 的恢复时立即写 `manifest.json` 与 `snapshot.json`（与 journal 一致的 manifest 在启动恢复时不重写）；仅 `lastSequence`、`updatedAt` 变化时最多延迟 2 秒写 `manifest.json`，关闭时刷盘，崩溃后从 journal 重建。journal 修复：末条记录缺少换行时恢复阶段补上；明文文件中间损坏时先把受损文件备份为 `events.corrupt.<ts>.jsonl`，再原子重写这些文件，有效记录原样保留，每个丢失的 sequence 以 `journal.recordLost` 占位（payload `{ "reason": "corrupt", "runStart", "runLength", "backup" }`，同一段丢失共享 `runStart`/`runLength`，客户端可合并显示；普通追加无法伪造该事件类型）；末尾损坏仍隔离到备份文件后截断。`journal.compacted` 标记的 payload 为 `{ "reason": "compacted", "originalType", "runStart", "runLength" }`，同一剥离段共享 `runStart`/`runLength`；旧版本压缩写入的紧凑行 `{"sequence", "compacted": {...}}` 仍按原样读取。客户端把该类型按未知事件处理、不产生时间线条目。daemon 就绪后会在后台复制迁移旧 `$DATA_DIR/codex_gateway/sessions`；旧文件不修改，迁移可重复执行，并会去除 approval response 和常见 secret 字段。迁移失败会记录日志并在下次启动时重试，不阻塞 API 可用性。

Codex 的原生 `thread/tokenUsage/updated` 通知会在 Provider 边界规范化为 `usage.updated`，避免原生字段名与凭证脱敏规则冲突。`payload.usage.last` 是最近一次模型调用，`payload.usage.cumulative` 是当前原生 thread 的累计值；两者都使用 `total`、`input`、`output`、`cacheRead`、`cacheWrite` 和 `reasoningOutput` 数值字段，`payload.contextWindow` 是模型上下文窗口。Pi 的逐回复统计继续位于 assistant `message.completed` 的 `payload.message.usage`。与同一 turn（或运行时作用域）上一条同类型事件 payload 完全相同的 `usage.updated` / `quota.updated` 不再重复写入 journal，`/v2/providers/quota` 的快照仍每次刷新。

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

该端点与 `/health` 一样不需要认证，供 daemon 自检、客户端连接卡片轮询和客户端版本一致性检测（开发构建 `DEV0.0.0`/`0.0.0` 不参与比较）。

响应字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | Cargo 包名 |
| `version` | string | 构建时注入的应用版本；开发构建为 `DEV0.0.0` |
| `data_dir` | string | 当前数据目录 |
| `workspace_root` | string | 主 workspace 根目录（根列表第一项） |
| `workspace_roots` | string[] | 全部已配置的 workspace 根目录 |
| `historyEncryption` | number | 后端支持的历史加密版本（当前 `1`），见「历史加密密钥」 |

## Workspace 缓存同步与信任

工作区清单由后端持久化到 `$TODEX_AGENTD_DATA_DIR/workspaces.json`。手机和桌面端的本地存储只作为离线缓存；连接成功后会拉取当前身份的后端快照。工作区 ID 由后端根据规范化路径稳定生成，并与对话 manifest 的 `workspaceId` 共用，从而让不同设备恢复同一工作区内的对话。

后端会把 `workspace_roots` 作为移动端可用工作区的权限边界。`PUT /v2/workspaces`、`/v2/workspace/entries`、本地 Codex 启动和本地终端启动都会拒绝所有根目录之外的目录；目录必须存在且是目录。

```http
GET /v2/workspaces
PUT /v2/workspaces
GET /v2/workspaces/{workspaceId}/trust
PUT /v2/workspaces/{workspaceId}/trust
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

`PUT` 请求体使用同样的 `workspaces` 数组。后端会校验 `name`、`path`、路径存在性和根目录边界，按当前认证身份和规范化路径合并记录，并返回后端生成的稳定 ID。它不会接受客户端伪造的租户，也不会持久化设备本地的 `threadId` 和 `localAdapterState`。可选的 `icon`、`iconColor`、`ringStyle` 是客户端的侧栏展示偏好（图标键名、`#rrggbb` 颜色与状态环样式键名），后端只做透传持久化，不校验取值。

校验失败的记录（例如目录已被删除或位于 `workspace_roots` 之外）不再让整个请求报错：它们会被跳过并在响应的 `rejected` 数组中回报，每项包含客户端提交的 `id`、`name`、`path`、错误 `code`（如 `WORKSPACE_PATH_NOT_FOUND`）和 `message`；即使请求中所有记录都被拒绝也按同样方式返回 200。`GET` 和 `PUT` 响应都会重新校验已持久化的记录：目录后来被删除的存量记录同样只出现在 `rejected` 中，不再阻塞同步；客户端可据此把失效工作区灰显并在路径恢复后自动复原。加载已持久化的快照时同样跳过失效路径并记录告警，因此目录被删除后 daemon 仍可正常启动。要彻底移除一条失效工作区记录，需要显式调用 `DELETE /v2/workspaces/{workspaceId}`；该操作允许目录已不存在的记录被删除。

`PUT /v2/workspaces` 会自动信任当前 owner 下、已经通过 `workspace_roots` 边界校验且尚未做过信任决定的工作区。显式撤销会保留为拒绝决定，后续同步不会重新自动信任；未注册路径也不会因调用模型或执行接口而获得信任。

旧版信任文件没有保存撤销记录。首次升级时，旧版曾撤销的工作区与从未决定的工作区无法区分，都会在同步时获得信任；需要继续阻止的工作区应在升级后再次显式撤销。

`GET /v2/workspaces/{workspaceId}/trust` 返回当前状态；`PUT` 请求体为 `{ "trusted": true }` 或 `{ "trusted": false }`。信任记录同时绑定认证 owner 和规范化路径，独立保存在 `$DATA_DIR/workspace-trust.json`。Provider 模型/命令发现、prompt、MCP 调用、Git 写操作、本地终端和本地 Codex 启动前都必须通过信任检查；只读目录、文件预览与 Git 扫描仍受 `workspace_roots` 边界约束。Provider 启动许可持有信任读锁直至子进程完成 spawn；撤销先取得写锁，再取消该 owner 在工作区内已登记的活动 turn，因此不会漏过处于检查与启动之间的任务。删除工作区会先撤销信任并取消活动 turn，但不会删除已有对话历史。

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
      "createdAt": 1700000000000,
      "updatedAt": 1700000001000,
      "deletedAt": null
    }
  ],
  "updatedAt": 1700000002000
}
```

`PUT` 请求体使用同样的 `tasks` 数组，是合并而非全量替换：后端按 `(tenantId, id)` upsert，`updatedAt` 较新的记录胜出，同毫秒时采用提交方记录。`tenantId` 一律由后端覆盖为当前认证 owner，客户端伪造无效。

删除以墓碑同步：客户端把记录标记为非空的 `deletedAt`（Unix 毫秒）后再推送，`GET`/`PUT` 响应会继续携带墓碑直到其超过 30 天保留期被清理。其他端拉取到墓碑后同样隐藏该任务；比墓碑更旧的写入无法复活任务。接口不提供单独的 `DELETE`，避免抹掉墓碑后被旧端重新上传。

后端校验与前端一致：`title` 非空且不超过 200 字符，`description` 不超过 2000 字符，`status` 为 `planned`、`in-progress` 或 `done`，`dueDate` 为 `YYYY-MM-DD`；每个 owner 最多 500 条未删除任务，超出时整批拒绝（`INVALID_REQUEST`）。

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

### 文件 `@` 引用建议

```http
GET /v2/workspace/entries?cwd=/home/user/projects/demo&query=routes&limit=40
```

响应返回 `entries` 数组（`name`、`path`、`kind`）。`query` 为相对路径片段，支持递归匹配（跳过 `node_modules`、`target`、`.git` 等大目录）；以 `/` 结尾时按目录直接列出。

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

`authoredAt` 为 Unix 秒；`subject` 最多 512 bytes、`authorName` 最多 256 bytes。`pushed` 与状态摘要使用同一比较基准（上游，或无上游时的任意远端）：`false` 表示尚未推送；仓库没有远端，或未推送提交超过 10,000 个而无法判定时为 `null`。尚无首个提交的仓库返回空 `commits`。状态、diff 与提交历史都是只读接口，与扫描共享 2 个并发名额（排队超过 2 秒返回 `409 CONFLICT`），单次请求超过 10 秒返回 `GIT_COMMAND_TIMED_OUT`，并执行与状态摘要相同的元数据与仓库级可执行配置检查。

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

Git 进程不经过 shell，stdin 关闭并设置 `GIT_TERMINAL_PROMPT=0`；子进程只继承必要的基础运行环境，不继承 `TODEX_AGENTD_*` 或 `GIT_DIR`/`GIT_WORK_TREE` 等路径覆盖变量。所有命令显式关闭 fsmonitor 与 untracked cache，差异统计禁用 external diff/textconv。写操作会再次确认 worktree、git-dir 和 git-common-dir 全部位于 `workspace_roots` 内，拒绝写入相关 Git 元数据树中的符号链接和 object alternates，并使用 daemon 管理的空 hooks 目录、关闭 commit/push 签名和 `ext`/`file` transport；local 与 worktree 级配置都会展开 `include` 后检查，包含 clean/process filter、askpass、local credential helper、`core.sshCommand`、自定义 GPG program、自定义 remote helper 或仓库级 URL 重写时直接拒绝。仓库远端仅接受 HTTP(S)、SSH、Git URL 与 SCP-like SSH 写法；daemon 账户自己的全局 credential/SSH 配置仍可用于正常的非本地远端推送。同一 daemon 内的 Git 写操作串行执行，排队超过 2 秒返回 `409 CONFLICT`，单次写请求总计超过 120 秒返回 `GIT_PARTIAL_SUCCESS`，提醒客户端状态可能已改变、刷新后再决定后续动作。每条命令 15 秒超时；超时、输出超限或外层请求取消都会终止 Git 进程组并回收直接子进程。完整进程组清理依赖 Unix，因此非 Unix 平台的 Git 写 API 返回 `501 UNSUPPORTED`；Git 扫描仍可使用。

通过认证、schema 与路径校验并进入 Git 执行阶段的请求会尝试写入 `$TODEX_AGENTD_DATA_DIR/audit/audit.jsonl` 的 `git.audit` 事件，审计记录只包含动作、路径、结果码和输出长度，不记录提交说明。无效 token、无效 JSON/action 和路径校验失败发生在审计事件创建之前。Git 副作用完成后的审计 I/O 失败会记录 daemon 警告，但不会把已经成功的操作伪装成失败响应。

Git API 使用以下错误码（均为统一 JSON 错误 envelope 的 `code` 字段）：`GIT_UNAVAILABLE`、`GIT_REPOSITORY_NOT_FOUND`、`GIT_COMMAND_FAILED`、`GIT_PARTIAL_SUCCESS`、`GIT_COMMAND_TIMED_OUT`、`GIT_OUTPUT_LIMIT_EXCEEDED`、`GIT_PROCESS_ERROR`、`GIT_SCAN_LIMIT_EXCEEDED`、`UNSUPPORTED`。`GIT_PARTIAL_SUCCESS` 表示 `commit-push` 已创建本地提交但后续 push 失败，或写请求触及总时限而无法证明仓库/远端完全未变；客户端必须刷新仓库状态且不得自动重试提交。路径越界仍返回通用的 `WORKSPACE_PATH_OUTSIDE_ROOT`。

### 浏览器代理

```http
POST /v2/browser/fetch
{"url": "https://example.com/page"}
```

仅允许指向本后端 loopback 的 `http`/`https` URL；禁用系统代理，最多跟随 3 次且每一跳仍必须是 loopback。响应返回最终 `url`、实际 `status`、`contentType`、`body`（≤2 MiB）。

## SSH 主机与远程连接

管理**运行 todex-agentd 的机器**上的 SSH 主机、密钥与远程文件，并把选定主机开放给 Agent。所有 `/v2/ssh/*`、`/v2/ftp/*`、`/v2/remote/*` 接口都需要设备签名。TodeX 从不保存密码或密钥口令，从不写入 `~/.ssh/config`，私钥内容不会出现在任何响应或日志中。

### 主机清单

- 自动发现：递归解析 `~/.ssh/config`（跟随 `Include`，相对路径按 `~/.ssh` 解析、glob 按字典序展开），列出具体的 `Host` 别名；通配符、`!` 否定模式与 `Match` 块只参与连接参数解析，不单独列出。
- TodeX 自管主机与 FTP 站点保存在 `$DATA_DIR/ssh/store.json`，SSH 主机渲染为 `$DATA_DIR/ssh/hosts.conf`（均为 0600）。
- 所有 ssh 调用都使用生成的 `-F $DATA_DIR/ssh/ssh_config`，依次 `Include` 自管主机、`~/.ssh/config` 与系统 `ssh_config`，所以密钥、ssh-agent、ProxyJump、`known_hosts` 行为与用户 shell 中的 `ssh` 一致；有效参数由 `ssh -G` 解析（缓存 30 秒）。Unix 上同一主机的终端、SFTP 与 Agent 命令通过 `ControlMaster`（`$DATA_DIR/ssh/cm/%C`，空闲 5 分钟关闭）复用一次登录。

| 接口 | 说明 |
| --- | --- |
| `GET /v2/ssh/hosts` | `{ hosts: SshHost[], ftpSites: FtpSite[] }`。`SshHost`：`alias`、`source`（`sshConfig` 只读 / `managed`）、`sourcePath?`、`agentAccess`、`resolved?`（`hostName`、`user`、`port`、`proxyJump`、`identityFiles`）、`resolveError?`、`managed?`（可编辑定义）。 |
| `POST /v2/ssh/hosts` | 新建自管主机 `{ alias, hostName, user?, port?, identityFile?, proxyJump?, options?: [{key, value}] }`。别名已存在于 `~/.ssh/config` 或自管列表返回 409；拒绝换行、引号、以 `-` 开头的值以及 `Host`/`Match`/`Include`/`LocalCommand`/`PermitLocalCommand`/`KnownHostsCommand` 选项。 |
| `POST /v2/ssh/hosts/import` | `{ text }` 粘贴 `ssh_config` 片段，返回 `{ hosts, errors }`；通配符、`Match`、`Include` 只报告不导入。 |
| `PUT` / `DELETE /v2/ssh/hosts/{alias}` | 修改或删除自管主机（`~/.ssh/config` 中的主机只读）。 |
| `PUT /v2/ssh/hosts/{alias}/agent-access` | `{ enabled }`，默认关闭，见「Agent SSH 工具」。 |
| `POST /v2/ssh/hosts/{alias}/test` | 以 BatchMode、严格主机密钥校验执行一次 `exit 0`，返回 `{ ok, durationMs, failure?, detail? }`；`failure` 为 `hostKeyUnverified`、`hostKeyChanged`、`authenticationFailed`、`unreachable`、`timedOut`、`other`。 |
| `POST /v2/ssh/hosts/{alias}/disconnect` | `ssh -O exit` 关闭共享 master，返回 `{ disconnected }`。 |
| `POST /v2/ftp/sites`、`PUT` / `DELETE /v2/ftp/sites/{id}` | FTP 站点 `{ name, protocol: "ftp"|"ftps", host, port?（默认 21）, user?, initialDirectory? }`，不含密码。 |

### SSH 密钥

管理后端用户 `~/.ssh` 下的密钥。密钥解析、生成与加密均在进程内完成（不调用 `ssh-keygen`，口令不会出现在进程参数中）。

- `GET /v2/ssh/keys` → `{ sshDirectory, agentAvailable, keys: SshKey[] }`
  - 按内容识别私钥（OpenSSH / PEM），`<name>` 与 `<name>.pub` 配对；跳过 `config`、`known_hosts*`、`authorized_keys*`、目录、socket、超过 64 KiB 的文件及指向 `~/.ssh` 之外的符号链接。
  - `SshKey`：`name`、`privateKeyPath?`、`publicKeyPath?`、`algorithm`（如 `ssh-ed25519`）、`bits?`、`fingerprint?`（`SHA256:…`；仅无 `.pub` 的旧式 PEM 私钥缺省）、`comment?`、`encrypted?`、`loadedInAgent`、`publicKey?`（完整 OpenSSH 公钥行）、`usedBy`（`IdentityFile` 指向该密钥的主机别名）。
  - `agentAvailable`：Unix 上未设置 `SSH_AUTH_SOCK` 或 `ssh-add -L` 失败时为 `false`。
- `POST /v2/ssh/keys/import` `{ name, privateKey, publicKey?, passphrase? }` → `{ key }`。仅支持 OpenSSH 格式私钥（PEM 需先 `ssh-keygen -p -f <file>` 转换）；`publicKey` 若提供必须匹配；`passphrase` 只用于临时校验加密私钥，不会保存。
- `POST /v2/ssh/keys/generate` `{ name, algorithm?, comment?, passphrase? }` → `{ key }`。`algorithm`：`ed25519`（默认）、`rsa`（4096 位）、`ecdsa`（P-256）。
- 名称为 `[A-Za-z0-9._-]{1,64}`，不得以 `.`/`-` 开头、以 `.pub` 结尾或与 OpenSSH 文件同名。写入 `~/.ssh/<name>`（0600）与 `<name>.pub`（0644），`~/.ssh` 不存在时以 0700 创建；任一文件已存在返回 409，从不覆盖。请求体上限 64 KiB。

### SSH 终端

`terminal.start`（见「统一 WebSocket 命令面」）可带 `ssh: { host }`：后端在 PTY 中运行 `ssh -tt <alias>`，工作目录为用户主目录，环境变量清空后只保留白名单（不继承 `TODEX_AGENTD_*`）。此时忽略 `cwd` 与 `workspaceId`，也不做 workspace 根目录与信任检查，只要求别名存在于主机清单。主机密钥确认与密码提示直接在终端里完成；登录后的共享连接可被 SFTP 与 Agent 命令复用。`terminal.started` 事件额外带 `ssh: { host }`，其余 `terminal.*` 命令与事件不变。

### 远程文件（SFTP / FTP）

内存会话：空闲 5 分钟自动关闭，最多 16 个，daemon 重启即失效；密码只用于本次连接，从不保存。

- `GET /v2/remote/connections` → `{ connections: RemoteConnection[] }`（`id`、`kind`、`host?`、`siteId?`、`label`、`homeDirectory`、`openedAt`、`lastUsedAt`）。
- `POST /v2/remote/connections`，`{ kind: "sftp", host, password? }` 或 `{ kind: "ftp", siteId, password? }` → `{ connection }`。SFTP 走系统 ssh（`ssh -s … sftp`，复用 TodeX 的 `-F` 配置与共享连接）；带密码时通过一次性的 `SSH_ASKPASS` 辅助进程（`todex-agentd ssh-askpass`，只回答密码/口令提示）登录，且不会成为长期 master。FTP 使用被动模式，`ftps` 为显式 TLS（系统 webpki 根证书）。
- `DELETE /v2/remote/connections/{id}` → `{ closed: true }`。
- `GET …/{id}/entries?path=<绝对路径>`（省略时为 home）→ `{ path, parent?, entries, truncated }`，目录在前，最多 5000 条；条目含 `name`、`path`、`kind`（`file`/`directory`/`symlink`）、`sizeBytes?`、`modifiedAt?`、`permissions?`。
- `GET …/{id}/file?path=` 与 `/v2/workspace/file` 返回结构相同（文本 1 MiB / 图片 8 MiB）；`PUT …/{id}/file` `{ path, text, expectedText }`，内容不一致返回 409。SFTP 以临时文件加 rename 保存并保留权限，FTP 原地覆盖。
- `POST …/{id}/mkdir` `{ path }`、`…/rename` `{ from, to }`（目标存在返回 409）、`…/delete` `{ path }`（仅文件、符号链接或空目录，从不递归）→ `{ ok: true }`。
- 上传：`PUT …/{id}/upload?path=&offset=&overwrite=`，body 为原始字节（`application/octet-stream`，参与设备签名），每块 ≤ 8 MiB，单文件 ≤ 100 MiB；`offset=0` 新建（已存在且未带 `overwrite=true` 返回 409），其余块的 `offset` 必须等于远端当前大小，返回 `{ sizeBytes }`。中断的上传会留下部分文件。
- 下载：`GET …/{id}/download?path=` 流式返回原始字节（带 `Content-Length` 与 `Content-Disposition`），≤ 100 MiB。
- 路径必须是绝对 POSIX 路径，`.`/`..` 按字面规范化，禁止 NUL 与换行。

### Agent SSH 工具（MCP）

只要至少一台 SSH 主机开启了 Agent access，TodeX 就会在会话启动时为 Codex、Claude Code 与 ACP 系列（OpenCode/Devin/Grok/自定义 ACP）注入名为 `todex_ssh` 的 stdio MCP 服务器（`todex-agentd agent-mcp-bridge`，旧名 `ssh-mcp-bridge` 仍可用；环境变量 `TODEX_AGENT_MCP_URL`、`TODEX_AGENT_MCP_TOKEN`，桥也接受旧名 `TODEX_SSH_MCP_*`）；Claude Code 的配置写入 `$DATA_DIR/agent-mcp/` 下的 0600 文件，令牌不出现在命令行。同一会话注入多个 TodeX MCP 服务器时，它们共用一个会话令牌，各自走自己的端点；Codex 每个服务器一个 `mcp_servers.<name>` 覆盖，Claude Code 写入同一个配置文件（每个服务器带 `timeout`）并以逗号分隔列在 `--allowedTools`，ACP 列在同一个 `mcpServers` 数组。没有开启的主机时，Provider 启动参数与以前完全一致；Pi 与旧 codex_gateway 不注入。已运行的 Codex/ACP 进程要到下一次会话启动才会看到新开启的工具；关闭 Agent access 立即生效。

- 端点：`/internal/agent-mcp/ssh`（MCP Streamable HTTP）。不走设备签名，只接受回环地址、不带 `Origin` 头、携带会话级 `Authorization: Bearer <token>` 的请求；非回环或带 `Origin` 返回 403，令牌缺失或无效返回 401。令牌只存在内存，删除会话或重启 daemon 后失效。
- `ssh_list_hosts {}` → `{ hosts: [{ alias, hostName?, user?, port? }] }`，只列出开启了 Agent access 的主机。
- `ssh_exec { host, command, cwd?, stdin?, timeoutSec? }` → `{ stdout, stderr, exitCode, truncated, durationMs }`。**无需审批**；以 BatchMode、严格主机密钥校验运行；`cwd` 按 POSIX shell 引用后 `cd`；默认超时 60 秒，最长 600 秒；stdout/stderr 各最多 256 KiB，超出截断并置 `truncated: true`；同一主机最多 4 条并发命令。非零退出码是正常结果；ssh 失败（退出码 255）、超时或无法启动时返回 `isError: true`，带 `failure` 与处理提示。ACP 类 Agent 若自身处于“询问”权限模式，仍可能针对 MCP 工具弹出其原生权限请求。
- 会话事件（以 `execId` 关联同一次调用，并发调用互不混淆）：
  - `ssh.exec.started { execId, host, command, cwd?, turnId? }`
  - `ssh.exec.output { execId, stream: "stdout"|"stderr", data, turnId? }`：实时输出，按 100 ms 或 16 KiB 合并为一条事件，UTF-8 多字节字符不会被拆开；每次调用每个流最多记录 64 KiB（Agent 收到的结果仍最多 256 KiB），超出部分不再记录。
  - `ssh.exec.completed { execId, host, exitCode?, durationMs, failure?, truncated, outputTruncated, turnId? }`：`truncated` 指返回给 Agent 的结果被截断，`outputTruncated` 指事件中记录的输出被截断。
  - 桌面端与 Web 在当前会话中为每个 `execId` 打开一个只读侧边栏标签；其他客户端可忽略这些事件。

### Agent 浏览器（MCP `todex_desktop` 的 `browser_*` 工具）

浏览器由 daemon 在**自己所在的主机**上运行（`src/agent_browser`）：一个有界面的 Chrome for Testing（每个后端版本固定一个版本，首次使用时下载并按源码中的 SHA-256 校验），因此 `localhost` 指后端主机。客户端只观看（实时画面、截图、操作记录）并处理授权。桌面端不再作为执行端；旧桌面端发来的 `executor.register` 仍返回成功（`retired: true`）但不再使用，`tunnel.open` 一律以 `tunnel.close` 拒绝。

- 开关：`GET /v2/agent-desktop` → `{ enabled, computerEnabled, computer?, browser?: { available, reason?, host, chromium: { version, installed, downloading, progress?, error?, overridden } }, executors: [] }`；`PUT /v2/agent-desktop { enabled }`。默认关闭，存于 `$DATA_DIR/agent-desktop.json`；打开时后台开始下载 Chromium。打开后，会话启动时与 `todex_ssh` 一样注入 `todex_desktop`（端点 `/internal/agent-mcp/desktop`，同一会话令牌与鉴权规则）。已运行的 Provider 要到下次启动才看到新工具；关闭立即生效（调用返回错误，并撤销所有会话授权、关闭标签）。旧 daemon 上此路由为 404，客户端据此隐藏设置。
- 工具：`browser_open {url}`、`browser_navigate {url | action: back|forward|reload}`、`browser_snapshot {screenshot?}`（URL、标题与带 `[ref=eN]` 的无障碍树，可附 JPEG）、`browser_act {action: click|type|press|scroll|select|hover|wait, ref?, text?, key?, deltaY?, ms?}`、`browser_close {}`。顶层地址只允许 `localhost`、`127.0.0.0/8`、`[::1]` 的 http(s)（daemon 校验，桌面端对页面自身发起的导航同样拦截）；页面内部加载的外部子资源不受限。
- 运行方式：每个浏览器资料一个 Chromium 进程（同时最多 2 个，无标签 10 分钟后关闭），每个会话一个后台窗口（最多 4 个标签），打开时不抢焦点；CDP 在 macOS/Linux 走 `--remote-debugging-pipe`，Windows 走仅回环的随机端口；没有图形会话的 Linux 用 Xvfb（未安装时 `UNAVAILABLE`）。顶层导航与重定向只允许本机回环（`Fetch` 拦截，被拦截的导航取消并保留当前页），daemon 自身端口永远拒绝；页面内部的外部子资源不受限；下载与网站权限一律拒绝；JS 对话框自动取消；页面弹出的新窗口折叠进当前标签（本地地址在当前标签打开，其余拒绝）。Chromium 还没装好时工具返回 `BROWSER_INSTALLING`。`TODEX_AGENT_BROWSER_PATH` 可指向另一个 Chromium（开发用）；`todex-agentd browser install [--data-dir]` 或 `install.sh --with-browser` 可提前下载。
- 首次授权：会话第一次调用时发出 `permission.requested { kind: "desktop_browser", details: { host } }`，任意已配对设备都能回答，与权限模式无关；最多等待 5 分钟。授权只存在内存中，daemon 重启后重新询问。
- 敏感操作：向密码框输入时返回 `SENSITIVE_ACTION`，daemon 发出 `kind: "desktop_browser_action"` 的单次确认（任意设备可答），批准后带 `confirmed: true` 重试。Agent 不能自行传 `confirmed`。
- 浏览器资料：每个工作区（workspace id，没有时用路径）首次使用时自动创建独立资料（Cookie、存储、缓存），位于 `$DATA_DIR/agent-browser/profiles/<id>`，索引 `$DATA_DIR/agent-browser/profiles.json`。`GET /v2/agent-browser/profiles` → `{ profiles: [{ id, name, createdAt }], workspaces: { <workspace>: <profileId> } }`；`POST /v2/agent-browser/profiles { name }` 新建；`PUT /v2/agent-browser/profiles/{id} { name }` 改名；`DELETE /v2/agent-browser/profiles/{id}` 删除资料及其数据；`PUT /v2/agent-browser/workspaces { workspace, profileId }` 改用另一资料（该工作区已打开的标签会关闭，下次 `browser_open` 在新资料中打开）。`POST /v2/agent-browser/install` 开始下载 Chromium，返回设置对象。
- 实时画面：`/v2/ws` 上发 `{ id, type: "agentBrowser.watch", payload: { conversationId } }`（需能读取该会话）→ `server.result { watching: true }`，随后推送 `{ type: "agentBrowser.frame", payload: { conversationId, seq, mimeType: "image/jpeg", data, width, height } }`（CDP screencast，约 10–15 帧/秒，连接拥塞时丢弃旧帧只保留最新），会话没有标签时推送 `{ conversationId, closed: true }`；`agentBrowser.unwatch` 停止。每个连接最多 8 个观看；没有观看者时停止录制；画面不写入事件记录。轮询兜底：`GET /v2/conversations/{id}/agent-desktop/frame?capability=browser` → `{ mimeType, dataUrl }`（没有标签时 404）。
- `DELETE /v2/conversations/{id}/agent-desktop?capability=browser`：撤销会话授权并关闭其标签（客户端“停止”按钮）。`GET /v2/conversations/{id}/agent-shots/{shotId}` → `{ shotId, mimeType, dataUrl }`。截图存于 `$DATA_DIR/agent-desktop/shots/`，每会话保留最新 200 张，会话删除时一并删除。
- 会话事件：`desktop.browser.grant { status: "granted"|"revoked", deviceId?, deviceName?, reason? }`（`deviceId` 为 `host`，`deviceName` 为主机名）；`desktop.browser.action { actionId, tool, ok, summary, url?, title?, error?, shotId?, deviceId, deviceName }`。事件不含截图数据与输入的文字；`detail=summary` 回放也保留 `desktop.*` 事件的完整内容。
- 截图交给模型：2026-10 实测 Codex、Claude Code、Grok、Devin 都能把 `browser_snapshot` 的图片交给模型；OpenCode 当次改用 `curl` 读取页面源码，图片可能没有传给模型，此时只能依赖无障碍树文本。ACP 类 Agent 处于询问模式时，仍可能对这些 MCP 工具弹出原生权限请求。
- 设备限定的权限：任何 `permission.requested` 都可能带 `allowedDeviceIds`，客户端应在本机不在列表中时隐藏操作按钮。

### Computer Use（`todex_desktop` 的 `computer_*` 工具）

Computer Use 由 daemon 在**自己所在的主机**上执行（`src/computer`，基于 xa11y：macOS AX、Windows UI Automation、Linux AT-SPI），客户端只做预览和审批。桌面端执行端不再提供 `screen` 能力；旧桌面端登记的 `screen` 会被忽略（浏览器照常可用）。

- 开关：`PUT /v2/agent-desktop` 也接受 `{ computerEnabled }`（字段都可选，至少给一个），默认关闭，只有 `enabled` 也打开时生效。打开后工具列表多出 `computer_observe`、`computer_act`、`computer_done`（已运行的 Provider 要到下次启动才看到；关闭立即生效，并结束屏幕会话）。
- 主机状态：`GET /v2/agent-desktop` 另返回 `computer { supported, available, reason?, host, platform, permissions: { screen, accessibility } }`。`available` 要求系统支持、权限齐全、且 daemon 运行在主机的桌面会话里（能弹出确认框）。`POST /v2/agent-desktop/computer/permissions` 在主机上弹出系统授权提示（macOS：屏幕录制、辅助功能），返回同样的设置对象。macOS 上 `serve`/`tui`/`daemon-run` 启动时会以自身身份重新执行（pid 不变），权限授给 `todex-agentd` 本身而不是启动它的终端；授权按“路径 + 签名身份”记录，所以发布版必须用固定证书签名、安装路径保持不变。
- 首次授权：只能由主机前的人确认——daemon 在主机上弹出原生对话框（macOS NSAlert；300 秒无人确认按拒绝处理），其他设备无法代为批准。事件 `desktop.computer.grant { status: "requested" }`（客户端据此显示“等待主机确认”），随后 `granted` 或 `declined`；用户撤销或在主机上按“停止”时为 `revoked`。无人可确认时工具返回 `UNAVAILABLE`。
- 屏幕租约：主机同一时间只服务一个会话，其他会话调用返回 `SCREEN_BUSY`；`computer_done`、停止、撤销或闲置 120 秒后释放（每 15 秒清理一次）。租约期间主机显示“Agent 正在控制”的浮条（不进截图），浮条“停止”或 ⌘⇧⎋（macOS）结束当前会话并撤销其授权。
- 按应用批准：本会话首次操作某应用时 daemon 发 `permission.requested { kind: "desktop_computer_app", details: { bundleId, app, action } }`（任意设备可答），批准后记住；向密码框输入时发 `kind: "desktop_computer_action"` 单次确认。Agent 无法自行跳过。禁止操作的应用（TodeX 自身、系统认证窗口、凭据存储、系统设置、密码管理器）返回 `TARGET_BLOCKED`。
- `computer_observe { app?, window?, display?, screenshot?=true }` → 前台（或指定）应用、窗口列表（`id` 来自本次观察）、带 `[ref=eN]` 的无障碍树、显示器、窗口（或整屏）截图；`computer_act { action: click|double_click|right_click|hover|drag|scroll|type|key|wait|open_app|focus_window, ref?, x?, y?, toX?, toY?, text?, keys?, app?, window?, deltaX?, deltaY?, ms? }`。有 `ref` 时尽量在后台送达元素（`path: "background"`，不动指针、不抢焦点；文字直接插入，不受输入法影响），只给坐标时移动指针（`path: "pointer"`，用户正在操作时返回 `USER_ACTIVE`）；无法后台完成的按键/输入会先激活目标应用（`path: "keyboard"`）。`keys` 里的 `cmd` 在 macOS 是 ⌘，其他平台是 Ctrl。
- 实时画面：`GET /v2/conversations/{id}/agent-desktop/frame` → `{ mimeType, dataUrl }`，仅返回给当前持有屏幕租约的会话（否则 404）；最多每 300ms 截一次（宽 960、JPEG），多个观看者共享。客户端在预览可见时轮询，没有观看者时不截图。
- 会话事件：`desktop.computer.session { status: "started"|"ended", deviceId?, deviceName?, reason? }`（reason：`done`、`idle`、`user`、`revoked`；`deviceId` 为 `host`，`deviceName` 为主机名）；`desktop.computer.action { actionId, tool, ok, summary, app?, windowTitle?, path?, error?, shotId?, deviceId, deviceName }`。事件不含截图数据与输入的文字。
- `DELETE /v2/conversations/{id}/agent-desktop?capability=screen|browser` 只撤销其中一项；不带参数撤销两项。
- 受控主机（daemon 所在电脑，`GET /v2/agent-desktop` 返回的 `computer { supported, available, reason, platform, permissions }` 反映这些条件）：macOS 14+（需屏幕录制与辅助功能授权；浮条 + ⌘⇧⎋ 停止）；Windows 需在已登录用户的交互式桌面运行（作为服务在 session 0 或桌面锁定时不可用；无需授权；置顶状态窗 + Ctrl+Alt+Shift+Esc 停止；坐标为物理像素，进程按 Per-Monitor-V2 DPI 感知；应用标识为小写可执行文件名，如 `notepad.exe`）；Linux 支持 X11 会话与 KDE Plasma 6.6+ 的 Wayland 会话（其他 Wayland 桌面及更早的 Plasma 返回 `supported: false` 并说明原因；辅助功能指 AT-SPI 的 `org.a11y.Status.IsEnabled`，请求授权时由 daemon 打开；状态与授权确认用桌面通知的“停止”/“允许”/“拒绝”按钮，无通知服务时退回 `kdialog`/`zenity`；X11 没有全局停止快捷键；应用标识为小写可执行文件名；daemon 不链接 libxkbcommon 等图形库，无图形环境的服务器也能正常启动）。Windows 与 Linux 上无元素的 `type` 与 `key` 会先把目标应用切到前台再模拟按键。
- KDE Plasma Wayland（`src/computer/platform/kde_wayland.rs`）：输入走 RemoteDesktop 门户（首次由 KDE 弹窗征得主机同意，恢复令牌存于 `$XDG_STATE_HOME/todex/remote-desktop.token`，0600；会话随屏幕租约打开与关闭），截图走 KWin `ScreenShot2`（daemon 安装隐藏的 `~/.local/share/applications/com.unbaked0692.todex.agentd.desktop` 声明授权，未授权时退回 Screenshot 门户整屏截取），窗口、显示器与激活靠临时 KWin 脚本，空闲时间来自 `ext-idle-notify-v1`（2 秒粒度）。Wayland 原生窗口的 AT-SPI 坐标是窗口内坐标，daemon 按 KWin 窗口位置换算为全局逻辑坐标。Plasma 6.8 之前，向已知非密码框输入非 ASCII 文字或向 Chromium 系应用输入时，改用 Klipper 剪贴板 + Ctrl+V（约 400ms 后恢复原文字剪贴板，Klipper 历史会留下该文字）。停止快捷键 Ctrl+Alt+Shift+Esc 经 KGlobalAccel 注册（冲突时没有快捷键）。`POST .../computer/permissions` 安装截图授权、打开 AT-SPI 并提前弹出远程控制同意框。daemon 不会写 KDE 的权限库来跳过同意框（只有 CI 的隔离总线这样做）。
- KDE Wayland 真机验证清单（CI 任务 `kde-wayland` 只在无头 KWin 中跑过逻辑，以下需在 Plasma 6.6、6.7、6.8 实机逐项确认）：首次远程控制同意框出现且重启后靠恢复令牌免弹窗；6.6/6.7 上 `ScreenShot2` 授权生效（不再退回门户）；多显示器（含不同缩放）上按坐标与按 ref 点击落点正确；Firefox、Chromium、GTK4 应用的元素坐标与截图对齐；滚动方向（`deltaY` 为正向下）；fcitx5 下中日韩文字输入；Ctrl+Alt+Shift+Esc 停止会话；通知“停止”/“允许”/“拒绝”可用。

## WebSocket 协议

客户端发送文本帧，内容必须是 JSON。二进制帧会被忽略。默认仍支持明文 JSON；如果 WebSocket URL 带上加密握手参数，业务 JSON 会被包装在 `todex.crypto.v1` 加密帧中。

连接示例：

握手认证与 HTTP 一致：四个 `x-todex-*` header，或等价 query 参数（Electron 原生 WebSocket、浏览器等无法设置 header 的客户端）：`ws://127.0.0.1:7345/v2/ws?device_id=<id>&auth_ts=<unix>&auth_nonce=<b64url>&auth_sig=<b64url>`。签名按 `GET /v2/ws`、完整 canonical query 与空 body 计算；query 中的传输加密参数同样被签名覆盖，因此握手材料无法被中间人替换。注意 query 凭据可能进入反向代理日志，生产环境优先使用 header。

TUI 配对二维码只携带后端地址、当前首选加密方式和服务端公钥，不携带任何访问凭据——设备身份一律走 `/v2/device-pairing` 配对流程登记。客户端每次连接必须生成新的 X25519 client key 或 ML-KEM ciphertext，服务端会拒绝当前进程生命周期内重复使用的握手材料。进程内最多登记 65,536 份已使用握手材料；达到上限后新加密握手会失败关闭，需要重启 daemon 清空登记表。

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

设备签名认证当前映射到租户 `local`，认证主体是 `deviceId`（由设备公钥派生）。请求 payload 中的 `tenantId` 必须与认证上下文匹配。鉴权结果会写入 `$TODEX_AGENTD_DATA_DIR/audit/audit.jsonl` 并广播 `codex.audit` 事件，审计记录中的 `token_id`/`principal_id` 即 `deviceId`。

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
2. **本地控制命令**（原 `/v1/ws` 能力）：`terminal.*`（`terminal.start` 可带 `ssh: { host }`，见「SSH 终端」）、`codex.local.*`、`codex.gateway.control`、`codex.mcp.*`、`codex.cloudTask.*`。应答与事件通过按连接隔离的 ServerEvent 流返回（见「事件」一节），连接只会收到自己触碰过的 Codex session / 终端的事件。

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
| `history.encryption.updated` | 历史加密密钥状态变化（见「历史加密密钥」）。 |
| `error` | 通用后端错误事件。 |

## 错误码

| code | 说明 |
| --- | --- |
| `INVALID_REQUEST` | JSON 格式、字段或消息类型不符合当前协议。 |
| `UNAUTHENTICATED` | `enable_auth` 开启时未提供有效设备签名（header 或 query 参数），或设备未注册/已吊销、时间戳超窗、nonce 重放。 |
| `UNAUTHORIZED` | tenant 与认证上下文不匹配。 |
| `UNSUPPORTED` | 请求能力不在当前后端支持范围。 |
| `REMOTE_AUTH_FAILED` | SFTP/FTP 登录失败（HTTP 403）：提供密码，或先在终端登录以复用共享连接。 |
| `REMOTE_HOST_KEY_UNVERIFIED` | 远程主机密钥未确认或已变化（HTTP 403）：先在 TodeX 终端中连接一次。 |
| `REMOTE_UNREACHABLE` | 远程主机不可达或连接中断（HTTP 502），会话随之关闭。 |
| `REMOTE_PERMISSION_DENIED` | 远程服务器拒绝该文件操作（HTTP 403）；与 TodeX 设备认证无关。 |
| `REMOTE_OPERATION_FAILED` | 其他远程文件操作失败（HTTP 422）。 |
| `CLIENT_UPGRADE_REQUIRED` | 历史已端到端加密，而客户端未声明 `historyEncryption=1`（HTTP 426）；需升级客户端。 |
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
