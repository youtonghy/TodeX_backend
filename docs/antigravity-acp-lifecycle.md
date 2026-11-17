# AGY ACP 轮次完成边界验证

本记录说明 `src/provider/antigravity/native_idle.rs` 使用原生空闲状态的依据。验证对象为 macOS ARM64 的 AGY 1.5.10，SHA-256：

```text
780ee763a9938a37b45c7f9d6ae31747d7c574358fd10c64d3a1cdc36faf2798
```

通过 IDA Pro MCP 分析当前二进制副本，并核对函数机器码与原文件一致。原有 IDB 与当前文件不匹配，已排除；函数名与边界从当前文件的 Go pclntab 恢复。以下地址只适用于上述二进制。

| 函数 / 分支 | 地址 | 验证结果 |
| --- | --- | --- |
| ACP `Server.promptTurn` | `0x102f88a60` | ACP prompt 的生命周期入口；现场日志中其 `end_turn` 应答早于后续工具输出与审批。单独收到该应答不能代表后台工作全部结束。 |
| ACP `Server.sendOutOfTurnStepUpdates` | `0x102f94740` | 存在轮次外发送 step 更新的独立路径；发送前检查会话匹配与 `State.Closed`，会话未关闭时可继续发送。 |
| `CascadeManager.WaitForConversationFullyIdle` | `0x1022b6fe0` | 等待原生状态；`0x1022b74d0` 检查状态对象的 fully-idle 位，完成路径在 `0x1022b77d8` 返回 false。 |
| 同一等待函数的超时路径 | `0x1022b7730` | 返回 true；该返回值表示 `timedOut`，不能当作空闲或成功。 |
| 同一等待函数的 subagent 检查 | `0x1022b72d0` | 调用 `AgentState.HasSubagentModelRequest`；存在请求时走 `0x1022b7420` 的计时器重置路径。 |
| `ActivitySnapshot.IsFullyIdle` | `0x101dfbbd0` | 检查多项活动状态，不能以根代理是否正在输出或单个工具是否结束替代。 |

同时从当前二进制内嵌 protobuf 描述符核对了 `AgentStateUpdate.fully_idle`、`CascadeState.fully_idle` 及等待接口字段。反编译中自动猜测的局部变量名和手动设置的 C 参数名不作为语义证据；返回值及分支以 ARM64 指令为准。

在隔离进程中只执行 ACP `initialize`、`session/new`，然后调用同一进程日志公布的 HTTP 端口，验证下列 Connect 接口对空会话返回 HTTP 200 / `{}`：

```text
POST /exa.language_server_pb.LanguageServerService/WaitForConversationFullyIdle
Content-Type: application/json
Connect-Protocol-Version: 1
```

请求包含 `conversationId`、`inactivityTimeoutSeconds`、`stabilizationDurationSeconds`、`returnOnExecutorError`。响应的 `timedOut` 省略时为 protobuf 默认值 false。此探测没有发送模型 prompt；它验证了接口可访问与空会话响应，真实后台工作的返回分支由静态分析验证，TodeX 的生命周期处理由模拟 ACP / HTTP 回归测试验证。

TodeX 在收到正常 ACP `end_turn` 后保留原 turn，同时接收 ACP 输出、处理权限与取消；等待原生确认空闲、答复完未决反向请求、处理已缓冲的 ACP 帧后才完成。`timedOut: true` 会重新等待。状态查询失败会使 turn 明确失败并丢弃进程。审批答复可能启动新工具或模型执行，因此答复后会重新确认空闲，不能复用答复前的状态。取消仍走有界 ACP 取消流程，不等待原生空闲。

这是 AGY 的内部接口，后续 CLI 版本可能改变日志格式或 RPC。没有可验证的状态时必须明确报错，不能退回按静默时间推断成功。真实付费任务未用于本次验证。
