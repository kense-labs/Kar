# Kar 多客户端终端

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。对应 `apps/server/src/terminal/Manager.ts` 的 `retainViewerOnce`、`sharedViewerGrid`、`acceptsQueryReply`、`observeStream` 和 `subscribeMetadata`。其 `Manager.test.ts` 覆盖小屏退出、同一观看端重复连接、应答所有者转移和观察已退出的终端。

| Kar 工作台入口 | 行为 |
| --- | --- |
| `POST /projects/{p}/terminals/{id}/attach` | 返回 SSE。JSON 接受 `viewer_id`、成对的 `cols` 和 `rows`、可选 `worktree`、`after` 和 `max_history_bytes`。新终端在授权项目目录启动。已有终端复用原终端和留存历史。省略观看端 ID 时由服务生成，在首包返回。 |
| `PUT …/{id}/size` | 已连接的观看端带上自己的 `viewer_id`。共享 PTY 的列数、行数分别取所有有效尺寸的最小值。同一设备的同一观看端可以同时有多个附着流，重连保留最近一次调整的尺寸。最后一个流断开或设备授权撤销后释放该端的尺寸。 |
| `POST …/{id}/input` | `id` 和 `data_base64` 沿用原输入协议。终端仿真器回答设备查询时传 `reply: true` 和 `viewer_id`。同一 PTY 上最先应答的已连接端持有应答权，其他端的应答取得 `suppressed: true` 回执。最后一个所属流关闭后可重新选主。同一输入 UUID 的重试复用原回执，普通键入始终走原输入接口。 |
| `GET …/{id}/observe` | SSE 返回历史和实时输出，每页包含实际尺寸及状态。观察者不占用观看端租约。已退出的进程可读历史，未知终端返回 404。使用 `after` 或 `Last-Event-ID` 按字节续读，可用 `max_history_bytes` 限制首次历史。 |
| `GET /projects/{p}/terminals/metadata` | 新连接先收到本项目目录快照，其后收到 upsert、remove。正常退出保留目录，可读取退出码。显式关闭移出目录。macOS 采样 PTY 所属子进程的活动和进程名，目录流不包含终端输出。相同服务实例支持 `epoch.sequence` 游标，重启或超出保留窗口时返回新快照。 |

终端历史和最近生效的尺寸沿用现有磁盘格式。观看端租约和应答所有者属于当前连接。主机重启后，原 PTY 继续按 Kar 既有逻辑报告中断。

## 验证

开发平台为 macOS arm64，Rust 1.100.0。编译命令为 `cargo check --locked --offline --lib --tests --message-format=short`。集中场景待运行。

复验命令为 `cargo test --locked --test workbench_terminal_preview multiple_viewers_share_grid_query_writes_and_metadata_without_observer_votes`。用例启动 Kar 和真实 PTY，比较两端各自维护的尺寸，验证窄端断开恢复、无身份尺寸请求、同端双流、授权撤销、元数据快照及生命周期。测试 shell 发送实际设备属性查询并在 raw 模式捕获字节，以检查两个渲染端、重复输入和应答权转移后的确切结果。
