# Kimi KAP 和 Codex 的跨重启队列

Kar 以输入 UUID 识别一次投递，将开始发送与收到回执分别落盘。重复提交相同 UUID 和原输入会返回已有状态。进程可能在供应商已收到输入、Kar 尚未记下回执时退出。重启时，开始发送过的输入保留原 UUID；Kar 不会自动重发。待投递项保留顺序。

这两个适配器遇到未确认的首条时会暂停后续投递。重开原工作区、原生会话后，Kar 通过以下证据自动确认原输入并放行队列。

| 适配器 | 可证明原输入的证据 | 保留待确认的情况 |
| --- | --- | --- |
| Codex app-server | 原线程的 `thread/resume` 或 `thread/read(includeTurns=true)` 回应中，某回合开场的 `userMessage.clientId` 等于 Kar UUID。原生回合 ID 唯一，UUID 只出现在这一个用户项中。 | 快照缺少 UUID、用户项位于插话位置、重复出现、来源不符或身份信息被截断。 |
| Kimi KAP | 重开原 session，订阅 `main`。原生 `turns` 中唯一的 `agent` 回合有用户开场项，`placement=opener`、`origin.kind=user`，`prompt_ids` 仅含目标 UUID。整份快照的用户项里这个 UUID 只出现一次。 | 主会话没有可匹配记录、只有插话或合并输入、跨项重复、来自其他代理或身份信息被截断。Kimi 使用 ACP 的版本走原有 ACP 路径。 |

`kar chat queue --chat CHAT` 会显示待确认输入。JSON 中 `needs_acknowledgement: true` 表示它阻挡后续自动投递。可以核对原生会话内容后用下列命令继续。此操作保留该条的 `uncertain` 状态并记录 `uncertainty_acknowledged: true`，后续按序继续。若以后重开时找到原生 UUID，原回合仍可得到确认。授权其他用户代为判断时，应提供明确的聊天和输入 ID。

```sh
kar chat acknowledge-uncertain INPUT_UUID --chat CHAT_UUID --command-id COMMAND_UUID
```

Rust SDK 的 `ChatStore::queue_control` 接受 `QueueAction::AcknowledgeUncertain { input }`。已认证的工作台入口为 `POST /projects/{project}/chats/{chat}/queue/commands`，JSON 为 `{"command_id":"COMMAND_UUID","action":"acknowledge_uncertain","input":"INPUT_UUID"}`。命令 UUID 可安全重试。普通队列 resume、指定下一条 start 或 reorder 均不能越过待确认的首条。

## macOS 复验

将对应环境变量设为已安装的真实供应商 CLI 的绝对路径。Kimi 要求有主代理历史快照的 KAP Web 构建，Codex 要求支持 `app-server` 和线程读取。四个 opt-in 用例使用隔离的 HOME、工作目录、聊天存储与原生会话，模型端使用 Rust 回环 HTTP 服务。每个用例打印实际 CLI 版本、平台、根会话 ID、输入 UUID、独立原生查询结果与模型请求次序。

```sh
cargo test --locked --test provider_queue_crash kimi_kap_sigkill_ -- --ignored --nocapture --test-threads=1
cargo test --locked --test provider_queue_crash codex_sigkill_ -- --ignored --nocapture --test-threads=1
```

| 操作 | 期望 |
| --- | --- |
| 原生进程组暂停，Kar 已持久化第一条的开始记录；终止真正的 `kar chat resume` 进程和此测试的原生进程组 | 无原生开场记录，模型看不到首条；后两条等待人工处置。同一处置命令重试两次，后续两条仍各送一次。 |
| 原生进程组继续，模型已经收到首条，持久聊天日志仍被共享文件锁固定在回执之前；发送 SIGKILL | 在相同 HOME 和工作区用独立原生连接读取历史。能确认 UUID 时自动续投；已收包而历史缺少身份时显示待确认，人工放行后继续。原条目不会产生第二次模型请求。 |
| 再开相同聊天、清队列、重交原 UUID | 回合 ID 和顺序稳定，模型请求计数不增加。 |

独立查询读取 Codex 的原生 JSON-RPC 响应或 Kimi 的 HTTP/WS 主会话快照。模型收包证明进程收到了输入，用于划定强杀时刻；它不能证明原生程序已经把 UUID 持久化。实机结果由统一验收执行方记录。本次开发检查为 macOS arm64 的 `cargo check --locked --tests`。
