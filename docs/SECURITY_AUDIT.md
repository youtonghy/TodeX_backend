# TodeX Backend 安全与性能审计记录

## 范围与状态

审计范围是 `TodeX_backend` 当前 `main` 的 v2 REST/WebSocket、conversation folder store、Provider subprocess、ACP profile、MCP/Skill catalog、配对与传输加密路径。Codex Security Standard scan `00166df0-f462-4330-ba3b-8977d7351748` 已完成，coverage 为 complete，生成 canonical findings、coverage、manifest、Markdown 和 SARIF artifacts；结果为 0 个报告项。

以下结论来自源码、现有测试和本地命令，可由同一仓库状态复核；它们不替代宿主扫描。

## 已验证控制

- v2 HTTP 和 WebSocket 入口都要求 Bearer token，并以 authenticated tenant 作为 conversation owner；`get_owned`、replay、prompt、cancel、permission response 和 subscription 都执行 owner 校验。
- workspace 路径在创建 conversation、catalog 查询、workspace API、终端和旧 Codex adapter 路径统一 canonicalize，并拒绝 workspace root 外的目录、符号链接逃逸和不存在目录。
- ACP 的 command、args、env 只来自管理员配置的 profile；客户端只能提交 profile 名称。`TODEX_AGENTD_*` 不会被传入 Provider 子进程。
- Provider 子进程清空环境后只恢复允许的基础变量；stdout 单行上限 4 MiB，stderr 保留窗口 64 KiB，停止时处理 Unix process group。
- Provider 在信任读许可仍有效时完成子进程 spawn；撤销工作区信任取得写锁后会阻止后续启动，并取消已登记的活动 turn。信任状态只有在快照成功落盘后才更新内存。
- Pi 在工作区获得 TodeX 信任后固定使用 `--approve` 全自动运行；TodeX 不为 Pi 声明逐工具审批或 OS sandbox，`permissions` capability 保持 `false`。
- conversation event payload 上限 1 MiB，journal 上限 64 MiB，replay limit 上限 1000；v2 WebSocket 单消息上限 4 MiB、单连接订阅上限 128。
- MCP/Skill catalog 只读取配置，跳过 symlink，限制扫描深度、文件数和文件大小；响应不包含 command、args、env、URL 或凭据；现有测试验证输入文件未被修改。
- 旧 Codex session migration 是 copy-only、redacted、idempotent，并保留原始文件。

## 性能边界

- `cargo test --locked --all-targets --all-features -- --test-threads=1` 当前执行 177 个 backend 测试和 1 个非计费 E2E，耗时约 11 秒；5 个真实 provider 测试默认 ignored。
- Provider protocol、event journal、catalog 扫描和 WebSocket 都有显式内存/数量上限，避免单个请求无界增长。
- event replay 当前会在 64 MiB journal 上限内读取并校验完整 journal，再按 limit 返回窗口；这优先保证 sequence 连续性和尾部恢复。后续高吞吐部署可增加按 sequence 的索引或 checkpoint，需配套恢复和迁移测试，当前不应直接绕过完整校验。
- `cargo clippy --locked --all-targets --all-features` 可通过但报告 32 个既有 warning；`-D warnings` 尚未达到零 warning，主要是旧 Codex gateway/TUI 的大型 Result、参数数量和 enum 布局问题。

## 复核命令

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features -- --test-threads=1
cargo clippy --locked --all-targets --all-features
cargo test --test e2e_real_codex -- --list
cargo run --locked -- doctor providers --provider codex,pi --format json
```

真实 Provider 测试需要显式凭据和模型额度，默认 ignored：

```bash
TODEX_REAL_E2E=1 TODEX_REAL_ALLOW_BILLABLE=1 TODEX_REAL_PROVIDERS=codex,pi \
  cargo test --locked --test e2e_real_codex real_v2_provider_http_ws_roundtrip \
  -- --ignored --nocapture --test-threads=1
```

2026-08-25 本机验证结果：Pi 0.84.2 与 Claude Code 2.1.226 的组合 round-trip 通过；`pi-acp` adapter 的独立 ACP round-trip 通过。测试均经过 v2 HTTP 创建、v2 WebSocket 订阅、真实 prompt 和 Provider 事件返回。

2026-09-02 本机只读预检结果：Codex CLI 0.145.0 的登录、5 个模型、20 个命令通过；Pi 0.84.3 的模型凭据、11 个模型、59 个命令通过。预检不发送 prompt，报告 `billable: false`。本次真实 Codex/Pi smoke 因执行环境未授权向外部模型服务发送 sentinel 并产生费用而未运行；不能用只读预检替代其端到端结论。

## 剩余事项

1. 生产部署前验证反向代理仅开放 HTTPS/WSS，daemon 仅监听 loopback，token、audit 和 provider 登录目录使用最小文件权限。
2. 如果 replay 成为主要 CPU/IO 热点，先用生产规模 journal 做基准，再设计 checkpoint/index；不要以取消完整校验换取未经测量的优化。
