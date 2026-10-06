# 会话历史存储与端到端加密（history v3）

本文是后端存储格式、密钥体系与客户端解密的唯一规格。后端（`TodeX_backend`）、共享协议（`TodeX_protocol`）、web/desktop 与 iOS（`TodexCore`）按本文实现；与本文冲突时以本文为准并同步修订本文。

## 1. 目标与威胁模型

- 单个会话没有体积上限；只有数据目录所在磁盘可用空间低于 1 GiB 时新 prompt 返回 `STORAGE_LOW`（HTTP 507）。
- `history_encryption = "e2e"` 时，会话内容以密文落盘，后端磁盘上不存在能解密历史的私钥；实时推送与历史回放发送同一份密文，后端不解密、不重加密，原样转发（外层传输加密照常）。
- 保护：TodeX 数据目录被盗、被备份或泄露；事后攻破后端时读取已写入的历史。
- 不保护：后端运行 agent 时看到的明文（它必须处理模型输出）；provider 自身在后端保存的明文 transcript（`~/.claude/projects`、`~/.codex/sessions` 等）；排队中的追加 prompt（`queue.json`，投递后删除）；元数据（sequence、时间、事件类型、大小、下文 §5.2 的信封字段）。
- 迁移为单向：原文件硬链接到 `journal-v2-backup/` 保留 7 天；旧版 daemon 无法读取已迁移的会话。

## 2. 密码学原语（history crypto v1）

实现：`src/history_crypto.rs`、`TodeX_protocol/src/historyCrypto.ts`、`TodexCore/HistoryCrypto.swift`，共享向量 `history-crypto-v1.json`。

- 接收方密钥：X-Wing（ML-KEM-768 + X25519，draft-06，与 CryptoKit `XWingMLKEM768X25519`、noble `ml_kem768_x25519` 互通）。私钥是 32 字节种子，公钥 1216 字节。`rid = SHA-256(pk)[0..16]`。
- 分片密钥：`kid` 16 字节随机、`dek` 32 字节随机，只在后端内存中存在。
- 封装：`(ct, ss) = Encaps(pk)`，`kek = HKDF-SHA256(salt=ct, ikm=ss, info="todex-history-v1/wrap"‖kid‖rid)`，`wrapped = ChaCha20-Poly1305(kek, nonce=0¹², aad=info, dek)`。JSON：`{"rid","kemCt","wrapped"}`（base64url 无填充）。
- 内容：`ChaCha20-Poly1305(dek, nonce=u32be(stream)‖u64be(counter), aad="todex-history-v1/content\0"‖conversationId‖"\0"‖kid‖u32be(stream)‖u64be(counter))`。stream：1 事件摘要、2 事件完整、3 帧摘要、4 帧完整；事件流 counter 为 sequence，帧流 counter 为该 `kid` 下的帧序号。

## 3. 密钥体系

### 3.1 文件

- `$DATA_DIR/history/recipients.json`（0600，原子替换）：

```json
{
  "version": 1,
  "mode": "off",
  "epoch": 3,
  "recipients": [
    { "rid": "…", "kind": "device", "deviceId": "dev_…", "publicKey": "…",
      "addedAt": "RFC3339", "revokedAt": null },
    { "rid": "…", "kind": "recovery", "deviceId": null, "publicKey": "…",
      "addedAt": "RFC3339", "revokedAt": null }
  ],
  "grants": [
    { "grantId": "grt_…", "rid": "…", "deviceId": "dev_…",
      "requestedAt": "RFC3339", "status": "pending" }
  ]
}
```

  `epoch` 在接收方集合变化（新增、吊销、恢复密钥替换）时加一，切换 `mode` 不变。最多一个未吊销的 `recovery` 接收方；已吊销的公钥不能再次登记（`rid` 由公钥决定，须换新密钥对）。设备在 `devices.json` 被吊销时，其接收方同时标记 `revokedAt`；daemon 访问本文件时也会吊销 `devices.json` 中已不存在的设备的接收方。授权 `status`：`pending`、`fulfilled`、`dismissed`、`revoked`（接收方被吊销或换钥时待办授权转为 `revoked`）。
  `mode` 的来源：文件不存在时视为 `off`；首次写入时取配置 `history_encryption`（`TODEX_AGENTD_HISTORY_ENCRYPTION`），但仅当该次写入后已有未吊销的设备接收方才为 `e2e`；文件存在后以文件为准，配置不再生效。
- 每个会话 `keyring.json`（0600，原子替换）：`{"version":1,"keys":[{"kid","createdAt","epoch","wraps":[WrappedKey…]}]}`。授权新设备只向已有 `kid` 追加 `wraps`，不改动分片文件。

### 3.2 分片密钥生命周期

- `e2e` 模式下每个会话至多一个活动 DEK，首次加密写入时惰性生成：对所有未吊销接收方（设备 + 恢复）各封装一次，写入 `keyring.json`（fsync）后才能用于加密。
- 轮换条件：活动分片封存、`epoch` 变化、DEK 使用超过 24 小时、daemon 重启（内存丢失即轮换）。
- 已轮换但所在分片尚未封存的 DEK 仍留在内存，供封存时重打包；分片封存完成后清零。daemon 崩溃后丢失的 DEK 只影响该分片不能重打包压缩，内容仍可被客户端读取。活动文件改名封存的同时轮换 DEK，所以一个 DEK 的记录只落在一个分片里。
- 会话标题：`manifest.titleEnc = {"kid","ct"}`，以该 `kid` 的 stream 2、counter 0 加密，`kid` 同样登记在 `keyring.json`；`e2e` 下省略 `manifest.title`（客户端视同空串）。每次设置标题都用一个只为它生成的新 `kid`（不进入活动 DEK、用后即弃），所以 counter 0 永不在同一 `kid` 下复用。
- 一次性 DEK：标题与迁移（§8）各自生成新 DEK，同样先对全部接收方封装并写入 keyring。
- 无接收方：`e2e` 下若没有未吊销的接收方（或 keyring 无法写入），新 DEK 无法生成。此时新 prompt（包括追加队列的投递与 `conversation.retry`）在保存请求快照时被拒绝，返回 `CONFLICT`（「history encryption has no active recipients…」），不写任何内容；已在运行的 turn 继续用内存中最新的 DEK 加密写完。内存中没有任何 DEK（daemon 刚重启）时追加直接失败、记录错误日志，turn 以失败结束（终态事件同样写不进去时由重启恢复关闭），绝不回落为明文。

### 3.3 新设备与恢复

- 新设备配对后调用 `history.recipient.register` 登记自己的公钥；它只能读取此后轮换出的 DEK。
- 读取旧历史需要授权：新设备发 `history.grant.request` → 已授权设备 `history.grant.list` 看到待办 → 逐会话 `history.keys.list` 枚举 `kid`、`history.keys.wraps` 取回给自己的封装、本地解开后对目标 `rid` 重新封装 → `history.grant.fulfill` 分批上传。后端只校验 `kid` 存在、目标 `rid` 与授权一致，不接触 DEK。
- 恢复密钥：首台设备开启加密时生成 32 字节种子，以 BIP39 英文 24 词与二维码展示（可跳过，跳过须二次确认警告），只上传公钥（`history.recovery.set`）。导入恢复密钥的设备以恢复 `rid` 取回封装，对自己重新封装后上传（等价于一次自授权 `history.grant.fulfill`，`grantId` 为空、目标为自身 `rid`）。

## 4. 存储格式 v3

### 4.1 文件布局

```
events.jsonl            活动分片：v3 行，明文 JSONL，只追加并逐条 fsync
events.000007.jsonl     刚封存、待压缩的分片（短暂）
events.000006.seg       封存分片：分帧 raw DEFLATE（e2e 下再加密）
events.000006.idx       sidecar 索引（JSON，含校验和）
keyring.json            e2e 才有
manifest.json  snapshot.json  provider-state.json  last-request.json  queue.json
```

活动分片原始大小超过 64 MiB 时封存（改名为 `events.NNNNNN.jsonl`），后台任务转为 `.seg + .idx`：写临时文件 → fsync → rename `.idx` → rename `.seg` → 删除明文分片 → fsync 目录。恢复规则：同号 `.jsonl` 与 `.seg` 并存且 `.idx` 校验通过时删除 `.jsonl`，否则删除 `.seg/.idx` 重做。

### 4.2 v3 行

```json
{"s":42,"i":"evt_…","t":1759670000123456,"y":"message.delta","r":"rawType?","p":"claude-code?",
 "e":{ /* §5.2 信封字段 */ },
 "c":{ /* off：完整 payload */ },
 "x":{ /* e2e：密文，见 §5.3 */ }}
```

`conversationId`、`schemaVersion`、`normalizedType` 不落盘，读取时还原。`c` 与 `x` 互斥。读取同时兼容 v2 完整行与 `CompactedRecord`。

### 4.3 封存分片

- 帧：按约 1 MiB 原始数据切分，帧不跨 `kid`。三条流：信封流（每行 `{"s","i","t","y","r","p","e"}`，raw DEFLATE，不加密）、摘要内容流、完整内容流（各为 payload 的 JSON 数组，raw DEFLATE（RFC 1951，无 zlib/gzip 头，浏览器可直接解压）后在 e2e 下以 stream 3/4、counter=帧序号加密）。
- `.idx`：首尾 sequence、每帧（流、首 sequence、条数、压缩偏移与长度、原始长度、`kid`、帧序号）、§6 的分片摘要、SHA-256。
- 封存时精简：已确认存在终态记录（`message.completed`、`tool.completed`、`subagent.completed` 等，按 block/tool id 匹配）的 `message.delta`、`tool.updated`、`subagent.updated` 换成 `journal.compacted` 标记；`thought.delta` 与以 toolUse 结束的旁白 delta 保留。
- 内存：每个会话只常驻分片表；帧表按需加载；全局解压帧缓存 32 MiB、会话索引 64 个，均 LRU。

### 4.4 实现补充（存储轨，off 模式）

以下是 `src/conversation/{record,segment,store,maintenance}.rs` 对本节的细化，不改变上文约定：

- v3 行另有两个可选字段：`n` 仅在原 `normalizedType` 与按 `y` 重算的值不同时写入（`""` 表示原值缺失）；`tn` 保存旧事件微秒以下的纳秒余数。迁移后的事件与原事件逐字段相同。新追加的事件时间截到微秒。信封 `e` 为空时省略。
- `.seg` 文件头为 `TDXSEG1\n` 加 16 字节分片 id；每帧有 36 字节帧头（流、`kid` 长度、首 sequence、条数、原始与存储长度、帧序号、原始数据 CRC-32），因此丢失或损坏的 `.idx` 可由帧头重建。raw DEFLATE 自身无校验，读取每帧都核对 CRC-32；`.idx` 的 SHA-256 覆盖其 body，另记 `.seg` 全文 SHA-256（只在崩溃恢复判定时计算）。信封流编号 0，内容帧沿用 3/4。
- `.idx` 记录 `sources`（被替换的明文文件）。v2 迁移把相邻明文段合并为至多 64 MiB 的一个分片、以首个来源的段号命名，恢复时 `.idx` 通过且 `.seg` 全文校验通过才删除残留来源，否则删除 `.seg/.idx` 重做。单个超过 64 MiB 的旧文件整体成为一个分片。
- 帧表与分片表一起从 `.idx` 读入（约每 MiB 原始数据 0.1 KB），不另行懒加载。
- 封存精简的匹配规则：`message.delta` 需同 turn 内同 `block.id` 的后续 `message.completed`（或列在 `block.supersedes` 中）；没有 block id 的 delta（Claude Code、ACP）一律保留，因为终态记录无法按 id 对应；`tool.updated` 需同 turn 同 `toolCallId`/`block.id` 的后续 `tool.completed`/`tool.failed`，或 ACP 的 `status` 为 `completed`/`failed` 的后续 `tool.updated`（终态 update 本身保留）；`subagent.updated` 需同 `subagentId` 的后续 `subagent.completed`/`failed`/`cancelled`。终态必须在同一分片内。
- 帧压缩级别 9：真实 64 MiB 会话封存为 4.0–5.9 MiB。
- 调试构建（不含 release 与 `cargo test`）读取 `TODEX_AGENTD_DEBUG_JOURNAL_SEGMENT_BYTES`（≥ 4096）作为封存阈值，供隔离后端的端到端测试在几轮对话后就产生封存帧与 passthrough 帧；未设置时行为不变。

### 4.5 实现补充（密钥轨，e2e）

- 行：加密记录写 `e` + `x`，`x` 与线上 `$enc` 逐字节相同（`{v, kid, c, n, s?, f}`）；读取时还原为「信封字段 + `$enc`」，实时推送、回放与 digest 用的都是这一形式。provider payload 自带的顶层 `$enc` 键改名为 `_$enc` 保存，不会被误认为密文。加密行的 base64 使单行最多约为 payload 的 3 倍，尾部读取窗口按此放宽。
- 内容帧分三组，同一帧不混组：明文（off 或尚未迁移）、`Sealed(kid)`（封存时 DEK 仍在内存：打开事件密文、精简、重新分帧后以 stream 3/4 加密）、`Passthrough`（DEK 已不在内存，或是 fork 的拷贝：帧条目就是事件级 `$enc` 对象，完整流的条目保留 `s` 与 `f`，摘要流的条目在有 `s` 时去掉 `f`）。帧头第 7 字节为标志位，bit 0 表示 passthrough；`.idx` 帧项相应带 `"x": true`。信封帧永远是明文组。
- 帧 AEAD counter = `段号 << 32 | 帧序号`（帧序号为本分片内同一 `(kid, stream)` 的第几帧）。DEK 通常只覆盖一个分片；段号在高位保证即使同一 `kid` 的帧出现在多个分片里 nonce 也不重复。线上 `frames[id].counter` 直接给出该值。
- 同一 `Sealed` 段的摘要帧与完整帧覆盖完全相同的记录（一起切帧），因此 `fr.i` 对两者通用。
- `Sealed` 帧的 CRC-32 覆盖存储的密文（daemon 不能解密），明文与 passthrough 帧覆盖原始数据。
- 精简只看明文：DEK 在内存时按解密后的 payload 判断，passthrough 记录从不精简，只作为终态参与匹配；被精简记录的 `journal.compacted` 标记进入该记录所在的组（`Sealed` 时同样加密）。
- 封存提交后，被重打包的 `kid` 调用 `release_sealed` 从内存清零。
- 分片打捞（`.seg` 或 `.idx` 损坏时重建该分片，见 [conversation-runtime.md](conversation-runtime.md)）从不加密：明文与 passthrough 记录照旧重建，两条帧都通过 CRC 且整段信封都可读的 `Sealed` 段原样拷贝（保留 `kid`、帧序号与 counter），其余记录为 `journal.recordLost`。丢失 `.idx` 的分片因此不会丢失加密内容。
- 迁移改写既有 `.seg`（§8）时以替换协议提交：新文件先改名为 `events.NNNNNN.idx.next` / `.seg.next` 并 fsync，再依次替换 `.idx`、`.seg`。恢复时 `.idx.next` 仍在说明旧文件完好，删除两个 `.next`；否则新索引已就位，把 `.seg.next` 改名补上。
- 去重字段（§5.2）：`message.created` 的 `requestFingerprint` 为 `HMAC(fingerprintKey, sha256hex(请求 JSON))`（off 模式写的是 sha256hex 本身，比较时两者都接受）；`control.requested` 的 `requestFingerprint` 为 `HMAC(fingerprintKey, control 对象 JSON)`；带文本的 control 另有 `control.textMac = HMAC(fingerprintKey, text)`。这些在加密前写入 payload，因此信封与密文中的值一致。迁移明文记录时同样换算。

## 5. 线上格式

### 5.1 off 模式

与 v2 完全相同，客户端无需改动。

### 5.2 信封字段

e2e 下 `payload` 只保留以下明文字段（存在才写），其余内容全部在密文中：

- 通用标识：`turnId`、`clientRequestId`、`requestId`、`permissionId`、`runtimeId`、`operationId`、`itemId`、`messageId`、`toolCallId`、`subagentId`。
- `role`（`message.*`）、`status`（`provider.runtime`、`turn.*`、`subagent.*`）、`scope`（`permission.*`、`tool.awaitingApproval`，决定会话状态是否变化）、`code`（`control.*` 的结果码，不含 message 与 result）。
- `block`：仅 `{category, id, turnId}`。
- `control`：仅 `{action, itemId}`；文本类 control 另带 `textMac`。
- `requestFingerprint`、`textMac`：`HMAC-SHA256(fingerprintKey, …)`（具体输入见 §4.5），`fingerprintKey` 是后端 `history/fingerprint.key`（32 字节，0600）。后端去重只比较 MAC。
- `usage` 数值对象（客户端用量统计）、`stopReason`。

### 5.3 密文字段

```json
"payload": {
  "turnId": "…", "role": "assistant",
  "$enc": {
    "v": 1, "kid": "…", "c": "AAD 会话 id", "n": 42,
    "s": "摘要密文（可缺省）", "f": "完整密文",
    "fr": { "s": "帧 id", "f": "帧 id", "i": 3 }
  }
}
```

- 事件级（活动分片、passthrough 帧与实时推送）：`s`/`f` 为 base64url；摘要与完整相同则省略 `s`。`detail=summary` 下发 `s`（缺省时发 `f`），`detail=full` 下发 `f`，同一事件不会同时带两者。实时推送按 `full` 下发，与写入 journal 的密文逐字节相同。摘要在写入时由后端用明文算出，回放从不对密文运行摘要。
- 帧级（`Sealed` 帧）：事件 `$enc` 为 `{v, kid, c, n, fr: {s, f, i}}`（`kid` 为帧的 `kid`，`c`/`n` 为本会话与该事件 sequence），`s`/`f` 是摘要帧与完整帧的 id（不透明字符串，`<分片 id>-<偏移>`，分片重建后不复用）。HTTP 分页响应顶层附 `frames: { "<帧 id>": {"kid","stream","counter","c","ct"} }`，只含所请求 detail 的那一种帧（summary 为 stream 3，full 为 stream 4），同一页内去重；帧在页字节预算中按 base64 长度计一次。WebSocket 每条 `conversation.event` 单独解密，所以凡引用帧的回放消息顶层都附带它所引用的帧（同一帧会在多条消息里重复出现）。一次订阅补放累计携带的帧超过 16 MiB 时补放提前结束，结果为 `hasMore: true`，其余由客户端按 `nextSequence` 走 HTTP 分页（每帧只发一次）。帧明文是 raw DEFLATE 压缩的 payload JSON 数组，`i` 为下标。
- `c`/`n` 是 AAD 用的原会话与 sequence：fork 复制密文时保留来源值，新会话 sequence 可不同。fork 复制的帧级记录保留 `fr`，被引用的帧以线上形式存放在 fork 目录的 `frames/<帧 id>.json`，回放时同样放进 `frames`；fork 同时复制来源的 `keyring.json`，客户端用 fork 的会话 id 取封装。
- 解密后的 payload 整体替换 `payload`（其中已含信封字段）。无法解密（无 DEK、授权未到、校验失败）时 `payload` = 信封字段 + `{"detailLocked": true}`，sequence 照常推进。

### 5.4 能力协商

客户端握手声明 `historyEncryption: 1`：`/v2/ws` 升级请求与 HTTP 回放请求的 query 带 `historyEncryption=1`（v2 没有握手消息，升级 query 受设备签名覆盖）。`e2e` 后端拒绝未声明的客户端订阅与回放：`CLIENT_UPGRADE_REQUIRED`（HTTP 426）。`/v2/version` 返回 `historyEncryption`（后端支持的版本）。

## 6. 分片摘要

`.idx` 内嵌该分片的 `JournalDigest`（见 `src/conversation/digest.rs`，按区间可合并）；会话摘要 = 各分片摘要依次合并 + 活动分片增量。启动恢复、去重、重试定位只读摘要与定点事件，不读全量历史。

## 7. 命令（WebSocket v2，`dispatch_command_inner`）

| 命令 | 请求 | 响应 |
|---|---|---|
| `history.encryption.get` | `{}` | `{mode, epoch, recipients[], myRid?, grants[]}` |
| `history.encryption.enable` | `{}`（需至少一个设备接收方） | 同 get |
| `history.encryption.disable` | `{}` | 同 get |
| `history.recipient.register` | `{publicKey}`（绑定当前连接的 `deviceId`，幂等，换钥即替换并 `epoch+1`） | `{rid}` |
| `history.recipient.revoke` | `{rid}` | 同 get |
| `history.recovery.set` | `{publicKey}` | `{rid}` |
| `history.grant.request` | `{}` | `{grantId}` |
| `history.grant.list` | `{}` | `{grants[]}`（每项附目标 `publicKey`） |
| `history.grant.dismiss` | `{grantId}` | `{}` |
| `history.keys.list` | `{conversationId?, cursor?, limit≤500}`（默认 500） | `{items:[{conversationId, kid}], nextCursor?}` |
| `history.keys.wraps` | `{conversationId, kids[≤500], rid?}`（默认调用方 `rid`） | `{wraps: {kid: WrappedKey}}` |
| `history.grant.fulfill` | `{grantId?, rid, wraps:[{conversationId, kid, wrapped}]（≤500）, complete?}`（最后一批 `complete: true` 结束授权；已有封装跳过） | `{added}` |

`conversation.retry` 在 e2e 下必须携带 `prompt`（客户端解密后的原请求文本），否则 `INVALID_REQUEST`。e2e 下 `last-request.json` 不含提示原文：`request.text` 为空串、内联 `text`/`image` 内容项被移除，只保留 `textMac`、`contentMac`（内联项 JSON 的 HMAC）与文件引用；`prompt` 的 HMAC 必须等于 `textMac`（快照是明文时必须等于原文），否则 `CONFLICT`。重试只带回文件类附件，内联文本与图片不会重发。off 模式下快照仍含原文，`prompt` 被忽略。

control 幂等（`conversation.control` 以 `requestId` 去重）：加密记录只保留 control 对象的 MAC（`requestFingerprint`）与 `turnId`，重复请求按 MAC 判定是否为同一输入。已完成的重复请求返回本进程内存中记住的结果（最多 256 条，只在内存，不落盘）；daemon 重启后返回 `null`，结果仍在客户端可解密的 `control.completed` 事件中。被拒绝的重复请求返回 `Control was rejected (<code>).`（信封中的 `code`），原始错误信息只在密文里。

## 8. 迁移

- 惰性：启动不批量改写。空闲时后台按体积从大到小把 v2 分片合并为 64 MiB 的 `.seg`；每完成一个分片提交一次，可中断续跑。
- 开启 e2e 后，后台把已有分片逐个以新 DEK 加密重打包（此期间后端短暂看到这部分明文），完成后清零 DEK。具体顺序（只处理空闲会话，`history.encryption.enable` 会立即触发一次）：活动文件含明文记录时先封存它，封存转换把明文记录以一次性 DEK 加密进 `Sealed` 帧；仍有明文内容帧的既有 `.seg` 逐个以新 DEK 重建并按 §4.5 的替换协议提交；最后加密标题、去掉 `last-request.json` 的提示原文，并在 manifest 写入 `historyEncryptedAt`。每一步单独提交，崩溃后从未完成处继续。之后若在 off 模式下写入明文，`historyEncryptedAt` 被清除，下次开启时再迁移。e2e 下新建且起始为空的会话直接带 `historyEncryptedAt`。
- 原文件硬链接到 `journal-v2-backup/`，7 天后清理；开启加密时提示 Time Machine / APFS 快照可能保留旧明文。e2e 迁移本身不再生成新的明文备份；开启时 daemon 记录一条警告，说明 `journal-v2-backup/`（7 天内）、`events.corrupt.*` 打捞副本与 Time Machine / APFS 快照仍可能保留旧明文，客户端负责界面提示。
- 不在本规格保护范围内的文件不迁移：`queue.json`（投递后删除）、`provider-state.json`、provider 自己的 transcript。
