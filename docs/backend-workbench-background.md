# 后台任务等待与停止

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。下表的行号和符号来自该提交。Kar 的 Claude 实现见 `90d5793`，单个工作流停止见 `91c518d`。

| 固定源码与行为 | Kar 操作和期望 | macOS 验收进度 |
| --- | --- | --- |
| `apps/server/src/orchestration-v2/ThreadManagementService.ts:1688`、`Adapters/ClaudeAdapterV2.ts:14857` | 在所属聊天调用 `POST …/background-tasks/await`，提供 1–20 个任务 ID。匹配项按请求顺序去重，未匹配项保留顺序及重复。默认设置等待，全未匹配返回 `unmatched`。清除未知 ID 返回空匹配。有原始启动游标时，同名的后续任务无法被旧客户端修改。控制器失联、适配器缺少实现与超时分别返回可区分的错误。 | 双客户端协议候选已编译；运行命令见下文。 |
| `ClaudeAdapterV2.ts:3886,6970,7033,10677` | 只追踪 `local_bash`。显式前台任务稍后出现在列表时加入，保留启动工具和命令。首次识别 Monitor 自动进入等待；清除等待后，列表刷新保留用户选择。列表排序/移除、原生结束、ID 再次使用以及任务与前台回合独立结束均通过聊天事件传递。 | 双客户端协议候选包含完成后修改、暂停/恢复订阅、启动游标、父回合及 Monitor 识别。已安装 Claude Code、可用的 Bash 与本地模型服务仍待统一实测。 |
| `ClaudeAdapterV2.ts:7162,14473,14504`、`RestartBackgroundNote.ts:258` | `GET …/background-tasks` 返回待办、失联项、原回合和受影响的已结束回合。重启后清除旧进程活动状态，保留此前是否在等待。相同 ID 在新原生进程产生新游标。Kar 聊天记录保留前台原生结果。 | 协议候选包含杀死工作台、重连与原会话重用同名任务。已编译，未执行。 |
| `ThreadManagementService.ts:1650`、`ClaudeAdapterV2.ts:14826` | 单个工作流停止需要指定所属聊天、任务 ID 和原始启动游标，15 秒内等待原生答复。工作区内原生 Bash 单任务停止、等待、取消等待分别通过已有公开 API 调用。 | 独立原生候选包含对 `stop_task` 的原生终态检查。 |
| `apps/server/src/mcp/toolkits/orchestrator/handlers.ts:65`、`tools.ts:146` 和 `core.test.ts:1293` | 工具权限由调用者所处的会话决定，固定契约允许在该原生会话没有活动前台回合时操作后台任务。Kar MCP 工具为 `kar_background_wait`，项目/会话授权由工作台入口提供。 | 真实 MCP 客户端、前台已完成与后台并发的整条链路待验。 |

## 待统一运行

| 环境与供应商 | 命令 | 期望与边界 |
| --- | --- | --- |
| macOS，Kar 服务器、两个 HTTP 客户端、隔离的 Claude 协议对端 | `cargo test --locked --test chat_restart -- workbench-background` | 匹配/清除、SSE 断线续读、列表变更和两次 ID 复用。记录协议结果。 |
| macOS，已安装 Claude Code（记录实际版本），隔离 HOME，回环模型，Bash 和主机批准正常 | `cargo test --locked --test native_live actual_claude_waits_for_host_approval -- --ignored --exact` | 原生 CLI 创建后台 Bash，返回工具 ID、原回合和启动游标，主机标记/释放其等待，原生停止通知结束该任务。模型响应来自回环服务。 |

开发阶段在 macOS 执行了 `cargo check --locked --offline --tests --example backend_suite`。上表两条运行命令及供应商版本结果留待统一验收。

## 仍需移植或联调

- `packages/provider-acp/src/server/adapter.ts:10240`、`provider-cursor/src/server/adapter.ts:3396`、`provider-opencode/src/server/v2/adapter.ts:5271` 均定义自己的任务标识和匹配方式。`AntigravityAdapterV2.test.ts:1777` 还断言跟踪过的命令可匹配。Kar 当前等待入口支持 Claude。其他适配器的公开契约是实现缺口；逐个使用该供应商的真实命令与工作台复验。
- `ThreadManagementService.ts:1743` 按子线程去重递归停止应用托管的委派任务，包括远程子环境、已有结果后的后续工作和部分失败汇总。`ClaudeAdapterV2.ts:14892` 处理原生子代理停止，`provider-opencode/src/server/v2/adapter.ts:5299` 实现该适配器的任务停止。Kar 当前单个工作流停止尚未覆盖这些调用链。
- `RestartBackgroundNote.ts:232,258,433` 区分用户主动取消和进程丢失，并按是否等待决定何时把中断说明送回模型。Kar 在原生进程退出或控制器重启后，将失联说明和首次承载它的输入一同持久化。带等待标记且前台已完成的任务可自动唤醒；普通后台命令在下一次用户输入时告知模型。主动停止不触发恢复说明。协议候选覆盖发送次数、原因、原用户输入和主动关闭，供应商实机待统一复验。
