# Qwen Code 原生队列与跨重启续收

| 系统 | 真实供应商程序 | 测试结果 |
| --- | --- | --- |
| macOS 27.2.1 arm64 | Qwen Code 0.27.4，Node 24.15.0 | `qwen_live` 8 项通过 |
| Ubuntu 24.04.5 aarch64，Lima 虚拟机 | Qwen Code 0.27.4，Node 24.15.0 | 同一组 8 项通过 |
| macOS 27.2.1 arm64 | Qwen Code 0.28.4，独立发行包 | 同一组 8 项通过 |

复验时将 `KAR_QWEN_BIN` 设为对应发行包中 `qwen` 的绝对路径。npm 发行包在 `PATH` 里需要可用的 Node。每个用例创建独立的工作区、Kar 聊天目录和 Qwen 用户目录，启动真实 Qwen 进程，通过 stream-json 收发。模型请求由 Rust 回环 HTTP 服务接收并按用例放行，未测试远程模型服务。

```sh
"$KAR_QWEN_BIN" --version
cargo test --locked --test qwen_live -- --ignored --test-threads=2 --nocapture
```

## 进程终止与重开

三个 `queue_crash::kill_after_*` 用例先完成一轮，再从正常入口 `kar chat --json resume --chat <id>` 启动控制进程，排入三条具有独立 UUID 的输入。测试在下列位置强制结束控制进程和该用例的原生子进程组，重开原工作区的同一原生会话。

| 操作 | 期望 | 两个平台 0.27.4 与 macOS 0.28.4 的实测 |
| --- | --- | --- |
| Qwen 已写入用户消息，模型已收到请求，Kar 尚未写入发送回执 | 首条待确认，其余两条按序续投 | 首条 `Uncertain`，Kar 回合 `Interrupted`。两条 `Pending` 输入完成，后续模型请求含第一轮历史 |
| Qwen 又已将助手回答写入原生日志，Kar 仍未写入发送回执 | 对不上输入 ID 时保留待确认，避免重复执行 | 首条仍为 `Uncertain`。其余两条完成，原生日志和模型端均无重发 |
| Kar 已持久化发送回执，模型回答尚未返回 | 保留已发状态，继续发送其余两条 | 首条 `Sent`，Kar 回合 `Interrupted`。其余两条完成 |

每次重开后，再用原 UUID、原文分别调用 Kar 的排队和发送入口，重复清空队列。原生 JSONL 的用户消息和回环模型收到的请求均只包含预期的四次新输入，次序为初始轮、首条、第二条、第三条。某条未获答时 Qwen 会在下一次模型请求中合并相邻用户内容，测试同时核对每次请求的最新输入和独立的原生用户记录。

两个独立 HTTP 客户端分别在初始轮后和首条排队输入已开始后断开。事件服务随控制进程重建，两端用各自保存的 `Last-Event-ID` 补齐至重启点，保持连接接收其余输入的实时更新。各自的事件序列与 Kar 持久记录逐条相等，最终回合与聊天快照一致。

## 原生 UUID 的边界

`queue_crash::native_retry_of_same_sdk_uuid` 先完成一次原生会话，再以相同会话 ID 重开，并从 SDK 以完全相同的 UUID 和原文再次直接发送。对两次出站 stream-json 帧的检查均看到了调用方原始 UUID。Qwen 仍产生了两次模型调用、两个不同的用户记录 UUID 和两个不同的原生 `promptId`（同一会话的 `########1`、`########2`）。完整原生 JSONL 中没有调用方的 UUID。

固定上游运行时 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 的 Qwen `session.ts` 第 55 行把 `inputId` 写进 `user.uuid`。Kar 现按此约定发送。Qwen 0.27.4 包内 `chunks/session-SKSTYW3C.js` 的 `processUserMessage` 调用 `getNextPromptId()` 依据会话 ID 和计数生成提示 ID。`chunks/chunk-WG5L7ABP.js` 的 `ChatRecordingService.createBaseRecord` 为日志行生成另一 UUID。上述实测证实直接重投不具备幂等性，也无法从这份记录按调用方 UUID 对账。Kar 保留待确认状态并跳过自动重发。
