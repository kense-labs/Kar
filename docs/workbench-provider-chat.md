# Kiro、Fx 和 Muse 聊天复验

工作台对照提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。Kiro V3 和 Fx 使用各自的 ACP 设置，Muse 使用 MSP v1。运行时注册表驱动 `kar chat new` 和工作台创建聊天接口。

| 供应商 | 原生程序 | 原生设置及恢复 |
|---|---|---|
| Kiro | `kiro-cli acp --agent-engine=v3 --auth-method=cli` | 关闭 autopilot。再次启动时使用已保存的原生会话 ID 和 cwd。模型、代理和推理强度由原生配置方法确认。 |
| Fx | `fx acp` | 使用工作区权限模式和本机 profile MCP。新建和恢复都显式写入 ask 或用户选择的模式。 |
| Muse | `muse serve --trust-workspace` | 协商 MSP v1。保存客户端生成的原生 UUIDv7，恢复时确认会话 ID 和存活的投影。回合回执必须匹配提交的命令 ID。会话级 MCP 需要原生授权，仅可发送 streamable HTTP 配置。 |

## 协议回归

`cargo test --locked --test workbench_provider_chat` 从三个真实 Kar CLI 入口分别新建、跨进程恢复并读取历史。测试子程序扮演原生运行时，校验启动参数、工作区、设置和消息身份。

## 已安装供应商

逐个选择本机已登录并可发送模型请求的程序。在默认搜索路径之外安装时可分别设置 `KAR_KIRO_BIN`、`KAR_FX_BIN` 或 `KAR_MUSE_BIN`。记录程序版本、平台和命令退出状态。

```sh
cargo test --locked --test workbench_provider_chat -- installed kiro
cargo test --locked --test workbench_provider_chat -- installed fx
cargo test --locked --test workbench_provider_chat -- installed muse
```

每条命令创建独立临时工作区和聊天存储，通过真实 Kar CLI 发出两轮请求。第二轮在新进程中恢复原生会话，提示只要求复述第一轮随机值。断言读取两轮各自的助手输出。

## Muse 的四项能力边界

以下对照使用工作台固定提交和它锁定的 `@muse-code/sdk@1.4.6`。证据来自生产适配器源码。测试文件定位用于复核预期，尚未在本轮执行。[依赖锁定](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/package.json#L32)

| 用户操作 | 固定 T3 的行为 | 当前 Kar 的可观察结果 |
|---|---|---|
| Muse 进程在回答中退出，重新打开会话 | 适配器声明活动回合不支持原生恢复。进程退出时将正在进行的回合标为失败，只有空闲时退出才允许当前适配器重新启动。重新打开已保存的会话会调用 `session/resume`，返回的 `activeTurnId` 本身不能让工作台接管原回合。[回合能力](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L128-L136) · [退出处理](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L1125-L1147) · [恢复会话](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L1955-L1986) | 可按已保存的 ID 重开会话。Kar 根据回复中既有的 `activeTurnId` 识别旧回合，保留原历史，并将新提交匹配到新的原生命令 ID。旧回合的延迟完成事件不会结束新回合。在当前连接已观察到开始的回合遇到意外退程时，Kar 记录失败原因。[打开校验](../src/session/muse.rs) |
| 选中已完成回合，创建 Muse 原生子会话 | 线程、回合和子代理分叉均声明为不可用。生产分叉入口直接返回错误；源码记录已完成回合的切点遭原生 `InvalidCut` 拒绝，并注明可以使用可移植交接。这里没有覆盖其他原生切点或其他 Muse 版本的实机证据。[能力声明](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L119-L126) · [分叉入口](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L2497-L2513) | 原生分叉请求返回不支持，父聊天继续使用原来的原生会话。[运行时分叉入口](../src/runtime.rs) |
| 没有用户发消息，Muse 自行开始后台回合 | 有当前聊天和续跑服务时，适配器会向工作台登记续跑请求，缓存该原生回合的事件。工作台接纳请求后沿用已有回合 ID，回放缓存，无须再次发送 `turn/start`。宿主退出会清空等待中的请求。[登记与归属](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L1438-L1474) · [接纳已有回合](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L2158-L2167) | 已附加的空闲会话收到 `turn/started` 时，会按原生会话与回合 ID 建立可持久查询的自主回合，模型输出进入普通时间线。前台忙时，加入回合的输出属于前台，其完成事件只结束加入的原生回合。[回合过滤](../src/transport/muse.rs) |
| 工作流子代理在父回合结束后请求审批 | 适配器把带 `subagentOrigin` 的审批挂在线程上，可跨越父回合结束；普通模式下用户仍可应答。完全访问模式下适配器尝试选择批准，原生拒绝且该请求仍待处理时才显示。会话能力另声明待审批请求不跨宿主重启保留。[审批归属](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L1183-L1215) · [应答与回退](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L1313-L1333) · [重启边界](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L112-L118) | 父回合结束后，已附加的会话继续接收该会话的子审批。工作台以现有 `approvals=host` 配置展示请求，通过原生授权选项应答；`deny` 配置选用原生拒绝选项。Kar 当前只提供这两种权限模式。新连接清空上一连接的待审批请求。[审批筛选](../src/transport/muse.rs) |

Kiro 和 Fx 的恢复取决于本机发行版协商到的 ACP 会话能力。运行时拒绝时，Kar 通过普通入口返回原因。

## Muse 后台行为复验

`cargo test --locked --test workbench_provider_chat -- muse-continuation` 启动正常 Kar 工作台服务，使用隔离工作区和 MSP 替身。两个 HTTP 客户端核对前台加入回合、自主回合的输出与稳定 ID、父回合结束后的子审批、重复通知与重复决定。随后重启工作台，验证响应之前到达的旧原生回合通知、恢复同一会话、新输入正常完成以及历史只保留一份。最后让原生替身在活动回合中退程，检查持久时间线中的失败分类。

实机复验需要已登录的 Muse、可产生后台任务及子代理权限请求的工作流。以 `approvals=host` 创建 Muse 聊天，启动该工作流，在另一个客户端读取 `GET /projects/{project}/chats/{chat}/snapshot`。记录 `history.turns` 中新增的 `native_continuation` 和 `native_turn_id`，通过 `POST /projects/{project}/chats/{chat}/requests/respond` 携带当前请求 ID、游标和决定，核对 Muse 的实际结论。关闭宿主后重新附加，检查旧请求已失效，新消息在同一原生会话继续。普通聊天实机命令只覆盖两次前台提问；后台工作流单独取证。
