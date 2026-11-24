# Kar 后端功能复核 · 2026-11-24

首次复核的 Kar 基线为 GitHub `main` 的 `d02239d9234a`、上游运行时 `v0.130.1`（`2b15859a13dc`）及 T3 Code `2808e878de91`。以下结论来自固定提交的实际处理逻辑。列出的操作供独立验收复现，当前候选的统一实测仍待执行。

## 11 月 25 日复核与验收交接

上游运行时固定在发布版 `v0.130.1`（`2b15859a13dc`），起点是 `v0.129.1`（`2582c9018b53`）。工作台固定在 `2808e878de91`。首次公开主线对照以 `d02239d` 为基线。对齐单位是一个用户、模型或外部客户端在正常入口能完成的操作。

| 对照的操作 | 当前实现 | 独立验收的关键观察 |
| --- | --- | --- |
| 双端打开聊天，断线后续收，重启回原会话继续聊天 | 事件以聊天和游标定位。队列用稳定输入 ID 和原生回执对账；Kimi、Codex 的原生记录可用于恢复。 | 两个客户端在回合结束前读到同一事件。分别在原生接收前、接收后而 Kar 尚未记回执时强杀，按原生记录数实际发送次数；证据不足的输入停在待确认，后项不抢发。 |
| 从指定回合继续改文件 | `b3cd44f` 可在原聊天联合恢复文件和模型上下文，`8fccd99` 保留继承的附件。原生分叉、文件差异和工作树交接也是单独可调用的操作。 | 制造三个不同文件版本和三段模型上下文，分别检查工作区字节、聊天 ID、模型下一轮能复述的事实及同命令重试。 |
| 模型使用项目、Git、PR 和工作流 | 工具目录、发现及正常调用已接入。当前工作树可以创建或复用 PR 并关联聊天（`242fd6a`），PR 监视可唤醒原聊天，审阅指引固定到不可变 head。非 GitHub 写操作按原生驱动处理。 | 用获授权的测试仓库核对发布 head、base、审阅、CI、重试次数和拒绝结果。按驱动分别记账。 |
| 读取另一台 Kar 的聊天，或委派它执行任务 | `0ebc2c9` 接入有界历史、单条详情、快照和续收。读、执行授权分开。远端执行 `194c43c` 需要对方批准具体项目和能力，结果回传父聊天。 | 两台 Kar 各用独立状态。只读链接不能执行，断线重试不新建第二个任务，撤销链接结束流。 |
| 连接供应商、外部客户端和桌面浏览器 | `4606209` 为内置与动态 ACP 按协商注册工具；外部 OAuth 提供同意、PKCE 和撤销；`8cbba10` 支持浏览器帧 ACK、输入和选定文件上传。配置、热加载快捷键、Codex 和 Antigravity 的受管安装有独立入口。 | 分开检查原生拒绝、HTTP 协议、实际浏览器、供应商账号和真实中继。 |
| 了解后台任务以及原生子代理归属 | Claude、Antigravity 与指定版本 OpenCode 的等待路径有实现。OpenCode 停止还要求确切 shell 身份、本机进程归属及版本。`57c8d2c` 暴露实际传输的子代理归属类型。 | 分前台已结束但后台仍活动、原生数据库重挂、PID/名字复用及两会话隔离。 [后台任务协议及版本](backend-opencode-background-shells.md) |

[逐项清单与测试命令](backend-source-parity.toml) · [模型工具的调用名称和动作](backend-workbench-mcp-map.tsv) · [macOS 交接](backend-workbench-final-acceptance.md)

**源码对照已执行。** 本轮运行 Kar 的 Rust 静态覆盖检查器，对固定上游 Git 对象复核了 233 个来源文件、192 项断言映射和 50 项 CLI 映射。工作台另核对了 16 组共 94 个业务工具声明及独立的工具搜索；逐行表新增可用的 Kar 模型调用名称或动作。一个工具通常包含多个子操作，入口映射不能证明每个供应商或平台实测通过。

**复核后已合入的修复**

- 安装升级的主机预检、拒绝分类、原生退出码与输出，以及按探测结果区分已安装/新安装/更新/版本未变已接入（`a5af388`）。 [安装升级回执](backend-installer-receipts.md)
- 委派任务按父回合和策略唤醒、合并尚未发送的通知、显式确认及处置和原生回执分类已接入（`263079e`）。 [委派交付](backend-delegation-receipts.md)
- 工作区准备可在同一个 run 上显示 worktree/setup 阶段、保留失败、重启后重试和幂等放行（`b682eae`）。同步脚本退出成功才放行，异步脚本启动后可以继续。 [准备和重试的入口](backend-prepared-runs.md)
- 双端终端按已连接观看者取最小行列、选取唯一的设备查询应答端。另有被动观察与终端目录元数据流（`36f89aa`）。 [多端终端复验](backend-terminal-views.md)
- Codex 和 Claude 的额度重置已有工作台 HTTP 入口及持久领取回执（`eab019b`）。 [额度重置与结果核对](backend-quota-http.md)
- ChatGPT 独立档案支持加密导入、重新连接和交接状态订阅。真实账号的浏览器授权、模型调用与撤权仍待实测（`a93cb69`）。 [账号交接](backend-chatgpt-handoff.md)

**仍有可观察的差异。**

- 固定工作台对 Cursor 后台 shell 有专门的等待与停止回调。Kar 的 Cursor ACP 未取得等价原生句柄。 [固定 SDK 补丁](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/patches/%40cursor__sdk%401.4.2.patch)
- 服务重启后，符合条件的普通提问还不能转换为可回答或关闭的新消息，未确认送达的答案也缺少持久失败记录。
- 富文本搜索缺部分可见文件标签、行号和引用内容。外部工作流里程碑缺父运行 ID 和部分结束原因。运行时安装发布线缺少面向用户的说明字段。

**本轮已接入的其他操作**

侧聊天随父聊天归档、恢复及删除，保留独立分叉和既有手动归档（`ca1731c`）。MCP App 的工具与资源请求绑定到原生卡片，按回合保存模型上下文（`96ee4e7`）。外部 MCP 客户端可发现、轮询和续收 12 类里程碑，最近 1,000 条跨重启保存（`0676d3f`）。Codex 和 Claude 的活动会话可完成单个 MCP 服务登录，Codex 可向供应商上传诊断并返回回执（`61650ba`）。桌面更新任务支持准备、提交及重启后与宿主对账（`917bdf4`）；签名发布包在真实桌面宿主安装仍需实测。 [侧聊天生命周期](backend-workbench-thread-family.md) · [MCP App 卡片](backend-mcp-app-session.md) · [原生会话控制](backend-provider-session-controls.md) · [桌面宿主协议](backend-workbench-configuration.md)

**待逐项确认的边界**

远端委派的断网及重启仍需双机验证。旧记录缺少父回合、回执或主动停止原因时无法回填确证。供应商运行模式切换和多聊天 PR 关联的子操作仍有待核对项。

**运行时的独立差异。** 对共享 Morph Console 和远端 Cybion 聊天，原实现会在释放本地控制权时同时发出退出事件。固定上游只在实际停止进程后报告退出。Kar 在 `ee61e23` 保留原关闭回执，并区分控制器释放和原生退出；接手方可用 `cargo test --locked --test cybion` 和 [Morph 实机用例](../tests/morph_live.rs) 对照。安装计划从 `1a01ff5` 起给出来源、程序、参数、网络及执行身份。运行时 SDK 的历史成员映射仍有标为「部分契约」的行，包含同一操作的多种导出、Rust 类型差异和已在后续提交修正的旧结论；最新的安装与更新回执见上方修复条目。 [运行时契约映射](backend-sdk-map.tsv)

**实测条件。** 当前阶段只核对功能和编译。真实账号、真实 Cursor/OpenCode/Claude/Codex 版本、受管中继、移动设备以及允许测试写入的代码托管仓库各有自己的运行条件。缺少环境时保留待验状态；原生接口有明确拒绝时记录供应商和版本。Linux 后移，Windows 实机不在本轮内。

## 功能修复后的对照

固定对照版本为上游运行时 v0.130.1 和工作台 2808e878。检查以用户或模型实际可调用的行为为单位。工作台的 16 个业务工具组共有 94 个声明，另有工具搜索。一个声明可以有多个动作，也可由 Kar 的多个入口实现。[上游工具与处理函数索引](backend-workbench-mcp-map.tsv)

| 用户操作 | Kar 实现及证据 | macOS 复验入口 |
| --- | --- | --- |
| 在原聊天回到所选回合并继续写文件 | `b3cd44f` 先检查文件冲突，按同一个命令 ID 提交模型绑定、工作区位置和回退记录；`8fccd99` 保留继承的附件。 | `cargo test --locked --test workbench_handoff`，检验意外中止、双客户端事件与模型实际恢复的上下文。 |
| 重启后继续原 FIFO，保留用户不确定的投递 | `b04a7ed` 使用有归属的 Kimi/Codex 原生记录确认队首。缺少证据时暴露待确认状态，后项停止派发；工作树交接在 `2794c14` 使用同一边界。 | `cargo test --locked --test provider_queue_crash`，按 [供应商环境表](backend-macos-tests.tsv) 配置真实 CLI。 |
| 动态 ACP、原生会话和订阅 | `e890131` 允许只支持新建的代理在进程存活时工作；`5a207ee`、`a343998`、`3d74284` 提供本地启动器、模型菜单和原生登录。SDK 的文本聚合与安装诊断分别见 `0d62edc`、`2300804`。 | `cargo test --locked --test workbench_acp_registry`、`cargo test --locked --lib merge_by_native_message`、`cargo test --locked --test protocol installation`。 |
| 项目、Git 与协作 | `6b496b6` 管理和克隆项目并启动聊天；`4311ce3` 在受管工作树运行初始化脚本；`82ce2d6` 保留分叉和合回；`73075c7` 恢复结构化输出要求。 | `cargo test --locked --test workbench_mcp`，同时覆盖工具查找、直接调用、范围限制和重复命令。 |
| 模型在工作台使用其他服务 | 浏览器/桌面/设备见 `6907400`、`5bd94a9`；附件、私密输入、环境配对、主题和反馈见 `3dda5e8`、`f08087d`；工作流 KV 及信号见 `8163bdb`；本机委派、批量新聊及排程见 `d0affd2`。 | `workbench_mcp`、`workbench_workflows`；外部账户、真实设备和远端 Git 托管须提供各自的授权测试环境。 |
| HTTP 文件与追踪 | `09a0dc4` 增加受签名约束的附件 Range / If-Range、静态文件条件读取和浏览器跟踪接收与转发。 | `cargo test --locked --lib conditional_and_partial_downloads_validate_the_same_entity`，另以有效及过期的签名地址对照。 |

清单内的“待 macOS 统一实测”保留运行结果空缺。协议回放、真实供应商程序连接本地模型、真实账户和真实设备分别记录。以下历史表格仅描述其标题或正文标明的旧提交。

## 历史快照 · 候选 3ab6da6 与 cbd3c93

本次重新查询 GitHub 的 `refs/heads/main` 得到 `d02239d9234abb382d0a35c084d25dacc557738c`。首表核对本地候选 `3ab6da6`，完整模型工具组核对到 `cbd3c93`。运行时源码通过 `git show 2b15859:<文件>` 读取，工作台源码通过 `git show 2808e87:<文件>` 读取。以下判断限于用户或宿主实际调用的后端入口。

| 操作 | 本地候选和上游的实际差别 | 独立复验 |
| --- | --- | --- |
| 原聊天同时回退模型和文件 | Kar 有保留聊天 ID 和文件的 `rewinds`，有只恢复文件的 `rollback`，也能用 `forks` 恢复文件和选定的模型历史并创建子聊天。T3 对可恢复的独立工作树在同一个聊天回滚请求中处理模型回退和文件恢复。Kar 的三个入口均未提供这个组合。 | 在独立工作树完成三轮不同写入，从第二轮恢复。核对原聊天 ID、模型实际看到的轮次、磁盘字节，以及恢复失败时的回执。 [T3 回滚执行](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/CheckpointRollbackService.ts#L415) · [Kar 工作台处理](../src/workbench/chat_controls.rs) |
| 用授权模型管理 PR、CI 和聊天关联的 Issue | Kar 的托管 HTTP 接口与模型的 MCP 入口分别路由。候选已给模型提供 Issue 列表、详情、创建、评论、关闭和重开。模型工具没有 T3 的 PR 审查、合并、读取及重跑 CI，也没有绑定、解绑、列出当前聊天关联 Issue 的工具。 | 用项目授权模型搜索相应工具，再调用同一 MCP 会话的 `tools/call`。独立检查普通工作台接口，防止混淆两个调用方。 [T3 PR 和 CI](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/pullRequests/handlers.ts#L1181) · [T3 Issue 关联](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/issues/handlers.ts#L269) · [Kar 工具注册和分派](../src/workbench/mcp/toolkit.rs) · [Kar 托管接口](../src/workbench/forge/routes.rs) |
| 模型操作预览及工作流键值存储 | T3 提供模型可调用的预览导航、快照和录制控制。其工作流工具还能按键前缀读取持久值、写入和删除。Kar 工作台有预览服务，候选模型的工作流工具也能启停和恢复运行，但这些预览工具及工作流存储操作未在 MCP 注册。 | 在授权会话中发现并调用预览操作。写入一个唯一工作流键，重启后按前缀读回，再删除。 [T3 预览](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/preview/handlers.ts#L215) · [T3 工作流](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/workflow/handlers.ts#L25) · [Kar 工作流工具](../src/workbench/mcp/workflow.rs) |
| 订阅并按原生消息分组合并增量文本 | 上游运行时可选择合并连续正文或可读推理，按会话、代理、消息和原生文本阶段切分，静默超时可触发刷新。Kar 的公开事件订阅按游标逐事件返回；正文增量只包含 text，缺少相应消息 ID 和阶段。原始协议帧仍可读取。 | 让 Codex 同一轮输出两条不同消息和阶段，中间暂停，核对公共订阅返回的边界、最后游标和实际刷新时机。 [上游增量合并](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/observe/events.ts#L133) · [上游 Codex 消息](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/codex/messages.ts#L15) · [Kar 事件和订阅](../src/stream.rs) |
| 区分宿主依赖不满足与原生程序版本不支持 | 上游安装契约区分缺少依赖、宿主或 Node 不受支持等状态。Pi 的探测实际会在依赖缺失时返回 dependency_missing。Kar 的安装错误枚举提供程序未就绪、无法识别和版本不支持三种分类。其 Rust 启动方式及依赖不同；有影响的是宿主程序可以读到的诊断。 | 在隔离环境为对应的启动模式移除必需依赖，核对安装快照的机器可读原因以及能否向用户呈现修复动作。 [上游安装契约](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/contracts/installation.ts#L65) · [上游 Pi 探测](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/pi/installation.ts#L77) · [Kar 安装快照](../src/installation.rs) |

本地已接入同一聊天只回退模型上下文（`f5f1273`、`3ab6da6`）、模型 Issue 与工作流操作（`4553498`、`d9f0442`）、第三方 ACP 注册表（`2c53f5b`）、Muse 原生主动回合和父回合结束后的子代理审批（`a33f343`）。这个候选在 macOS arm64 上已通过 `cargo check --locked --offline --all-targets`。真实供应商已收件、Kar 尚未写回执时强杀 Kimi KAP/Codex 的证据仍须单独取得。统一实测由接手方执行。

后续集成的模型工作树交接支持在模型自己的活动回合内提出请求，在回合结束后将同一聊天迁入经授权的工作树（`ee01eae`）。动态 ACP 在进程退出后允许卸载，并可按记录的分发版本重装（`cbd3c93`）。动态 ACP 的其余差异涉及工作台配置本地命令或覆写启动器、代理登录/登出、动态模型菜单，以及仅能新建会话的代理。最后一种在 T3 可先聊；Kar 的持久聊天创建时即要求恢复能力。[逐项行为与固定源码](backend-workbench-acp-registry.md)

### 工作台模型工具覆盖

在候选 `cbd3c93` 核对 T3 实际注册的 16 个业务工具组（94 个工具声明），另有 1 个独立的工具搜索。按授权模型能否发现和调用对应操作计数。在同一工具内以参数选择的动作也逐一检查，工作流的 1 个声明就包含多个动作。

| 结果 | 工具组 | 可复核的边界 |
| --- | --- | --- |
| 搜索机制已接入（1） | 工具搜索 | 查询已注册工具、按会话加载和通知工具列表变化均有处理。能找到的功能取决于 Kar 实际注册的目录。 |
| 部分接入（6） | Issue、工作流、工作树、项目、编排、线程管理 | 示例包括 Issue 的 5 个已接入操作、工作流运行控制、工作树交接、项目与聊天读取、Claude 后台等待，以及队列编辑和撤销。项目创建/克隆、工作台委派、跨聊天控制、待处理请求等尚有模型侧缺口。 |
| 未找到对应工作台模型入口（10） | 预览操作、预览列表/关闭、PR、附件、设备、环境设置、反馈、HTML、向用户发消息、主题 | 涵盖 PR 的 18 个工具声明、预览操作的 12 个声明。例如普通客户端能打开预览或查看 PR，模型不能据此在当前工作台 MCP 会话调用这些操作。 |

作为参照，GitHub `d02239d` 的同一注册函数声明的 8 个工具是能力查询、列项目、读项目、列聊天、读聊天、队列、工具搜索以及 Claude 后台等待。各项还受运行时、租约和工具搜索模式限制。该远端版本没有候选中新增的模型 Issue、工作流和工作树操作。[GitHub main 对应提交的注册表](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/mcp/toolkit.rs)

汇总范围仅为模型的 MCP 工具。Kar 的 HTTP 服务将 `tools/list` 和 `tools/call` 分派到同一个受权限约束的工具注册表；另一个 stdio MCP 服务管理自己的子代理，未转发这些工作台调用。可先在具备相应权限的 Codex/Claude 会话中查询工具，再直接调用，并核对实际服务回执。[T3 注册清单](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/McpHttpServer.ts#L1434) · [95 个工具声明的逐项定位](backend-workbench-mcp-map.tsv) · [Kar 工作台 MCP 分派](../src/workbench/mcp/mod.rs) · [Kar 独立子代理 MCP](../src/mcp.rs)

## 上一轮复核记录

本地功能候选为 `7111a73`。首次报告的 16 个操作已有对应实现。第二次复核还发现原聊天仅回退模型上下文、第三方 ACP 注册表管理、模型工具的进一步操作以及 Muse 后台事件存在差异。输入投递历史已在本次复核期间补入候选。以下相对路径对应本地分支 `kar/functional-repairs`。

开发检查在 macOS 27.2.1 arm64、rustc 1.100.0 执行。包含上述输入投影的修复分支通过了 `cargo check --locked --offline --all-targets` 和 `git diff --check d02239d..HEAD`。正式运行级复验留给接手方。

## 本地修复与 GitHub 版本

GitHub `main` 复查时指向 `d02239d9234a`。下表给出本地分支 `kar/functional-repairs` 对首次复核中 16 个具体操作的实现提交和独立验收入口。提交及开发编译能证明代码已接入。供应商返回、真实强杀、断线和重投仍需由统一验收取证。

| 首次复核中的操作 | 本地提交 | macOS 复验入口 |
| --- | --- | --- |
| 1. Pi 图文及仅图片回合的强杀恢复 | `9235948` | `tests/native_live/pi_history.rs` 的图文恢复场景 |
| 2–3. 访客读取可见附件、主人接纳后原生带图投递 | `62d6a4c`、`c6c85d0` | `workbench_remote_access` 和 `chat_restart` 的分叉用例 |
| 4. 从只有非 origin 远端的项目 PR 建工作树 | `9a7367b` | `workbench_forge` |
| 5. 等价修改计划后保留到期任务 | `f35cb55` | `workbench_workflows` |
| 6. 已知 Kimi KAP 版本开会话前的能力 | `943f651` | `cargo test --locked --lib capabilities::installed`，另用指定二进制查看能力 |
| 7. 立即、轮后、空闲和中断合并投递 | `c5d93e5`、`f381a7f`、`78addec` | `tests/protocol/delivery.rs`，含根回合、并发、拒绝和退出切点 |
| 8. 输入的用户、通知或自动化来源 | `c6fa4fd`、`5742659` | `cybion` 中的来源回放及原生请求比对 |
| 9. Pi 项目包预检和默认离线启动 | `56682fe` | `pi_provider_live` 的 `packages::native_rpc_package_preflight_and_install_opt_in`，需本机 Pi |
| 10. 提供方不能原生分叉时从选定回合便携交接 | `71bd1b3` | `workbench_handoff` 的 `portable_fork`；另对严格原生策略复验 |
| 11. Codex 父聊天仍在生成时分叉早先已完成回合 | `bb83299` | `chat_restart` 的独立工作树场景 |
| 12. 同一聊天切换同运行时、同模型的不同实例 | `d3d9e17` | `workbench_handoff` 的 `named_instances` |
| 13. 把失联后台命令说明发回模型、区分主动关闭 | `cec3da6` | `chat_restart` 中的工作台后台任务场景 |
| 14. 模型创建 Issue 及发表评论 | `634e11f` | `workbench_mcp`，同时核对计划模式、撤权及重复命令 |
| 15. 选择 Kiro、Fx 和 Muse 创建及恢复普通聊天 | `3f45a88`、`4f13d55` | `workbench_provider_chat`，真机分别选 `installed kiro`、`installed fx`、`installed muse` |
| 16. 工作台本地和云端语音转录 | `708d683` | `workbench_voice`，本地需转录引擎及模型，云端需测试账号 |

Muse 的独立协议还要区分具体行为。固定 T3 对已结束回合的原生分叉明确拒绝，但能接收自主启动的回合，并在父回合结束后继续处理后台子代理审批。Kar 已有普通聊天和前台审批，这两种后台操作仍有差异。[Muse 逐项源码定位与实测入口](workbench-provider-chat.md)

## 清单内的操作

下列差异直接影响原 12 项或 8 项中的用户操作。

| 操作 | 当前差异 | 复验方法与固定源码 |
| --- | --- | --- |
| Pi 图片回合的强杀恢复 | Pi 原生文件已记下助手回复，Kar 只来得及记录用户回显时，恢复器要求用户消息的每个内容块都是文本。图文和仅图片输入均匹配失败，缺失的回复无法补回，原回合可能被标为中断。 | 分别用一张 PNG 和文字加 PNG 制造上述切点。从原聊天恢复，检查回复、终态、图片归属和重复消息数。 [上游运行时 session.ts:55](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/pi/rpc/session.ts#L55) · [Kar pi.rs:23](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/log/pi.rs#L23) · [Kar session.rs:2094](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/session.rs#L2094) · [Kar log.rs:1505](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/log.rs#L1505) |
| 访客读取原聊天附件 | 主人先在普通聊天发图，再分享给访客。T3 能按分享范围读取原消息及可见的分叉祖先附件。Kar 访客视图只从访客分享登记表补附件，原消息的图片不会显示。 | 分享一条已有图片的聊天和从该轮分叉的聊天。访客应能读取可见附件，分享撤销后再次读取应被拒绝。 [T3 GuestShareService.ts:716](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/guest/GuestShareService.ts#L716) · [Kar guest.rs:409](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/access/guest.rs#L409) · [Kar retention.rs:247](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/access/guest/retention.rs#L247) |
| 接纳访客图片 | T3 把已验证的访客附件绑定到实际发送的消息。Kar 把文件路径追加到正文，再调用纯文本入队接口。主人的普通带图消息已有原生图片通路。 | 访客发 PNG，主人接纳，检查支持图片的模型实际收到的图片数、内容和对应消息 ID。重复接纳同一提议应只投递一次。 [T3 ThreadMessageIntake.ts:88](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/ThreadMessageIntake.ts#L88) · [Kar guest.rs:619](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/access/guest.rs#L619) · [Kar guest.rs:674](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/access/guest.rs#L674) · [Kar queue.rs:217](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/queue.rs#L217) |
| 从 PR 创建工作树 | Kar 查询仓库时能识别唯一远端 upstream，但 PR 的基准或头提交尚未下载时，抓取逻辑固定使用 origin。新 PR 在这种仓库里可以查到，创建工作树会失败。 | 使用仅有 upstream 远端的仓库和本地不存在的新 PR 提交。直接从 PR 准备工作树，核对检出提交。 [T3 GitManager.ts:1186](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/git/GitManager.ts#L1186) · [Kar mod.rs:505](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/forge/mod.rs#L505) · [Kar review.rs:62](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/forge/review.rs#L62) |
| 定时任务等价编辑 | T3 把 9:00 和 09:00 以及顺序不同的同一星期集合视为相同计划。Kar 保存时比较原始字符串与星期列表。任务已到期、调度器尚未领取时，等价编辑会重算下次时间，可能跳过本轮。 | 在可控时钟下保存 9:00，每日任务到期后、扫描前，把时间重存为 09:00。宽限期内扫描应补跑一次。 [T3 Schedule.ts:54](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/scheduledTasks/Schedule.ts#L54) · [T3 ScheduledTaskService.ts:1592](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/scheduledTasks/ScheduledTaskService.ts#L1592) · [Kar mod.rs:391](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/workflows/mod.rs#L391) |
| 已知 Kimi KAP 版本的开会话前能力声明 | 对 2.7.2-botiverse.1，上游预先声明图片、插话、停止任务和手动压缩。Kar 取 KAP 与 ACP 声明的交集，在安装能力列表中省略上述字段。只依赖该列表的客户端会漏掉可用操作。 | 对指定发行版本先查安装能力，再打开实际 KAP 会话读回能力。其他 Kimi 版本按各自握手分别记录。 [上游运行时 options.ts:33](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/kimi/options.ts#L33) · [Kar installed.rs:32](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/capabilities/installed.rs#L32) · [Kar installed.rs:450](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/capabilities/installed.rs#L450) |

## 运行时公开行为

这些是 SDK 调用方或使用 Pi 的宿主可以观察到的差异。普通聊天的 FIFO、撤回和插话各有独立实现。

| 操作 | 当前差异 | 复验方法与固定源码 |
| --- | --- | --- |
| 选择投递时机并合并被撤回的输入 | 上游可指定等到空闲、轮后投递，或在无法插话时限时等待、中断并按队列顺序合并投递，回执列出涉及的输入 ID。Kar 的 deliver 在活动回合优先插话，其次排队，缺少这组策略和合并回执。 | 在不可插话的活动回合排一条，再按 interrupt 送一条，核对原回合结算、合并后的模型输入及返回的 ID。 [上游运行时 deliver.ts:72](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/shared/deliver.ts#L72) · [上游运行时 deliver-interrupt.ts:156](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/shared/deliver-interrupt.ts#L156) · [Kar session.rs:1650](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/session.rs#L1650) · [Kar stream.rs:1021](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/stream.rs#L1021) |
| 调用方标注输入的来源 | 上游允许调用方将消息标为用户、通知或自动化，另附来源名称，写入请求记录。Kar 的公开 prompt、queue 和 steer 接口可带 ID 和图片，未暴露等价的来源元数据。 | 从宿主提交两条正文相同的用户和自动化消息，核对回放能否区分来源，且模型没有额外收到这段元数据。 [上游运行时 input.ts:15](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/contracts/input.ts#L15) · [上游运行时 session.ts:184](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/codex/session.ts#L184) · [Kar input.rs:237](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/input.rs#L237) · [Kar session.rs:1540](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/session.rs#L1540) |
| Pi RPC 默认包安装策略 | 固定上游在默认配置下预检缺失或需替换的包，并以离线参数启动 Pi。Kar 有项目可信设置，常规 Pi RPC 启动缺少这项预检及离线默认值，具体包处理结果交给 Pi。 | 在隔离目录配置一个尚未安装的包，对比默认启动返回、启动参数与包目录变化。 [上游运行时 project.ts:16](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/pi/project.ts#L16) · [上游运行时 open.ts:99](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/runtimes/pi/rpc/open.ts#L99) · [Kar session.rs:488](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/session.rs#L488) · [Kar instructions.rs:69](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/options/instructions.rs#L69) |

## 工作台的其他用户可见差异

这些能力应与原清单的基础场景分别评估。例如，明确要求原生分叉时，要保留原生身份与历史的语义。

| 操作 | 当前差异 | 复验方法与固定源码 |
| --- | --- | --- |
| 没有原生分叉时从选定回合继续聊 | T3 对支持上下文交接的提供方可用截至所选回合的历史创建新聊天。Kar 的回合分叉要求支持的原生协议和分叉边界，例如 Cursor 会在协议检查处被拒绝。 | 在 Cursor 完成三轮后选中第二轮。检查能否创建可续聊的新聊天，以及第三轮内容是否被排除。另行验证真正的原生分叉。 [T3 CommandPolicy.ts:454](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/CommandPolicy.ts#L454) · [T3 adapter.ts:164](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-cursor/src/server/adapter.ts#L164) · [Kar fork.rs:355](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/fork.rs#L355) |
| 父聊天仍忙时从较早回合分叉 | T3 的 Codex 适配器在有可靠原生边界时可从忙碌父聊天的已完成回合分叉。Kar 取得父工作区租约并检查父回合，父回合活动期间会返回忙。 | Codex 第三轮生成时选中已完成的第二轮，用独立工作树创建子聊天并检查父子历史。 [T3 CommandPolicy.ts:438](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/CommandPolicy.ts#L438) · [T3 CodexAdapterV2.ts:311](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts#L311) · [Kar fork.rs:405](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/fork.rs#L405) |
| 同一运行时和模型之间切换供应商实例 | T3 以实例 ID 选择目标适配器，允许配置不同的同类实例。Kar 交接请求只有运行时、模型等字段，当前运行时和模型与目标相同时直接返回目标已活动。 | 配置同一运行时和同一模型的两个实例，尝试在同一聊天中明确选取第二个实例。 [T3 ProviderSwitchService.ts:67](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/ProviderSwitchService.ts#L67) · [Kar handoff.rs:10](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/handoff.rs#L10) · [Kar handoff.rs:404](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/handoff.rs#L404) |
| 把后台任务的失联说明送回模型 | Claude 后台命令在控制器重启后失联。T3 会把任务信息带入后续模型输入，并按历史跟踪投递。Kar 能通过工作台查询失联任务，普通续聊输入缺少这段说明。 | 普通后台命令失联后在原聊天追问，再次重启。检查后续模型实际收到的任务身份及说明次数。 [T3 RestartBackgroundNote.ts:142](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/RestartBackgroundNote.ts#L142) · [T3 ProviderTurnStartService.ts:2138](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/ProviderTurnStartService.ts#L2138) · [Kar background.rs:46](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/background.rs#L46) · [Kar handoff.rs:243](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/chat/handoff.rs#L243) |
| 在模型会话里调用工作台工具 | 例如 T3 的模型可通过授权的内置 MCP 工具创建 Issue、发表评论。Kar 普通托管 API 已有相关操作，工作台 MCP 的工具注册及分派没有这两个动作。 | 使用具备项目写权限的模型会话，检查工具发现，再对专用测试项目执行相应动作并核对结果。 [T3 handlers.ts:234](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/issues/handlers.ts#L234) · [Kar toolkit.rs:79](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/mcp/toolkit.rs#L79) · [Kar toolkit.rs:353](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/mcp/toolkit.rs#L353) |
| 作为聊天供应商选择 Muse、Kiro、Fx | T3 内置驱动表包含这三者；Kar 正常聊天入口的运行时目录缺少相应 ID。Kar 已有 Muse 用量读取和 Kiro 编辑器启动，两者均有各自用途。 | 从工作台或 CLI 选择这些运行时并尝试创建聊天，记录供应商进程是否已被实际调用。 [T3 builtInDrivers.ts:33](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/provider/builtInDrivers.ts#L33) · [T3 adapter.ts:1956](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-muse/src/server/adapter.ts#L1956) · [Kar runtime.rs:322](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/runtime.rs#L322) · [Kar runtime.rs:364](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/runtime.rs#L364) |
| 工作台语音转录 | T3 注册本地与云端语音转录处理器。Kar 的工作台服务路由没有对应的音频转录入口。语音未列入原八项场景。 | 对本地模型和已配置的云端模型各提交一段音频，检查文本和错误回执。 [T3 http.ts:118](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/speech/http.ts#L118) · [T3 http.ts:222](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/speech/http.ts#L222) · [Kar http.rs:76](https://github.com/kense-labs/Kar/blob/d02239d9234abb382d0a35c084d25dacc557738c/src/workbench/http.rs#L76) |

## 实现和实测证据

已有正常入口的关键路径包括双客户端断线续收、持久化队列和待确认状态、逐轮 Git 检查点，以及支持相应原生能力时的文件和模型上下文分叉。每条路径的具体供应商、输入形态和崩溃切点都影响结果。Kimi、Codex 在原生收件与 Kar 回执落盘之间的真实强杀，以及 Cybion 在远端运行中关闭，仍需分别取得实机证据。这些缺证项单独记录，不计入上面的 16 个功能差异。参见[候选交接说明](backend-workbench-final-acceptance.md)。

## 第二次按操作复核

本节以本地修复分支 `3f45a88` 为截点。前文描述 GitHub `main` 的 `d02239d`。两份对照源码分别固定于上游运行时 `2b15859` 和 T3 `2808e87`。本节记录源码可证的行为，统一实测由接手方执行。

| 额外核实的操作 | 上游用户可以做什么 | Kar 已有入口与剩余差异 | 验收操作 |
| --- | --- | --- | --- |
| 从第三方 ACP 注册表准备和卸载代理 | T3 可以搜索代理清单，按条目准备可执行文件，卸载由工作台管理的二进制。 | Kar 有固定的运行时目录及本机会话扫描、导入，也可配置自定义运行环境。尚未找到连接第三方目录并执行上述生命周期的工作台入口。已有清单编号 `registry-agents`，此次确认了其中三个操作的差异。 | 从干净环境搜索一个不在内置目录的条目，准备后开启会话，再卸载该受管安装。核对实际文件和调用入口。 |
| 从录制记录读取一条输入的完整投递历史 | 上游公开的会话投影按逻辑输入归集多次投递、撤回、逐次回执和运行时回显，支持从录制重新计算。 | Kar 提供按请求查回执、按输入读终态、回放原始记录。在本节对照截点尚缺整条投递历史的公共投影。后续提交 `7111a73` 已将其接入 `Session::conversation`、游标订阅及 `kar replay <记录.jsonl> --conversation`。 | 以同一输入 ID 完成排队、撤回、再次投递，从重放返回完整尝试顺序，区分控制接纳和原生回显，核对来源与会话归属。 |

ACP 条目对应 [T3 工作台处理函数](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/ws.ts#L2437)、[Kar 运行时目录](../src/runtime.rs) 和 [Kar 原生会话扫描与导入](../src/workbench/agent_sessions/mod.rs)。

输入历史对应 [上游运行时会话投影](https://github.com/botiverse/oar/blob/2b15859a13dcc50e8b66cbaee5a007832aa00e08/packages/oar/src/observe/conversation.ts#L239)、[Kar 请求回执](../src/stream.rs)、[Kar 输入终态](../src/observe/turn_end/input.rs) 和 [Kar 会话视图](../src/observe.rs) 及 [本地新增的输入历史投影](../src/observe/conversation/mod.rs)。

### 用模型会话实际可发现的工具复核

在本地集成提交 `cec3da6`，获授权的模型已经可以创建 Issue、发表评论。进一步核对 T3 的工具声明和处理函数，还能确认以下入口差异。对照范围是模型在当前工作台会话内可发现和调用的工具。

| 模型的操作 | T3 处理路径 | Kar 的当前表现与复验入口 |
| --- | --- | --- |
| 列出、读取、关闭或重新打开项目 Issue | `list_issues`、`read_issue`、`set_issue_state` | 普通托管服务覆盖 Issue 查询及更改。授权模型的工具列表中只新增了创建和评论。用同一模型租约查找上述三个操作，并检查写权限边界。 |
| 查看工作树、把聊天移交到另一个工作树 | 工作树工具分别调用引用列表和交接服务 | Kar 有工作树命令和工作台交接，当前模型工具注册及分派缺少对应操作。用两个独立工作树查看列表，从当前会话调用交接并核对聊天归属。 |
| 以工作台模型身份启动、恢复或取消持久工作流 | 工作流工具按操作分派到工作流服务 | Kar 有持久工作流服务，当前模型工具缺少这组调用。让已授权模型启动测试流程，读回 run ID，再分别验证恢复、取消和重连后的结果。 |

对应 [T3 Issue 工具](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/issues/handlers.ts#L163)、[T3 工作树工具](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/worktree/handlers.ts#L13)、[T3 工作流工具](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcp/toolkits/workflow/handlers.ts#L52) 及 [Kar 的模型工具注册和调用分派](../src/workbench/mcp/toolkit.rs)。这些是原清单 MCP 工具组内进一步核实的操作，与前文的 Issue 创建及评论共用同一功能组。

### 同一聊天内回到早先回合

T3 的回滚请求可以选择只回退对话。供应商支持时，服务端在当前工作台聊天上回退模型上下文，保留回滚审计记录。选择恢复文件时才调用检查点恢复。Kar 的工作台回滚请求接受回合、回合前后位置和预演选项，执行文件恢复。要按选定回合继续旧上下文，Kar 已有分叉路径，子聊天有单独的聊天 ID。此次核对没有找到保留原聊天 ID、只回退模型上下文的操作。[T3 回滚处理](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/CheckpointRollbackService.ts#L415) · [Kar 工作台回滚与分叉](../src/workbench/chat_controls.rs) · [Kar 工作区恢复](../src/chat/checkpoint/restore.rs)

复验时在三轮中分别写入唯一标记，记录聊天 ID、事件游标和文件内容。选择回到第二轮且保留文件，从同一聊天继续提问。检查模型输入是否排除第三轮，客户端是否仍使用原聊天 ID，文件是否逐字节保持，以及审计记录是否能说明回滚。独立子聊天测试仍按原分叉清单执行。
