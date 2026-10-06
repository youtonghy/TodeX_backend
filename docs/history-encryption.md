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

  `epoch` 在接收方集合变化（新增、吊销、恢复密钥替换）时加一。最多一个未吊销的 `recovery` 接收方。设备在 `devices.json` 被吊销时，其接收方同时标记 `revokedAt`。
- 每个会话 `keyring.json`（0600，原子替换）：`{"version":1,"keys":[{"kid","createdAt","epoch","wraps":[WrappedKey…]}]}`。授权新设备只向已有 `kid` 追加 `wraps`，不改动分片文件。

### 3.2 分片密钥生命周期

- `e2e` 模式下每个会话至多一个活动 DEK，首次加密写入时惰性生成：对所有未吊销接收方（设备 + 恢复）各封装一次，写入 `keyring.json`（fsync）后才能用于加密。
- 轮换条件：活动分片封存、`epoch` 变化、DEK 使用超过 24 小时、daemon 重启（内存丢失即轮换）。
- 已轮换但所在分片尚未封存的 DEK 仍留在内存，供封存时重打包；分片封存完成后清零。daemon 崩溃后丢失的 DEK 只影响该分片不能重打包压缩，内容仍可被客户端读取。
- 会话标题：`manifest.titleEnc = {"kid","ct"}`，以该 `kid` 的 stream 2、counter 0 加密，`kid` 同样登记在 `keyring.json`；`e2e` 下 `manifest.title` 为空串。

### 3.3 新设备与恢复

- 新设备配对后调用 `history.recipient.register` 登记自己的公钥；它只能读取此后轮换出的 DEK。
- 读取旧历史需要授权：新设备发 `history.grant.request` → 已授权设备 `history.grant.list` 看到待办 → 逐会话 `history.keys.list` 枚举 `kid`、`history.keys.wraps` 取回给自己的封装、本地解开后对目标 `rid` 重新封装 → `history.grant.fulfill` 分批上传。后端只校验 `kid` 存在、目标 `rid` 与授权一致，不接触 DEK。
- 恢复密钥：首台设备开启加密时生成 32 字节种子，以 BIP39 英文 24 词与二维码展示（可跳过，跳过须二次确认警告），只上传公钥（`history.recovery.set`）。导入恢复密钥的设备以恢复 `rid` 取回封装，对自己重新封装后上传（等价于一次自授权 `history.grant.fulfill`，`grantId` 为空、目标为自身 `rid`）。

## 4. 存储格式 v3

### 4.1 文件布局

```
events.jsonl            活动分片：v3 行，明文 JSONL，只追加并逐条 fsync
events.000007.jsonl     刚封存、待压缩的分片（短暂）
events.000006.seg       封存分片：分帧 zstd（e2e 下再加密）
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

- 帧：按约 1 MiB 原始数据切分，帧不跨 `kid`。三条流：信封流（每行 `{"s","i","t","y","r","p","e"}`，zstd，不加密）、摘要内容流、完整内容流（各为 payload 的 JSON 数组，zstd 后在 e2e 下以 stream 3/4、counter=帧序号加密）。
- `.idx`：首尾 sequence、每帧（流、首 sequence、条数、压缩偏移与长度、原始长度、`kid`、帧序号）、§6 的分片摘要、SHA-256。
- 封存时精简：已确认存在终态记录（`message.completed`、`tool.completed`、`subagent.completed` 等，按 block/tool id 匹配）的 `message.delta`、`tool.updated`、`subagent.updated` 换成 `journal.compacted` 标记；`thought.delta` 与以 toolUse 结束的旁白 delta 保留。
- 内存：每个会话只常驻分片表；帧表按需加载；全局解压帧缓存 32 MiB、会话索引 64 个，均 LRU。

## 5. 线上格式

### 5.1 off 模式

与 v2 完全相同，客户端无需改动。

### 5.2 信封字段

e2e 下 `payload` 只保留以下明文字段（存在才写），其余内容全部在密文中：

- 通用标识：`turnId`、`clientRequestId`、`requestId`、`permissionId`、`runtimeId`、`operationId`、`itemId`、`messageId`、`toolCallId`、`subagentId`。
- `role`（`message.*`）、`status`（`provider.runtime`、`turn.*`、`subagent.*`）、`scope`（`permission.*`）。
- `block`：仅 `{category, id, turnId}`。
- `control`：仅 `{action, itemId}`；文本类 control 另带 `textMac`。
- `requestFingerprint`、`textMac`：`HMAC-SHA256(fingerprintKey, 原文)`，`fingerprintKey` 是后端 `history/fingerprint.key`（32 字节，0600）。后端去重只比较 MAC。
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

- 事件级（活动分片与实时推送）：`s`/`f` 为 base64url；摘要与完整相同则省略 `s`。`detail=summary` 下发 `s`（缺省时发 `f`），`detail=full` 下发 `f`。
- 帧级（封存分片）：事件只带 `fr`；分页响应与订阅回放消息顶层附 `frames: { "<帧 id>": {"kid","stream","counter","c","ct"} }`，同一页内去重。帧明文是 payload 数组，`i` 为下标。
- `c`/`n` 是 AAD 用的原会话与 sequence：fork 复制密文时保留来源值，新会话 sequence 可不同。
- 解密后的 payload 整体替换 `payload`（其中已含信封字段）。无法解密（无 DEK、授权未到、校验失败）时 `payload` = 信封字段 + `{"detailLocked": true}`，sequence 照常推进。

### 5.4 能力协商

客户端握手声明 `historyEncryption: 1`。`e2e` 后端拒绝未声明的客户端订阅与回放：`CLIENT_UPGRADE_REQUIRED`。

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
| `history.grant.list` | `{}` | `{grants[]}` |
| `history.grant.dismiss` | `{grantId}` | `{}` |
| `history.keys.list` | `{conversationId?, cursor?, limit≤500}` | `{items:[{conversationId, kid}], nextCursor?}` |
| `history.keys.wraps` | `{conversationId, kids[≤500], rid?}`（默认调用方 `rid`） | `{wraps: {kid: WrappedKey}}` |
| `history.grant.fulfill` | `{grantId?, rid, wraps:[{conversationId, kid, wrapped}]}`（≤500） | `{added}` |

`conversation.retry` 在 e2e 下必须携带 `prompt`（客户端解密后的原文）。

## 8. 迁移

- 惰性：启动不批量改写。空闲时后台按体积从大到小把 v2 分片合并为 64 MiB 的 `.seg`；每完成一个分片提交一次，可中断续跑。
- 开启 e2e 后，后台把已有分片逐个以新 DEK 加密重打包（此期间后端短暂看到这部分明文），完成后清零 DEK。
- 原文件硬链接到 `journal-v2-backup/`，7 天后清理；开启加密时提示 Time Machine / APFS 快照可能保留旧明文。
