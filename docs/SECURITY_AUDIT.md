# TodeX Backend 安全与性能审计记录

## 范围与状态

审计范围是 `TodeX_backend` 当前 `main` 的 v2 REST/WebSocket、conversation folder store、Provider subprocess、ACP profile、MCP/Skill catalog、配对与传输加密路径。宿主 Codex Security Standard scan `6231ccc4-9aa3-436c-984b-e3ead6a0769d` 已创建，但当前环境没有暴露要求的 `security_scan` preflight capability，扫描停留在 preflight，不能作为已完成的独立扫描报告。

以下结论来自源码、现有测试和本地命令，可由同一仓库状态复核；它们不替代宿主扫描。

## 已验证控制

- v2 HTTP 和 WebSocket 入口都要求 Bearer token，并以 authenticated tenant 作为 conversation owner；`get_owned`、replay、prompt、cancel、permission response 和 subscription 都执行 owner 校验。
- workspace 路径在创建 conversation、catalog 查询、workspace API、终端和旧 Codex adapter 路径统一 canonicalize，并拒绝 workspace root 外的目录、符号链接逃逸和不存在目录。
- ACP 的 command、args、env 只来自管理员配置的 profile；客户端只能提交 profile 名称。`TODEX_AGENTD_*` 不会被传入 Provider 子进程。
- Provider 子进程清空环境后只恢复允许的基础变量；stdout 单行上限 4 MiB，stderr 保留窗口 64 KiB，停止时处理 Unix process group。
- conversation event payload 上限 1 MiB，journal 上限 64 MiB，replay limit 上限 1000；v2 WebSocket 单消息上限 4 MiB、单连接订阅上限 128。
- MCP/Skill catalog 只读取配置，跳过 symlink，限制扫描深度、文件数和文件大小；响应不包含 command、args、env、URL 或凭据；现有测试验证输入文件未被修改。
- 旧 Codex session migration 是 copy-only、redacted、idempotent，并保留原始文件。

## 性能边界

- `cargo test` 当前执行 122 个 backend 测试，耗时约 6 秒；`cargo test --no-run` 可独立验证所有 test target 编译。
- Provider protocol、event journal、catalog 扫描和 WebSocket 都有显式内存/数量上限，避免单个请求无界增长。
- event replay 当前会在 64 MiB journal 上限内读取并校验完整 journal，再按 limit 返回窗口；这优先保证 sequence 连续性和尾部恢复。后续高吞吐部署可增加按 sequence 的索引或 checkpoint，需配套恢复和迁移测试，当前不应直接绕过完整校验。
- `cargo clippy --all-targets` 可通过但报告 24/25 个既有 warning；`-D warnings` 尚未达到零 warning，主要是旧 Codex gateway/TUI 的大型 Result、参数数量和 enum 布局问题。

## 复核命令

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets
cargo test --test e2e_real_codex -- --list
```

真实 Provider 测试需要显式凭据和模型额度，默认 ignored：

```bash
TODEX_REAL_E2E=1 TODEX_REAL_PROVIDERS=pi,claude-code \
  cargo test --test e2e_real_codex real_v2_provider_http_ws_roundtrip -- --ignored --nocapture
```

2026-08-25 本机验证结果：Pi 0.84.2 与 Claude Code 2.1.226 的组合 round-trip 通过；`pi-acp` adapter 的独立 ACP round-trip 通过。测试均经过 v2 HTTP 创建、v2 WebSocket 订阅、真实 prompt 和 Provider 事件返回。

## 剩余事项

1. 在提供 `security_scan` preflight capability 的宿主重新加入现有 scan，完成单次 Standard scan、记录 canonical findings/coverage 并生成报告。
2. 生产部署前验证反向代理仅开放 HTTPS/WSS，daemon 仅监听 loopback，token、audit 和 provider 登录目录使用最小文件权限。
3. 如果 replay 成为主要 CPU/IO 热点，先用生产规模 journal 做基准，再设计 checkpoint/index；不要以取消完整校验换取未经测量的优化。
