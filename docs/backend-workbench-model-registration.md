# 工作台模型工具注册

Kar 的 HTTP 工作台启动后，按项目、Kar 聊天、当前供应商和一次打开操作签发限权租约。创建、恢复、更换供应商和检查点分叉会重新注册。分叉使用所选回合的历史供应商和目标聊天 UUID。Pi 读取父快照的临时控制器不获得子聊天授权。短期分叉进程关闭后回收该租约，客户端实际打开子聊天时重新签发。工作台主人令牌通过进程环境移除。结束控制器、注销、归档及闲置过期均按现有授权规则处理。

| Kar 的实际入口 | 注册条件和行为 |
| --- | --- |
| Codex app-server、Claude、Qoder | 使用本次控制器的 HTTP 服务。Codex 的自动注册允许原生工作台工具异步可用。Claude、Qoder 使用 Kar 的私有启动配置。 |
| Muse MSP | 原生 `initialize.grantedCapabilities` 含 `sessionMcp` 时在 `session/start` 或 `session/resume` 提交 `streamableHttp`，工作台项为 `optional`。未获授该能力时仍可正常聊天。用户显式指定的 MCP 继续校验能力、传输，并按 required 模式启动。 |
| 允许 MCP-over-ACP 的 ACP 运行时 | 若握手声明 `mcpCapabilities.acp`，优先使用本次连接的私有服务器 ID。Kar 原生处理 `mcp/connect`、`mcp/message` 请求/通知和 `mcp/disconnect`，校验每个连接、调用身份和资源边界。供应商不接收 HTTP 凭证。停回合取消未完成的调用，关闭控制器回收连接。 |
| ACP 的 OpenCode、Grok、Kimi、CodeBuddy | 协商 HTTP 后发送本次会话的服务地址及短期请求头。仅支持已知 stdio 的安装在 macOS 使用 Kar 进程桥。创建、轻量恢复、load 和协议可用的 fork 共用选择逻辑。 |
| Cursor ACP、Devin、Antigravity、Kiro、Fx | macOS 用 Kar stdio 进程桥。原生收到的命令仅含 Kar 程序和私有套接字路径。套接字服务核验内核给出的 UID、PID、进程出生时间和供应商父子进程树。Kar 本机服务代发有凭据的 HTTP 调用，沿用权限检查、工具发现及变更通知。Antigravity 依协商能力选择 stdio。Cursor 当前通过 ACP 接入。 |
| Kimi KAP、Factory、Qwen | KAP 的原生元信息须支持 `session_ephemeral_mcp`，失败回退 ACP 时重新协商。Factory 1.12.1 起使用原生会话配置。Qwen 0.26.0 起使用 HTTP，较早的已识别安装在 macOS 可用 stdio。 |
| Pi RPC | 1.4.0 起，需要当前安装的官方 npm SDK 或精确匹配的 companion、支持加载对应原生扩展的 Node。预检通过才注入，调用现有 Rust 扩展的注册入口。工作台自动注册没有获得原生回执时仍可聊天。用户显式要求注册时继续严格检查回执。 |
| DSH Web / ACP | macOS 上从 0.2.1-rc.5 起使用已验证的工作区启动 profile 和无环境变量的 stdio 桥。显式禁用 Node addons 时跳过自动注册。 |
| Morph、Cybion | 两个社区入口没有会话 MCP 传输声明，继续使用普通会话。 |

显式 MCP 与自动工作台项在不同的内存字段。重名时保留用户条目。ACP 仅在原生返回明确指向 MCP 的 JSON-RPC 参数错误时尝试移除自动工作台项并再打开一次。超时、身份错误及未得到回复的分叉不触发二次创建。macOS 的 stdio 桥需要正常 `kar workbench` 二进制和可验证的本机供应商子进程；嵌入式宿主或不满足条件的平台仍可使用协商过的安全 HTTP。

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。Muse 见 `packages/provider-muse/src/server/adapter.ts:1936`。上游选择 ACP 内嵌传输、HTTP 或 stdio 的顺序见 `packages/provider-acp/src/server/AcpSessionRuntime.ts:2360`，开场及再激活见 2456、2479、2704、2737 行。Cursor 上游的 SDK 注册见 `packages/provider-cursor/src/server/adapter.ts:216`、323 和 2945 行。Kar 的 Cursor ACP 调用本机受限转接；上述 SDK 行号用于定位行为。上游 Pi 创建/恢复见 `packages/provider-pi/src/server/adapter.ts:500`。

## 集中复验

- `cargo test --locked --test workbench_provider_chat -- model-tools` 启动真实 Kar 工作台，使用本地 MSP/ACP 协议夹具。Muse 通过限权 HTTP 凭证实际调用 `orchestrator_capabilities`，Kiro 对端按协商的内嵌 ACP 协议完整 connect、initialize、call、disconnect，逐一核对错误服务器和已关闭连接受到拒绝。Cursor 对端从供应商子进程启动 Kar stdio 桥调用同一工具。新建和重启后均核对项目、聊天、运行时及轮换的租约 ID。另验证闲置于树外的进程知道套接字路径仍不能使用、无 sessionMcp 的 Muse 照常聊天、旧 Fx 拒绝自动注册后继续、SDK 用户显式指定的服务器遭拒时返回错误。
- `cargo test --locked --test workbench_mcp` 覆盖正常 Codex 流程、短期令牌与主人令牌隔离、工作台权限、工具发现及撤销。`cargo test --locked --test workbench_handoff` 覆盖切换、重启和历史回合分叉的现有入口。
- 供应商实机复验需受支持的本机 CLI、可用模型和原生工具权限。运行 `kar workbench --root <隔离项目> --state <临时目录>`，创建该供应商的聊天，询问模型调用 `orchestrator_capabilities` 并输出 `provider_session`。比对真实工具结果与工作台返回的项目、聊天。重启后在同一聊天再次调用，要求两次 `thread_id` 相同，`provider_session.id` 不同。切换供应商时要求 `runtime` 改变；分叉后要求 `thread_id` 为子聊天 UUID。

本次开发仅执行 macOS 编译检查。以上协议场景和供应商实机命令留给集中验收，尚无本次运行结果。

Owner-installed ACP registry targets and saved local profiles receive the same chat-scoped grant on open and resume. Kar uses MCP-over-ACP when the actual initialize result advertises it. Otherwise macOS offers the birth-checked stdio bridge. A dynamic agent advertising HTTP still receives no workbench bearer or owner token. A typed native rejection of the optional MCP registration retries the same open without that entry. Explicit user MCP settings keep their normal error contract. The session journal records `kar/workbench-mcp-registration` with `accepted_at_open`, `native_refused`, or `no_transport`; acceptance of open parameters is distinct from the agent actually calling a tool. The `model-tools` fixture includes a revision-pinned local profile, in-band calls, macOS stdio, optional rejection and restart. Its protocol fixture does not establish support for any individual third-party agent.
