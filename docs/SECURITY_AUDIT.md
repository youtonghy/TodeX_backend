# TodeX Backend 安全与性能审计记录

## 范围与状态

审计范围是 `TodeX_backend` 当前 `main` 的 v2 REST/WebSocket、conversation folder store、Provider subprocess、ACP profile、MCP/Skill catalog、配对与传输加密路径。Codex Security Standard scan `00166df0-f462-4330-ba3b-8977d7351748` 已完成，coverage 为 complete，生成 canonical findings、coverage、manifest、Markdown 和 SARIF artifacts；结果为 0 个报告项。

以下结论来自源码、现有测试和本地命令，可由同一仓库状态复核；它们不替代宿主扫描。

## 已验证控制

- v2 HTTP 和 WebSocket 入口都要求已注册设备的 Ed25519 请求签名（时间戳窗口 + 一次性 nonce 防重放），并以 device principal 作为 conversation owner；`get_owned`、replay、prompt、cancel、permission response 和 subscription 都执行 owner 校验。设备注册表见 `devices.json`，吊销入口在 TUI `d` 面板与 `x` 重置菜单。
- 历史加密密钥（[history-encryption.md](history-encryption.md)）：后端只保存接收方 X-Wing 公钥（`history/recipients.json`，0600、目录 0700、原子替换并 fsync，读取时校验属主、权限、大小与每条记录的 `rid = SHA-256(pk)[0..16]`）和按会话的封装 DEK（`conversations/<id>/keyring.json`，0600，按会话加锁）。DEK 只在内存中（`SegmentKey` 释放即清零），先写入并 fsync keyring 才能使用；设备私钥与恢复种子从不上传。`history/fingerprint.key`（32 字节随机，0600，只创建不覆盖）是 `textMac` / `requestFingerprint` 的 HMAC 密钥，属于后端秘密，泄露只暴露这些 MAC 与已知明文的对应。设备吊销（TUI 单台或全部）同时吊销其接收方、推进 `epoch` 并把设备加入 `revokedDevices`；被封禁设备（含以同一身份重新配对者）只能调用 `history.encryption.get`，其余 `history.*` 返回 `HISTORY_ACCESS_REVOKED`，须由另一台未被封禁的设备 `history.device.restore`，且旧公钥不能复用；状态变化推送 `history.encryption.updated` 只含 id、epoch 与 mode，不含密钥材料；钩子失败时 daemon 在下一次访问历史密钥时把 `devices.json` 中已不存在的设备接收方吊销，`history.*` 命令执行时也重新校验设备仍在注册表中。授权履约只接受目标 `rid` 与待办授权一致（或恢复导入时为调用方自身）的封装，后端从不接触明文 DEK。已吊销的公钥不能再次登记。
- 历史端到端加密的存储与线上路径：`e2e` 下每条记录在提交时以当前 DEK 加密（ChaCha20-Poly1305，nonce 为 stream + sequence，AAD 绑定会话、`kid`、stream 与 counter），journal 只留信封字段明文；实时推送、回放与 fork 拷贝都原样转发写入时的密文，回放从不解密、也不对密文运行摘要。封存时仅在 DEK 仍在内存期间解密重打包，帧 counter 为「段号 << 32 | 帧序号」以避免同一 `kid` 跨分片重复 nonce，封存后 DEK 清零；重启后丢失的 DEK 只让对应记录以事件级密文原样封存。标题每次用一次性 DEK（counter 0 不复用）；`last-request.json` 不含提示原文与内联内容（只留 HMAC）；去重、重试校验与 control 幂等只比较 `fingerprint.key` 下的 HMAC，control 结果只缓存在内存。没有可用接收方时拒绝新 prompt，进行中的 turn 只沿用内存中已有 DEK，绝不回落为明文；provider payload 自带的 `$enc` 键改名保存，不能伪装成密文。测试以测试种子扮演客户端，逐字节检查会话目录不含已知明文。
- 已知残留明文（文档化、不在保护范围内）：后端运行时内存中的明文、provider 自己的 transcript、`queue.json`、`provider-state.json`、开启加密前的 `journal-v2-backup/`（7 天后删除）、`events.corrupt.*` 打捞副本以及文件系统快照（Time Machine / APFS）；开启加密时 daemon 记录警告，客户端负责提示。迁移期间后端会再次读到旧明文。
- workspace 路径在创建 conversation、catalog 查询、workspace API、终端和旧 Codex adapter 路径统一 canonicalize，并拒绝 workspace root 外的目录、符号链接逃逸和不存在目录。
- ACP 的 command、args、env 只来自管理员配置的 profile；客户端只能提交 profile 名称。`TODEX_AGENTD_*` 不会被传入 Provider 子进程。
- Provider 子进程清空环境后只恢复允许的基础变量；stdout 单行上限 4 MiB（超长行不缓冲、丢弃到下一个换行，与非 JSON 行一样每 turn 最多 20 条记为 `provider.event`，preview 脱敏且不超过 512 字节，不再使 turn 失败），stderr 保留窗口 64 KiB，停止时处理 Unix process group。
- Unix 上已启动的 Provider 以 pid、pgid 与启动时间记录在 `<data_dir>/provider_processes.json`（0600、原子写入，另记录所属 server 的 pid 与启动时间）；server 启动时只 kill 首进程启动时间仍匹配的进程组，避免误杀复用 PID；所属 server 仍存活时第二个 server 既不回收也不接管。Linux 另设 `PR_SET_PDEATHSIG`；Windows 不做回收。
- Provider 在信任读许可仍有效时完成子进程 spawn；撤销工作区信任取得写锁后会阻止后续启动，并取消已登记的活动 turn。信任状态只有在快照成功落盘后才更新内存。
- Pi 在工作区获得 TodeX 信任后固定使用 `--approve` 全自动运行；TodeX 不为 Pi 声明逐工具审批或 OS sandbox，`permissions` capability 保持 `false`。
- conversation event payload 上限 1 MiB：超过 1 MiB − 16 KiB 的 payload 在脱敏之后截断（最大字符串截断并记录原长度，必要时整体替换为 `{"truncated": true, "originalBytes": N}`），1 MiB 检查保留为最后防线。journal 与单个会话都不设体积上限：追加永不拒绝，进行中的 turn 总能写完；只有新 prompt 有闸门——数据目录所在磁盘可用空间低于 1 GiB 时返回 `STORAGE_LOW`（HTTP 507）。活动文件超过 64 MiB 封存，后台转为 raw DEFLATE 分帧的 `.seg`（帧约 1 MiB，CRC-32 校验；`.idx` 带 SHA-256），封存时把已有终态记录覆盖的流式进度记录换成 `journal.compacted` 标记。内存有界：解压帧缓存全局 32 MiB、会话索引最多 64 个（LRU），封存分片只常驻分片表与帧表，活动文件按记录保留偏移（≤64 MiB）。replay limit 上限 1000，且每页不超过约 8 MiB journal。v2 WebSocket 单消息上限 8 MiB（升级层强制，超限关闭连接）、单连接订阅上限 128、并发补放上限 4；socket 发送超过 20 秒或出站队列阻塞超过 10 秒即断开连接。
- MCP/Skill catalog 只读取配置，跳过 symlink，限制扫描深度、文件数和文件大小；响应不包含 command、args、env、URL 或凭据；现有测试验证输入文件未被修改。
- 旧 Codex session migration 是 copy-only、redacted、idempotent，并保留原始文件。

## 性能边界

- `cargo test --locked --all-targets --all-features -- --test-threads=1` 当前执行 177 个 backend 测试和 1 个非计费 E2E，耗时约 11 秒；5 个真实 provider 测试默认 ignored。
- Provider protocol、event journal、catalog 扫描和 WebSocket 都有显式内存/数量上限，避免单个请求无界增长。
- event replay 通过可重建的内存 sequence/offset 索引按页读取，每页仍校验返回的记录，遇到损坏记录即回退到完整校验扫描（见 [conversation-runtime.md](conversation-runtime.md)）；这优先保证 sequence 连续性和尾部恢复。启动时已结束的会话跳过完整扫描，其损坏在首次读取时发现。
- journal 为 history v3 分片：封存分片（`.seg`/`.idx`，或等待转换的 `events.NNNNNN.jsonl`）在前、唯一可写的 `events.jsonl` 在后，sequence 跨文件连续；明文文件中间损坏时先把受损文件备份为 `events.corrupt.<ts>.jsonl`，再原子重写这些文件并以 `journal.recordLost` 占位丢失的 sequence，`.seg` 帧或 `.idx` 损坏时只重建该分片（备份为 `events.corrupt.<ts>.seg`），无法恢复的记录同样占位；v2 迁移前原文件硬链接到 `journal-v2-backup/`，7 天后删除；占位数量受损坏字节数约束，损坏的 sequence 字段不能凭空生成大量占位，普通追加无法伪造任何 `journal.` 前缀的事件类型（包括 `journal.compacted` 压缩标记）。
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

1. 生产部署前验证反向代理仅开放 HTTPS/WSS，daemon 仅监听 loopback，`devices.json`、`history/`、audit 和 provider 登录目录使用最小文件权限。
2. 如果 replay 成为主要 CPU/IO 热点，先用生产规模 journal 做基准，再设计 checkpoint/index；不要以取消完整校验换取未经测量的优化。
