# 设备验证

TodeX 的唯一授权方式是设备验证：每台客户端设备持有一对长期 Ed25519 密钥，配对成功后公钥登记在后端 `devices.json` 注册表中，之后所有请求都用设备私钥签名。系统不再使用 Bearer token，也不存在任何手动输入访问令牌的路径。

## 配对流程

1. 客户端生成（或复用本 profile 的）设备密钥对，发起配对申请（`create`）并提交设备公钥与对临时公钥、随机数的承诺，再用 `reveal` 公开二者（配对 v3，见 [transport-v2.md](transport-v2.md#device-pairing-v3-commit-then-reveal)）。验证码只在 reveal 之后出现在 TUI。
2. 在后端 TUI 按 `d` 打开设备面板，选择待验证申请，核对双方完整的 `XXXXX-XXXXX` 验证码。验证码旁显示后端传输公钥指纹（`XXXX-XXXX-XXXX-XXXX`，`none` 表示明文，仅限回环）；验证码同时认证该公钥和设备名称。详情还列出设备名称、由设备公钥推导的设备 ID（批准后登记的就是这个 ID）、对端地址（回环地址标注「本机」）。两类警告紧跟在验证码下方：
   - 该公钥已登记：「重新配对，会覆盖现有记录」；
   - 该设备 ID 在历史封禁列表（`revokedDevices`）中：「该设备曾于 … 被吊销。批准后恢复 API 访问，历史仍需另一台设备执行恢复」。
3. 按 `a` 批准，或按 `r` 拒绝。设备名称由客户端自报，但已写入 transcript：名称被篡改时验证码不同。核对时以验证码为准。
4. 批准后设备公钥写入注册表，加密凭据返回 `deviceId` 和后端当前的传输协议与公钥。客户端核对凭据与 `create` 响应一致后，一次性保存设备身份和公钥（`transportVerified`），然后用签名认证并加密连接。

申请有效期为 5 分钟，可在客户端取消；`create` 后 30 秒内未 `reveal` 的申请直接丢弃。设备名称规则在各端一致：客户端去掉首尾空白、为空时填默认名；后端只校验（1–80 个 Unicode 标量，无控制字符与 bidi 控制字符，首尾不是空白）并原样保存，不再改写。切换后端或修改地址会取消当前申请；过期、拒绝、取消以及已完成的申请不能再次领取结果。同一设备公钥重复配对会更新已有记录而不是新建；被吊销的设备可以重新配对恢复。

## 设备管理

TUI `d` 面板分两段：上半是待验证申请（`a`/`r`），下半是已注册设备列表（Tab 切换，`x` 吊销所选设备）。`x` 重置菜单中的「设备」项会吊销全部已注册设备。吊销在下一个请求即生效——后端按文件修改时间重载注册表，无需重启 daemon。

## 请求级签名

每个 HTTP 请求与 WebSocket 握手携带四个凭据：`x-todex-device-id`、`x-todex-auth-ts`、`x-todex-auth-nonce`、`x-todex-auth-sig`（无法设置 header 的握手使用等价 query 参数）。签名是 Ed25519，覆盖：

```
todex.device-auth.v1 \0 deviceId \0 method \0 path \0 canonicalQuery \0 timestamp \0 nonce \0 base64url(sha256(body))
```

服务端按序校验：时间戳偏差不超过 300 秒且不早于 daemon 本次启动 → 设备已注册 → 签名有效（Ed25519 `verify_strict`，拒绝小阶公钥与非规范签名）→ nonce 未在窗口内出现过。签名覆盖完整 query，因此 WebSocket 握手中的传输加密参数（`tv`、`enc`、`client_nonce`、`client_key`、`ciphertext`）也被绑定到设备身份，中间人无法替换；transport v2 还把签名凭证中的 `deviceId` 写入会话密钥的 transcript。经 `POST /v2/sealed` 隧道的 REST 请求由内层请求自带签名，签名覆盖内层方法、路径、query 与 body。

时间戳越界返回 401 `AUTH_TIMESTAMP_REJECTED`，响应顶层带 `serverTime`（Unix 秒）：REST 客户端按后端记下 `offset = serverTime − 本地时间`，用新 nonce 重签并重试一次；WebSocket 只靠退避重连。签名在发出请求时生成，客户端在系统休眠前签好、唤醒后才送达的请求同样会被拒绝，重签即可。自定义 header 使跨源请求需要 CORS 预检，服务端以 `Access-Control-Max-Age: 7200` 允许浏览器缓存预检结果（Chromium 上限两小时），避免每个签名请求都额外占用一次连接。nonce 缓存是进程内的，daemon 重启后会清空；为此后端记录启动时刻，时间戳早于启动时刻的凭证一律拒绝，重启前截获的签名无法在新进程里重放。缓存按时间戳排序整体淘汰过期条目（未来时间戳的条目不会阻塞淘汰），每台设备窗口内最多 8192 个 nonce，超出只对该设备返回 429 `RATE_LIMITED`，全局 65536 条作为最后防线。

## 与传输加密的关系

设备验证同时交付传输加密公钥：`create` 响应带回后端配置的 `transportProtocol` 与握手正在使用的 `transportPublicKey`，二者写入配对 transcript，由验证码认证，并在加密凭据中重复一次。没有任何手动同步公钥的路径；配对二维码只包含服务器地址。客户端只信任经配对验证的公钥（`transportVerified`），旧 profile 中手动保存的公钥一律要求重新配对。TUI 重置加密密钥后，daemon 按 `pairing_keys.json` 的修改时间重载，握手和配对同时切换到新公钥，已配对的客户端须重新配对。

## 实现边界

批准入口只读写后端本机的私有控制目录，没有公网批准 API。配对申请、查询和取消请求有大小、数量与频率限制：按来源（IPv4 地址或 IPv6 /64）每分钟 4 次 `create`、最多 2 个未完成申请（429 `PAIRING_BUSY`）；活动申请满 16 个时先淘汰最早未 reveal 的申请；每申请每分钟 8 次 `reveal`、每秒 4 次 `poll`/`cancel`，另有全局上限（429 `RATE_LIMITED` 带 `Retry-After`）。承诺不符的 `reveal` 返回 401 但保留申请，避免第三方用错误揭示把别人的申请作废。小阶 Ed25519 设备公钥在 `create` 时即被拒绝。

配对握手沿用双向临时 X25519 DH：双方各出临时公钥，验证码由共同 transcript（含设备公钥与客户端随机数）导出，可检测中间人。v3 要求客户端先提交承诺、看到服务端公钥后才揭示自己的临时公钥，中间人无法再针对 40 位验证码反复挑选自己的公钥；批准后设备身份与传输公钥经 HKDF-SHA256 + XChaCha20-Poly1305 加密投递，只有发起方能够解密。临时密钥只用于配对本身；长期传输公钥来自后端握手所用的同一份内存密钥。

设备私钥只保存在客户端各自的安全存储（iOS Keychain、WebCrypto 不可导出密钥、桌面端 safeStorage），不进入注册表、不出现在任何响应或 TUI 界面。
