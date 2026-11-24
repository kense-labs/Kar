# Kar

Kar 是用 Rust 编写的编程 agent 运行库和命令行。应用可以用同一套接口打开本地 agent 会话、发送任务、处理工具请求、读取输出，再用原生会话 ID 继续对话。库和 CLI 由 Cargo 构建（Rust 1.89 或更高版本），直接与供应商进程通信。各家 agent 的安装和账号由相应供应商管理。

## 开始使用

在项目目录执行：

```sh
cargo build --release --locked
./target/release/kar list
./target/release/kar open codex --cwd .
./target/release/kar run codex '读一下本项目，概括模块之间的关系' --cwd .
```

`open` 会做协议握手，输出会话 ID 和本次事件流 ID，然后关闭子进程。`run` 会实际发起模型请求。使用 `--resume <原生会话ID>` 可以继续已保存的会话。Pi 通常在第一条消息后才写入会话文件。

| 入口 | 用途 |
| --- | --- |
| `kar install claude --plan` | 查看供应商安装方式。省略 `--plan` 会运行安装器 |
| `kar account claude login` | 在终端使用供应商自己的登录流程 |
| `kar account codex login --json` | 逐行输出设备验证码、提示和最终登录结果，供当前登录界面消费 |
| `kar account codex logout --json` | 退出所选原生账号并核对结果 |
| `kar account codex status --json` | 查询结构化登录状态 |
| `kar account codex usage` | 在打开聊天前查询账号额度、重置时间和可用重置次数 |
| `kar account claude inspect-limit-resets` | 用户主动查看 Claude 提供的重置种类和剩余次数 |
| `kar credentials claude` | 列出凭据变量名、用途和存储位置 |
| `kar providers opencode menu` | 查看本机 OpenCode 实际提供的供应商和登录方式 |
| `kar providers pi list` | 仅读取 Pi 存储的供应商和凭据类型 |
| `kar providers qwen --cwd . menu` | 查看当前工作区的 Qwen 供应商配置 |
| `kar models codex` | 读取运行时实际报告的模型 |
| `kar usage codex` | 读取账号额度窗口，结果采用供应商格式 |
| `kar skills codex --cwd .` | 在打开聊天前查询工作区的技能、来源和作用范围 |
| `kar mcp-servers grok --cwd .` | 查看工作区的 MCP 服务 |
| `kar tools codex --cwd .` | 查看 MCP 服务提供的工具和输入参数 |
| `kar commands claude --cwd .` | 查看 Claude 在该目录下公布的斜杠命令 |
| `kar run claude '解释这个函数' --json --record run.jsonl` | 实时输出事件并创建新的 JSONL 记录 |
| `kar import upstream.jsonl run.jsonl` | 导入旧版 v3 会话记录，保留原始事件并写入新的 JSONL 文件 |
| `kar replay run.jsonl` | 验证事件顺序并重放记录。可传 `--stream <UUID> --after <序号>` |
| `kar mcp --runtimes codex,claude --max-sessions 4 --cwd .` | 向支持 MCP 的宿主提供一组可委派的 agent |

### 聊天前查询账号额度

Rust 宿主可调用 `Runtime::account_usage(&AccountUsageOptions::default())`。已支持 Codex、Claude、Factory、Grok、Kimi 和 Antigravity。返回的 `windows` 区分账号、单个模型和共享限额的模型组，`usedRatio` 的范围是 0 到 1。展示整个账号是否受限时使用 `rateLimited`。例如某个模型已经耗尽，账号仍可使用其他模型。CLI 示例中的超时按毫秒计，包含安装探测和原生查询。

```sh
kar account claude usage --timeout-ms 20000
kar account codex usage --count-only
kar account factory usage
kar account kimi usage
```

Codex 默认返回原生服务提供的重置详情，`--count-only` 请求仅取次数。用户主动兑换时，可调用 `Runtime::redeem_limit_reset` 或 `kar account codex redeem-limit-reset --idempotency-key "$ATTEMPT_ID" --reset-id "$CREDIT_ID"`。宿主为每次用户操作生成并保存唯一的 `ATTEMPT_ID`。连接中断、结果未知时，用同一个 ID 重试，Codex 可识别已经兑换的请求。读取额度不会兑换次数。

Factory 分别报告账号额度和 Droid Core 模型组额度。Kimi 在启用原生凭据锁的平台通过本地 Kimi Web 读取用量，由供应商进程刷新过期凭据。在 Windows 或显式设置 `KIMI_DISABLE_OAUTH_LOCK=1` 时，只读取仍有效的文件凭据。已在隔离账号和本地计费服务上验证 Factory 与 Kimi，两个并发 Kimi 查询共用了一次原生凭据刷新。

### Claude 额度重置

用户在界面上主动查看可用重置时，宿主可调用 `Runtime::inspect_limit_resets`。它在新建的空目录中打开 Claude Code 的交互终端，读取原生确认框里的种类、次数和到期说明，再取消。这个入口会启动 Claude，启动过程中可能产生模型请求并开启五小时用量窗口。日常刷新额度使用 `account_usage`。

```sh
kar account claude inspect-limit-resets --include-five-hour
kar account claude redeem-limit-reset --reset-id weekly --idempotency-key "$ATTEMPT_ID"
```

支持的种类为 `five_hour`、`weekly`、`fable`、`full`。旧版 Claude 可能在提交命令时就消耗五小时重置。默认会在需要时重新查询五小时额度，选择其他种类兑换时还会检查原生配置中的功能状态。若返回 `consent_required`，宿主应告知用户可能消耗五小时重置次数。用户接受后，才为该次操作设置 `five_hour_limit_consent` 或 CLI 的 `--five-hour-limit-consent`。指定种类、作用范围或选项文字发生变化时，Rust 控制器不会确认兑换。

Claude 的兑换回执保存在对应账号的原生配置目录下。宿主为每次用户操作保存唯一的 `ATTEMPT_ID`。重试时传入相同的 ID 和种类，进程重启后也会读取原回执。若兑换命令已经提交而原生结果丢失，返回 `unknown`。普通查询不写兑换回执。Rust 的 `redeem_limit_reset_report` 和 CLI 结果还会报告此次是否完成了原生首次启动引导。交互驱动适用于 macOS 和 Linux，在 Windows 返回 `unsupported_platform`。

在 macOS 上，Claude Code 2.1.322 和 2.1.330 都已通过隔离账号和本地模拟额度服务验证。两版的查询均可关闭原生确认窗，兑换周额度后以同一 ID 重试只产生一次兑换请求。

### 聊天前查询工作区目录

这四个查询均可通过 `--cwd <工作区>` 指定目录，无需发送聊天消息。查询成功时返回工作区、观测时间、结果范围 `view` 和目录是否不完整的 `partial`。

| CLI | Rust SDK | 支持的运行时与返回内容 |
| --- | --- | --- |
| `kar skills` | `Runtime::skills` | Antigravity、Claude、Codex、Cursor、Devin、DSH、Factory、Grok、Kimi、OpenCode、Pi RPC、Qwen。列出技能名称以及供应商提供的来源、作用范围和启用状态 |
| `kar mcp-servers` | `Runtime::mcp_servers` | Claude、Codex、Grok。列出 MCP 服务以及供应商提供的状态或来源 |
| `kar tools` | `Runtime::tools` | Claude、Codex 返回 `mcp-only`，列出 MCP 工具及所属服务。Pi 的 npm 版和配有同版本官方 SDK 的独立版返回 `registered`，列出内置和扩展工具、描述及启用状态。Codex 和 Pi 可提供输入 schema |
| `kar commands` | `Runtime::commands` | Claude 和 Pi RPC。列出命令名称、来源及供应商提供的描述 |

例如 `kar tools codex --cwd .` 会读取所有分页。Claude 的技能目录将原生命令和上下文中列出的技能对应起来，无法可靠区分同名技能时省略有歧义的元数据。Grok 遵循所选 CLI 的工作区信任设置。这些目录均已通过隔离工作区验证。Cursor 使用原生 SDK 对照，其余运行时使用真实 CLI。

OpenCode 1.x 用原生调试命令读取技能，较大的目录通过临时文件完整接收。OpenCode 2.x 从 2.2.7 起支持这项查询，先等待当前工作区的插件激活，再读取技能。有插件激活失败时返回已有技能并标记 `partial`。已用 OpenCode 1.19.6、2.2.7 和 2.3.2 验证。

Pi RPC 从原生 `get_commands` 读取已加载的技能、提示模板和扩展命令。查询沿用所选账号目录和 Pi 保存的工作区信任设置。已用 Pi 1.6.0 验证同一个项目在已信任和未信任时的目录差异，并核对查询前后的信任文件及会话记录。

`kar tools pi --cwd .` 会等工作区扩展完成启动，再从 Pi 注册表读取工具。已停用的工具带 `active: false`，受信任项目在启动时动态注册的工具也会出现。已在 macOS 用 Pi 的 npm 版 1.4.0、1.6.0 和 Node 24.15.0 核对参数、启用状态、工作区信任与会话目录。使用独立版时，按 [Pi SDK 路径配置](src/pi_sdk.rs) 指定官方 npm 包 `@earendil-works/pi-coding-agent` 的安装根目录，并在 PATH 中提供支持原生模块的 Node。Rust 会核对 CLI 和 SDK 版本，通过 SDK 自带的 CLI 入口读取同一工作区和账号目录的注册表。已在 macOS 用独立版 1.6.0 和同版本 SDK 验证。仅安装独立程序时返回 `native_query_unavailable`，指定的 SDK 版本不匹配时返回 `version_unsupported`。

在 macOS 和 Linux 上，Antigravity 1.5.10 与 1.5.11 可在未登录时直接读取工作区和用户技能文件。结果始终带 `partial: true`，因为原生运行时还会按条件加载内置技能。其他受支持的版本和平台会先核对登录状态，再查询原生 `/skills`。已在 macOS 上用 1.5.10、1.5.11 和 1.5.15 逐项对照原生输出，包括插件技能和调用权限。

`kar skills dsh --cwd <工作区>` 会在打开聊天前读取 ACP 配置的全局技能。Rust 插件等 DSH 加载完配置后调用 `skills.snapshot({cwd})`，结果包含项目、用户、内置及插件来源，并保留用户和模型的调用权限。`partial` 来自运行时的完整性标记。DSH 的 Node 宿主需要能够加载原生插件。已在 macOS 上用 DSH 0.2.1-rc.5 和 0.2.2-rc.5 验证五种技能来源和禁止用户调用的条目，查询期间没有创建聊天记录。

Cursor 按 SDK 1.4.0 的规则读取工作区和用户技能文件。Git 项目中的嵌套目录使用 Cursor 自带的 ripgrep，可用 `CURSOR_RIPGREP_PATH` 指定。扫描不完整或遇到格式损坏的技能时，返回已读出的条目并标记 `partial`。已分别在普通目录与 Git 工作区对照 SDK 的原生目录。

### 识别本机安装

`kar list` 的 `installation.command` 给出选中的可执行文件。通过 PATH 找到多个安装时，`shadowed` 按搜索顺序列出其余路径。同一目录、重复 PATH 项和指向同一文件的别名只计算一次。显式指定可执行文件时，只检查指定的路径。GUI 宿主的 PATH 找不到 CLI 时，还会检查相应供应商的安装目录，包括 macOS 的 ChatGPT/Codex 应用包。创建持久化聊天时会保存实际选中的启动路径。

在打开聊天之前，`kar list codex --cwd .` 可查看指定安装的 `session_capabilities`，也可以用 `--bin` 选择可执行文件。Rust 宿主先调用 `probe_installation(&options.installation_options())`，再把返回值传给 `runtime.session_capabilities(&installation)`。后一步只读取已有的探测结果。报告包含当前安装已知的图片输入、回合内追加、工具传输、子任务、压缩、上下文和设置限制。缺少字段表示尚未确认，显式 `false` 或空列表表示对应能力不可用。Kimi 在同一版本仍可能选择 ACP 或 KAP，打开前只展示两条路径共有的能力，连接后再由原生会话报告。缺失或未通过就绪检查的安装只返回安装状态。

Rust SDK 可用 `Runtime::probe_installation(&InstallationOptions)` 指定工作目录和子进程环境。环境变量传 `None` 时会从探测进程移除。结果区分程序缺失、就绪检查失败、程序身份不符和不受支持的版本。已验证的本机二进制可复用版本探测结果；更换文件、环境或工作目录会重新探测，脚本及版本管理器入口每次检查。安装和更新操作会清理该缓存。

### 模型设置表单

`kar config-form` 列出全部已发布的表单，`kar config-form grok` 输出 Grok 的表单。读取表单无需安装 Grok。模型和推理强度可以用 `kar models grok --cwd <工作区>` 查询。Rust 通过 Grok 的 ACP 目录接口读取可选模型、默认模型以及每个模型支持的推理强度，查询期间无需创建聊天。已用 Grok 1.0.92 和隔离的本机配置验证。

Rust 宿主通过 `kar::forms::config_form` 发现表单，调用 `Runtime::form_model_listing` 获取实时目录或使用宿主随附的备用目录。返回值会标明未登录、超时、空目录或其他失败原因。调用 `form.validate` 时传入创建或编辑上下文，编辑已有配置时可保留当前目录无法列出的原值。启动时用 `form.map_launch` 解析模型、推理强度及已由宿主取出的凭据，Grok 的结果可直接 `apply_to(&mut options)`。当前提供 Grok 的设置表单。

### 账号登录、退出与凭据声明

Codex、Factory、Kimi、Qoder、Grok、CodeBuddy 和 Cursor CLI 提供 `Runtime::login`。宿主通过 `LoginInteraction` 接收配对链接或设备码，用 `LoginCancellation` 取消正在进行的登录。`LoginOptions` 支持指定安装环境、工作目录和总超时，Kimi 还可以选择大陆或全球区域。`kar account <runtime> login --json` 输出同样的事件，最后一行给出结果。配对信息只交给发起登录的客户端，不进入聊天历史或 SSE。Cursor CLI 支持把登录链接交给宿主打开，原生流程提供匹配码时也会提示用户核对。成功后使用同一环境重新查询账号。已在临时凭据目录中用官方 CLI 和本机登录服务验证配对链接、PKCE 交换、状态查询和取消。Windows 上 Grok 的原生命令无法禁用浏览器，此时无浏览器登录会返回 `browser_suppression_unavailable`。

Codex、Claude、Cursor CLI、Devin、Grok、Kimi、Antigravity、CodeBuddy 和 Factory 提供 `Runtime::logout`，CLI 入口为 `kar account <runtime> logout --json`。退出后会在同一账号环境核对状态。供应商仍报告已登录时，退出成功的回执会转为 `still_logged_in`。Factory 仍采用环境密钥时也会给出这一结果。状态查询无法确认时保留原生操作的结果。宿主可用 `supports_login()` 和 `supports_logout()` 判断适配器是否提供对应操作。

真实 Codex 和 Kimi CLI 的设备登录测试使用临时账号目录和本地 OAuth 服务，覆盖取消、凭据落盘和重新查询。Kimi 还验证了原生退出后清空临时凭据的过程。Factory、Qoder、Grok 和 CodeBuddy 的宿主登录已覆盖协议或命令行交互测试。

`Runtime::auth_status` 和 `kar account <runtime> status --json` 返回 `logged_in`、`logged_out` 或 `unknown`。Codex、Claude、Qoder、Grok、Devin、Cursor 和 Antigravity 使用各自的命令。Factory、Kimi 和 CodeBuddy 在独立的协议进程里查询，查询期间不会创建聊天。Qwen 在本地检查用户和系统配置，有凭据来源时返回 `unknown` 并列出来源名称。运行时无法验证的环境密钥也会保留为 `unknown`。SDK 可用 `AuthStatusOptions` 设置环境、工作目录和总超时。原来的终端命令 `kar account <runtime> status` 继续提供供应商的原始输出。

`kar credentials <runtime>` 和 `kar::credentials::for_runtime` 提供 15 个运行时的静态配置声明，包含密钥变量名、配置优先级和供应商支持的插值方式。宿主查询这些声明时无需安装运行时，也无需读取凭据。浏览器里的 `PiDurable.credentials()` 返回宿主管理凭据的声明，`PiDurable.sessionCapabilities()` 可在创建会话前查询输入、工具和设置限制。Cursor 使用 CLI ACP，凭据声明包含 CLI 的平台存储位置。传入环境密钥时，查询只报告已配置的变量名。[Cursor CLI 账号文档](https://prod.cursor.com/docs/cli/reference/authentication)

### 管理模型供应商凭据

OpenCode 2 的 Rust 入口是 `Runtime::opencode_provider_auth`。在 CLI 中，`kar providers opencode menu` 返回本机注册表，`list` 只列已保存的凭据，`status <供应商>` 也能识别环境变量。用 `set-key <供应商>` 从标准输入保存密钥，用 `logout <供应商>` 删除已保存的凭据。

`kar providers opencode login <供应商> oauth` 支持设备码、浏览器回调和手动授权码，`api-key` 支持需要填写账号 ID 等附加信息的供应商。标准输出逐行给出 JSON 事件、提示和结果，宿主用标准输入逐行回答 JSON 字符串。远程客户端可在 `login` 子命令前传 `--refuse-loopback`，在展示授权链接前拒绝只能由服务端本机浏览器完成的方式。取消、过期或超时会清理已取得 ID 的授权尝试。Rust 服务绑定本机回环地址，空闲时关闭，宿主退出也会关闭其标准输入。OpenCode 2.3.2 的密钥替换、附加表单和登出已通过真实 CLI 验证。DigitalOcean 的原生 OAuth 也验证了展示链接后取消，以及远程模式在展示前拒绝本机回调。OAuth 的设备码、手动输入与浏览器竞速、取消和超时经过本地协议测试。

Pi 1.6 的 `kar providers pi menu` 列出 43 个内置供应商，并合并当前 `models.json` 的自定义供应商和名称覆盖。每个条目包含 Pi 声明的登录方式、订阅标记和登录按钮文字。`list` 查询已保存的凭据类型，`list --resolve` 与 `status` 让 Pi 解析凭据。供应商操作与聊天默认使用同一配置目录。可以用 `PI_CODING_AGENT_DIR` 指定 Pi 的配置目录。

使用 Pi 的 npm 版或为独立版指定同版本的官方 SDK 后，Rust 原生模块从所选 SDK 读取登录菜单，调用 Pi 的供应商登录、凭据状态和退出接口。菜单读取不会创建凭据文件。`set-key <供应商>` 从管道读取字面密钥，保留 `$` 和开头的 `!`。`login <供应商> api-key` 支持 Cloudflare、Bedrock、Vertex 的多步表单。`login <供应商> oauth` 将 Pi 发出的授权链接、设备码、轮询间隔、有效期和手动输入提示传给宿主。原生回调完成时，Rust 会撤销仍在等待的手动输入。

`kar providers pi login openai oauth --agent-name <名称> --device-id <UUID>` 可指定 ChatGPT 授权页收到的宿主名称和设备身份。Rust 接口 `PiProviders::login_with_options` 支持按需获取设备 UUID。适配代码使用 Rust，SDK 需要能加载原生模块的 Node。Pi 1.6 独立版可配合同版本 SDK 获取完整设备码元数据并传入宿主身份。仅安装独立程序时使用 Pi 的无浏览器终端流程。已用 Pi 1.6.0、隔离的配置目录和本机 Radius 服务验证凭据存取和设备码授权，并核对 ChatGPT 授权 URL 中的身份字段和取消后回调端口释放。

启用 Pi SDK 时，`kar providers pi --auth-path accounts/team.json --models-path configs/team.json menu` 可为这次操作选定凭据文件和模型配置文件。相对路径按 `--cwd` 解析。`--no-models` 对应 Pi 的 `modelsPath: null`。Rust 的 `Runtime::pi_provider_auth_with_paths` 接受同样的选项。代理设置和默认设备身份继续取自 agent 目录。只安装独立程序时，终端接口使用默认文件。显式指定的 SDK 版本不匹配或无法加载时，账号操作会返回错误。

`kar providers pi catalog --provider anthropic` 读取模型目录、供应商配置状态和默认模型。每个模型包含协议、服务地址、上下文窗口、最大输出、推理能力和输入类型。读取快照不请求远程目录。`--refresh` 让 Pi 刷新所选供应商，`--refresh --offline` 重读配置和缓存。`Runtime::pi_model_catalog` 返回可重复刷新的目录。`CatalogCancellation` 可取消正在等待的请求。真实 Pi 1.6.0 测试用本机 HTTPS 服务验证了远程模型、按供应商返回错误、取消后再次刷新，以及重新打开后使用 ETag 验证缓存。在同一份隔离配置中，测试逐项核对了刷新前后的 1,699 个模型、43 个供应商和每个供应商的默认模型。

Qwen 的 `Runtime::qwen_provider_auth` 通过私有 ACP 进程查询或写入配置。例如 `kar providers qwen --cwd . connect custom-openai-compatible --protocol openai --base-url http://127.0.0.1:8080/v1 --model local-model --scope workspace --key-stdin` 从管道读取密钥，省略 `--key-stdin` 时由 Qwen 复用已有配置。真实 CLI 测试验证了 `user` 和 `workspace` 两种写入位置，配置期间模型服务收到零个请求。成功回执表示 Qwen 已写入配置。读取凭据、写入配置和账号登录均独立于聊天事件流。

## 跨重启聊天

`kar chat` 会保存聊天 ID、原始工作区的绝对路径、运行时、原生会话 ID 和回合时间线。第一次在项目目录创建聊天，之后可以从任意目录继续。以下示例假设 `kar` 已加入 PATH。

```sh
kar chat new codex --cwd . '帮我梳理这个项目的入口'
kar chat resume '沿着刚才的入口继续看'
kar chat list
kar chat history --json
kar chat resume --chat <聊天ID>
```

`resume` 默认选择最近使用的聊天。省略提示词进入交互模式，每行提交一回合，Ctrl-D 退出。`list` 和 `history` 可以在 agent 离线时使用。Rust 后端可以通过 `kar::chat::ChatStore` 的 `create`、`resume`、`history` 接入，调用 `ChatSession::send(request_id, text)` 发送消息。客户端应在首次发送前生成 UUID 并保留到请求完成。在同一聊天中重试时传回同一个 UUID（CLI 使用 `--request-id`）会取回原回合，同一 UUID 携带不同内容会报错。使用新 UUID 可以有意再次发送相同文字。

时间线保留回合中的用户输入、助手回复、推理、工具活动和供应商提供的补丁。流式片段与最终消息合并。宿主在回复中途退出时，可以查看已经写入的部分，未确认结束的回合会显示为 `interrupted`。恢复 Codex 时，如果原生会话已有该回合的终态，会按原生条目 ID 补全回复、推理和工具结果。恢复聊天不会自动重发中断的输入。

聊天日志默认保存在用户状态目录下。需要固定存储位置时，可用 `kar chat --store <目录>`。一个聊天同一时间由一个进程续写，其他进程可以读取历史。若有以前通过 `kar run --record` 保存的单事件流 Rust JSONL，可用下面的命令导入，并通过输出的聊天 ID 继续原生会话。续聊需要原工作区和供应商保存的会话仍然可用。

```sh
kar chat import codex old.jsonl --cwd /path/to/original/workspace
```

创建聊天时可使用 `--effort` 选择原生推理强度，`--service-tier` 选择服务等级，`--name` 设置供应商会话名称，重复 `--add-dir` 可添加工作目录。Rust SDK 通过 `SessionOptions.settings` 接入。Codex 和 Claude 支持这四项设置，Factory 支持并核实推理强度，Pi 支持强度和命名，Qwen 支持强度和额外目录，Kimi KAP 支持强度及创建时添加额外目录，OpenCode 在握手声明支持时可添加额外工作目录，ACP 通过握手中的 `thought_level` 设置强度。额外目录相对于原聊天工作区解析；Git 检查点覆盖主工作区。打开会话时会校验供应商报告的强度和服务等级，持久化聊天在恢复时重新应用所保存的设置。Codex、Pi、Factory Droid 和 DeepSeek Harness 可通过 `Session::native_session_file` 查询供应商返回的会话文件路径。

用 `--system-prompt` 替换系统指令，用 `--append-system-prompt` 追加宿主指令。Codex、Claude、Pi、Qwen、OpenCode、Factory 和 Grok 接入了这两项。Factory 创建时写入原生指令，持久化聊天恢复后沿用已有指令。Kimi KAP 支持追加，CodeBuddy 支持替换，Antigravity 在握手声明支持时可追加。Codex 追加时保留工作区已有的开发者指令。OpenCode 2.x 从 2.0.30 起支持追加，从 2.2.0 起支持替换；适配器只向可选的主 agent 应用替换。Pi 工作区含有 `.pi/APPEND_SYSTEM.md` 时，若还需追加非空宿主指令，传入 `--pi-project-trusted true` 或 `false` 选择项目或用户级指令文件。这一选择只用于该会话。

Qwen Code 0.26.0 起可用 `--native-memory-default off` 关闭自动记忆、后台整理和技能提取的系统默认值。Rust 会为当前进程生成一份私有默认配置，用户、工作区或系统设置仍可覆盖它。用 `session.capabilities().native_memory` 或 `chat.capabilities().native_memory` 读取当前生效状态。遇到工作区信任、沙箱等无法判断的条件时，该字段为空。持久化聊天重开时会重新读取配置。已用 Qwen Code 0.28.4 和本地模型服务验证，原用户设置在两次打开之间启用自动记忆后，同一聊天的下一次模型请求包含了记忆目录。

DSH 0.2.1-rc.5 起支持 `--session-log-upload off`。Rust 在创建、恢复或分叉原生会话前，读取本次 DSH 进程实际加载的日志插件配置。确认上传已关闭后才打开聊天。该选项优先于用户配置中的上传许可，持久化聊天恢复时会重新应用。用 0.2.1-rc.5 的 ACP 和 0.2.2-rc.5 的 Web 协议实测，开启时模型请求会带上 `dsh_session_log`，关闭后父会话、子会话和恢复的会话均省略该字段。Web 协议下按回合分叉的会话也通过了同样的验证。对话仍会通过正常的模型请求发送。

重复传入 `--disallow-tool` 可以禁用多个工具。Claude、Qwen、Pi 和 CodeBuddy 按原生名称或规则处理，Codex 接受已配置服务器的完整 MCP 工具名，例如 `mcp__review__publish`。Claude 还支持 `--exclude-user-config` 排除用户级设置、MCP 和指令，或用 `--exclude-user-config=settings,instructions` 选择类别。聊天会保存这些选项，恢复后沿用；Claude 的临时分叉提问也沿用原聊天的系统指令和配置来源。

DSH 的 Web 和 ACP 接口都支持 `--name`。新建聊天时会保存原生标题。重开聊天时，已有的手动或自动标题会保留，原来没有标题的会话才使用这次指定的名称。ACP 会等 DSH 完成持久化，创建后立即关闭也能保留标题。

DSH 支持 `--system-prompt` 替换根聊天的系统提示词，`--append-system-prompt` 在保留原生提示词的基础上追加文本。两项同时指定时，以两个换行连接，`{{变量}}` 等文本按字面传给模型。持久化聊天重开时沿用保存的设置，分叉时可单独设置子聊天的提示词，原生子代理使用自己的提示词。供应商插件已注册互斥的完整提示词时，打开聊天会返回对应的设置错误。已在真实 DSH ACP 和 Web 进程中核对模型请求，包括插件在创建会话时就排入的第一条输入。支持历史内系统消息更新的 DeepSeek 模型会在改动生效的回合发送新的 system 消息。

DSH 的 `--disallow-tool bash` 会从指定原生会话的工具列表中移除 `bash`，并阻止执行。宿主在第一条输入前确认限制已生效，重启后按原会话 ID 再次绑定。按回合分叉时可以给子会话指定自己的限制。已在 DSH 0.2.1-rc.5 的 ACP 和 0.2.2-rc.5 的 Web 接口上实测。

DSH 0.2.2-rc.3 起在 macOS 和 Linux 上支持 `--add-dir`。目录按会话加入原生沙箱的可写范围，沿用部署的沙箱模式。同一工作区的其他聊天保留各自的权限。macOS 实测覆盖 0.2.2-rc.3 的 ACP 和 0.2.2-rc.5 的 Web 接口，分别检查了文件工具和 shell 写入、相邻聊天的拒绝，以及重启后原会话的再次写入。

同一工作区可以为不同聊天设置独立的子进程环境。例如 `kar chat new codex --unset-env HTTP_PROXY --unset-env HTTPS_PROXY` 会移除这个聊天继承的两个代理变量。Rust 调用 `options.remove_env("HTTP_PROXY")?`。随后写入 `options.env` 的显式值优先，空字符串也是一个值。聊天只保存要移除的变量名，重启和分叉都会沿用。临时的环境值由宿主在 `resume_with` 时重新提供。托管在共享远端进程中的 Morph 和 Cybion 会拒绝进程环境设置。

需要结构化输出时，在 `kar run` 或 `kar chat new` 传入 `--output-schema '<JSON 对象>'`，也可以设置 `SessionOptions.settings.output_schema`。Codex 把 schema 传给每次由宿主发起的回合，结果从助手文本读取。Codex 自主目标回合遵循供应商自己的设置。Claude 使用原生 `--json-schema`；Grok ACP 使用原生提示元数据。供应商返回的结构化结果可从 `TurnReport.structured_output`、持久化的 `ChatTurn.structured_output` 和实时事件读取。未返回值和显式 JSON `null` 可以区分。Grok 返回的 schema 校验失败记为 `output_schema`。聊天恢复和队列投递会沿用已保存的 schema。Codex、Claude 已用真实程序验证跨进程恢复，Grok 已在 ACP 协议测试中验证。

### 恢复会话时的提示词

调用 `Runtime::resumed_session_options()` 可以在启动供应商前查询这项行为。已安装的运行时也会在 `kar list <runtime>` 的 `session_capabilities.resumed_session_options` 中返回相同声明。每项指令分别说明直接调用 `Runtime::open` 恢复原生会话时，传入新值（`provided`）和省略该值（`omitted`）会发生什么。

| 运行时 | 指令 | 恢复时传入新值 | 恢复时省略 |
| --- | --- | --- | --- |
| Codex | 系统提示词 | 下一回合使用新值 | 沿用已保存值 |
| Codex | 追加指令 | 下次上下文压缩后生效 | 压缩前沿用已保存值，压缩后使用默认值 |
| Claude Code | 系统提示词、追加指令 | 下次上下文压缩后生效 | 压缩前沿用已保存值，压缩后使用默认值 |
| Pi RPC、Qwen Code、DSH | 系统提示词、追加指令 | 下一回合使用新值 | 使用该运行时新建会话时的默认值 |
| Pi Durable | 追加指令 | 下一回合使用新值 | 沿用已保存值 |

其余适配器目前没有这项时机声明。`ChatStore::resume` 会重新传入聊天保存的配置，因此表中的“省略”适用于直接打开原生会话的调用者。Pi Durable 需要等正在执行的回合结束后才能修改指令，宿主通过 `PiDurable.sessionCapabilities()` 查询。

### 恢复被拒绝时

新建、恢复或分叉时，如果运行时报告的模型与请求不一致，宿主可以读取 `OpenError::ModelConfirmation` 的 `requested` 和 `reported`。Codex、Pi RPC、Factory、Kimi KAP、DSH、Qoder 和 Antigravity 按各自的协议核对。Pi Durable 的浏览器接口通过 `ModelConfirmationError` 返回这两个字段。Qoder 会保留最后一次成功读取的目录选项 ID，目录查询失败时还附带查询阶段和错误。模型字段或目录没有原生读数时，调用方可区分缺失的读数与空目录。

Antigravity 的 ACP 配置需要同时收到命令成功回执和本会话的配置通知。兼容旧的组合选择器时，先核对原生模型，再从该模型的菜单中选择推理强度。命令被拒绝、等待通知超时，以及截止时仍报告其他模型，分别保留各自的错误。设置 `GEMINI_HOME` 时，Rust 通过 `--gemini-dir` 指定 ACP 使用的配置目录。

OpenCode、Kimi ACP、Antigravity 和 Devin 恢复或分叉前会查询原生会话列表，逐页确认目标会话的工作目录。目录不符、未列出目标或无法核实目录时，宿主可读取 `OpenError::ResumeCwd` 的具体原因。其他适配器按各自原生协议识别会话丢失、归档、被占用、凭据失效或模型不可用。

Rust 调用方可匹配 `Error::Open`，CLI 的 `chat --json` 与 `run --json` 会将结构化错误写入标准错误并以失败状态退出。原生会话打开失败时，队列保留在原聊天。Codex 和 Claude 的跨进程测试覆盖了恢复失败时游标和队列保持原值，以及恢复原存储后继续投递。

### 多客户端实时订阅

一个控制器负责与原生会话通信，其他进程可以随时订阅同一聊天。服务端通过 SSE 推送用户输入、增量回复、工具事件、回合终态和检查点。发送进程异常退出时，订阅者可看到持久记录的 `interrupted`。从命令行观察或启动仅服务所选聊天的 HTTP 端点：

```sh
kar chat watch --chat "$CHAT_ID"
kar chat serve --chat "$CHAT_ID" --listen 127.0.0.1:8090
```

客户端先取 `GET /snapshot`，将完整历史和返回的 `cursor` 一起保存，再打开 `GET /events?after=<cursor>`。两次请求之间发生的更新也会重放。每个 SSE 事件的 `id` 对应 JSON 中的 `cursor`，而 `previous` 是应用当前更新前应持有的位置。重新连接时，标准 `Last-Event-ID` 请求头优先于 URL 中的起始位置。每个客户端在成功应用更新后保存自己的游标；需要跨客户端进程崩溃恢复时，应把视图修改和游标存入同一事务。省略游标可从聊天创建时开始重放。

对于 Kimi KAP、Codex 和 Factory，一条工具调用可能先出现在实时流里，又出现在控制器重启后读取的原生历史里。恢复时按原生调用 ID 更新同一张工具卡片，补全参数和结果，并依照供应商的记录恢复它在回合里的顺序。旧聊天日志在读取时也会应用这条规则。

Rust 客户端可以用 `ChatStore::feed_snapshot` 和 `follow` 从共享存储订阅，或用 `ChatReplica::from_snapshot` 和 `apply` 消费 HTTP 事件。相同事件重复交给投影会被忽略，出现游标缺口会返回错误。宿主服务可将 `kar::chat::event_router` 挂在已有的鉴权路由下。CLI 默认监听本机回环地址。

DSH 原生历史可以补全控制器退出时只保存了一部分的工具调用。恢复时会把参数和结果合并到对应的工具卡片；同一回合再次使用相同调用 ID 的工具按执行顺序分别保留。

通过 HTTP 订阅时，Rust 客户端可直接使用 `HttpChatFeed`。连接中断、服务重启或最后一帧只收到一半时，`recv` 会用传入的已保存游标自动重连。若应用事件后本地事务未提交，再次传入旧游标即可重收该条事件。需要鉴权时，可用 `HttpChatFeed::with_client` 传入配置了认证信息的 HTTP 客户端。

`kar models cursor` 可在创建聊天前读取 Cursor 的基础模型 ID、显示名称、可选推理强度和默认值。新聊天可传 `--model <模型ID> --effort <强度>`。当模型同时提供思考开关和强度菜单时，Rust 选择强度菜单，并在打开聊天前核对 Cursor 返回的模型和参数。只传旧变体 ID 时沿用原生变体选择器。已用真实 Cursor CLI `2026.11.22-e9042b2` 验证两种选择方式及无效强度的拒绝。本机 AgentService 在三个回合都收到了基础模型 ID 和所选强度。

Cursor CLI 通过 ACP 的 `subagents` 能力上报子会话。任务列表保存父会话、子会话及发起调用的工具 ID，子任务的回复和工具结果归入发起它的回合。恢复时，Cursor 会为历史工具调用重新编号，持久化聊天沿用首次记录的归属。状态 `disconnected` 表示 Cursor 已停止观察该子任务，其执行结果未知。已用真实 Cursor CLI `2026.11.22-e9042b2` 验证能力协商，用协议测试和两个真实 HTTP 客户端验证嵌套子任务的实时输出、跨回合归属及断线续收。

Cursor 的重启续聊测试使用官方 CLI、本机 AgentService 和 CLI 自己的会话数据库。第一轮结束后关闭控制器，再排入两条消息。新进程按原顺序投递，重复启动后仍只有三次模型请求。服务端逐轮核对原生会话 ID、历史检查点和可从原生数据库读回的前一回合内容。

Cursor CLI `2026.11.22-e9042b2` 只执行 `/copy-request-id` 就退出时，可能还没创建原生会话数据库。再打开时，Rust 会识别 Cursor 针对该 ID 返回的 `SessionNotFound`，保留本地时间线、工作区绑定和待投递输入。已用真实 CLI 和回环元数据服务验证，连续两次尝试恢复缺失会话后，队列仍在原聊天里。该版本还会把部分模型连接错误作为助手文字发出，随后返回 `end_turn`。这类回合在时间线中保留错误原文，原生终态仍为已结束。

OpenCode 2 的控制器重启时，会用已保存的根助手消息、推理消息或工具调用 ID，在原会话中定位中断的回合。Rust 核对原生用户输入和结束标记，再从原生档案和 ACP 的历史回放补齐回复、工具结果及回合终态。两个 HTTP 客户端都从各自的游标接收同一条 `reconciled` 更新，其中的 `tool_snapshot` 按来源和调用 ID 更新已有工具卡片。恢复得到的结束记录可以确认成功、失败或取消。未在当时保存的 Git 结束检查点会继续显示为缺失。如果本地只记下已发送、没有唯一的原生 ID，或档案缺少结束标记、工具结果等核对依据，该回合保留为中断。后续排队消息仍在原聊天继续投递。已用官方 CLI 和本机模拟模型服务验证工具调用、带图追加输入、认证失败和双客户端续收。另在 macOS 上暂停 Rust 控制器，待 OpenCode 写入结束标记后强制终止控制器，恢复时原输入与离线排队输入各执行了一次。

客户端在分叉过程中断开时，后台仍会写完分叉记录。控制器的 `close().await` 会等这些写入结束并释放聊天和工作区锁，随后可以立即恢复原会话。读取历史的客户端也可以在这段时间继续订阅。Pi 的中断回合恢复会按原生调用 ID 补齐工具结果，已经显示过的工具卡片保留在原来的位置。

已用两个独立 Chromium 页面订阅同一 HTTP 聊天。工具执行期间让其中一页断网并重启服务，恢复后两页收到的 28 条事件逐条一致，各自的游标连续且无重复。测试使用原生 `EventSource`、真实 Rust 服务和本机协议测试程序。

```rust,no_run
# async fn subscribe(chat_url: &str) -> kar::Result<()> {
let mut feed = kar::chat::HttpChatFeed::new(chat_url)?;
let mut view = kar::chat::ChatReplica::from_snapshot(feed.snapshot().await?);
loop {
    let event = feed.recv(view.cursor()).await?;
    view.apply(event)?;
}
# }
```

Codex、Claude、Pi、Qoder、Kimi KAP、DSH、Factory 和 OpenCode ACP 可报告上下文压缩的开始或终态。聊天会把收到的边界写入当前回合，根会话的最新边界也可通过 `ChatReplica::last_compaction()` 和历史快照读取。记录包含事件游标，以及供应商给出的压缩 ID、触发方式或失败原因。

### 手动压缩

在空闲聊天上调用 `ChatSession::start_compaction()`，或者在原生会话上调用 `Session::start_compaction()`。它返回本次请求的句柄，`wait(Some(timeout)).await` 可等到供应商确认完成、取消、拒绝或失败。只停止等待时会返回 `unconfirmed`，同一原生操作仍可继续观察。关闭控制器后，也可对已保存的聊天运行以下命令。

```sh
kar chat compact --chat "$CHAT_ID" --json
```

Codex 会核对根线程、压缩条目和回合终态。Claude 先确认内置命令，再关联命令 UUID、手动压缩边界与最终结果。原生 Pi 按手动开始和结束事件确认，OpenCode 2.x 同时核对压缩消息 ID 和提示请求的回复，Grok 使用专用 RPC 的回执。Codex、Claude、Pi 和 OpenCode 已通过真实 CLI 和本地模型服务验证，Grok 已通过 ACP 协议测试。Qoder 按命令 UUID 关联开始和结束事件，只有收到对应操作的手动压缩边界才会报告成功，已通过 worker 和 daemon 的 Rust 协议测试。Kimi KAP 从 HTTP 确认请求，再通过 WebSocket 上的手动压缩条目确认结果，已用上游补丁版 CLI 实测。DSH 通过原生 `/compact` 命令和 WebSocket 确认结果。回执引用的摘要序号、原生压缩 ID、开始及结束事件须对应同一条命令。DSH 没有可压缩内容时返回 `not_compacted`。Factory 的 `droid.compact_session` 回执必须标明原会话 ID 和正数的删除条数，空会话的原生拒绝会保留原因。

压缩期间可以向持久化聊天队列追加输入。只有收到终态后才会继续投递。会话快照、聊天历史和 `ChatReplica::manual_compaction()` 都可读取最近一次请求的状态与证据游标。客户端重连时按聊天游标补收中途的状态变化。控制器意外退出留下的待定请求会记为 `unconfirmed`。

### Codex 和 DSH 原生目标

创建聊天时加 `--goals` 即可启用并核实 Codex 或 DSH 的原生目标。目标绑定到原生线程，可在普通回合之间更新，也能在宿主重启后继续读取。以下命令操作已关闭控制器的聊天。控制器在线时，Rust 宿主直接调用 `ChatSession::set_goal`、`read_goal` 和 `clear_goal`。

```sh
kar chat new codex --goals
kar chat goal set '完成索引迁移' --chat "$CHAT_ID" --status paused --token-budget 100000
kar chat goal read --chat "$CHAT_ID"
kar chat goal clear --chat "$CHAT_ID"
```

目标更新通过聊天 SSE 和 `ChatReplica::goal()` 分发。查询时可以区分尚未收到原生报告、供应商确认没有目标、以及已设置的目标。DSH 使用原生目标 ID 与修订号更新或清除目标。可设置目标文本及 `active`、`paused`、`complete` 状态；DSH 使用回合上限，仅 Codex 支持这里的 token 预算选项。Codex 和 DSH 启动的目标续跑回合带 `native_continuation`，时间线会显示模型回复和工具调用。在线控制器观察到完整边界并取得工作区锁时，还会记录该轮的 Git 检查点。若恢复时才收到历史回合或工作区已由其他控制器持锁，聊天时间线保留原生记录，该轮检查点会显示为缺失。暂停目标后的当前回合仍可正常结束。

### 原生聊天输入队列

`enqueue` 在消息持久写入后返回请求 UUID。聊天打开时，控制器在上一回合结束并保存检查点后发送下一条。聊天关闭时，其他进程仍可入队或撤回；下次 `resume` 会在原工作区和原生会话中按入队顺序投递。命令行同时传入新提示词时，会先处理队列。

```sh
kar chat enqueue '先梳理入口' --chat "$CHAT_ID" --request-id "$REQUEST_ID"
kar chat enqueue '再检查刚才找到的调用点' --chat "$CHAT_ID"
kar chat queue --chat "$CHAT_ID" --json
kar chat resume --chat "$CHAT_ID"
```

同一聊天中，使用相同请求 UUID 和文本重试会返回已有回执。多个进程同时入队时，队列以持久写入的先后为准。`kar chat withdraw <请求UUID> --chat "$CHAT_ID"` 可以撤回待发消息。分叉聊天有独立队列。

`queue` 显示每条输入的投递状态。`pending` 等待投递，`sending` 表示已开始尝试，`sent` 表示传输层已写出或原生会话已确认，`withdrawn` 表示已撤回。如果进程在记录发送尝试之后、取得发送凭据之前退出，该条显示为 `uncertain`。恢复时会跳过这条并继续处理后续待发消息，避免重复触发工具或文件修改。模型回复和回合终态可用同一个 UUID 在 `history` 中查看。

Claude 持久化聊天把这个 UUID 一起发给原生进程。重启时，Rust 控制器在原工作区和会话里核对 UUID 与消息正文，找到唯一匹配的原生输入后将投递状态补为 `sent`。客户端从聊天事件流收到这次状态更新。若丢失的是模型回复的结束事件，回合仍显示为 `interrupted`。历史输入缺少匹配 UUID 时保留 `uncertain`。

Claude 的 `queued` 和 `started` 回执会保存到当前回合的 `native_input_receipt`。原生会话识别出已用过的 UUID 时，队列显示 `duplicate`，这一回合以 `invalid_request` 结束。原生进程在开始前取消输入时，队列显示 `cancelled`，回合显示 `aborted`。控制器会继续处理下一条待发输入。已用 Claude Code 2.1.322、2.1.330 和本地模型服务验证。两个 HTTP 客户端在断线重连后看到相同回执。重复 UUID 和开始前取消都没有产生额外模型请求，换用新 UUID 的相同文本正常投递一次。

Claude 可以在输入排队期间或两轮对话之间处理后台通知。这段回复和工具调用可通过 `history.notifications`、聊天 SSE 和 `ChatReplica::notifications()` 查看。后台通知自己的结束事件只更新那条通知。回合保存 Claude 结束事件的原生 UUID，控制器重启后再次收到同一个结束事件时会跳过，下一条输入仍等待自己的结果。如果 Claude 在同一个工具循环里开始处理排队的输入，通知的 `joined_input` 会记录该输入 UUID。

Rust 后端可调用 `ChatStore::enqueue(chat_id, request_id, text)`、`queued_inputs(chat_id)` 和 `withdraw(chat_id, request_id)`。持有 `ChatSession` 时可直接调用 `enqueue(request_id, text)`，需要等待队列处理完毕时调用 `drain_queue().await`。

### 按回合查看文件和回滚工作区

`kar chat` 会在提交提示词前和运行结束后分别保存 Git 树。两棵树之间的差异就是该回合的文件清单，包含新增、修改、删除。没有文件变更的回合也可作为恢复点。先从 `history` 取得回合 UUID，以下命令可从任意目录执行。省略 `--chat` 时选择最近的聊天。

```sh
kar chat history
kar chat changes <回合UUID> --json
kar chat rollback <回合UUID> --dry-run
kar chat rollback <回合UUID>
kar chat rollback <回合UUID> --before
```

默认恢复到指定回合结束时，`--before` 恢复到它开始前。回滚只应用这些回合涉及的文件变化；跨回合保留的聊天记录与原生会话可继续使用。回滚后继续发送会产生新的工作区历史分支，旧回合仍可查看和恢复。若目标路径上有冲突的本地改动，命令会列出路径并在写入前停止。若宿主在写入中途退出，重新执行同一条回滚命令即可继续。

Git 对象保存在聊天目录的 `<根聊天UUID>.git` 裸仓库。每个回合分别保存开始和结束时的 Git 引用。子聊天复用已有对象，并单独保存新回合的引用。Rust 的 `gix` 直接读写 Git 对象。工作区自己的 HEAD、分支和暂存区独立于这些检查点。检查点覆盖工作区目录内的已跟踪文件和未忽略的新文件，包含二进制、符号链接及 Unix 可执行位。普通目录也支持检查点，并遵循其 `.gitignore`。嵌套仓库、子模块工作树和聊天存储目录不参与快照。回合执行期间其他程序对同一工作区的修改也会出现在文件清单中。

Rust 后端可以直接读取 `ChatTurn.checkpoint.files`。在已打开且空闲的聊天上使用 `ChatSession::rollback`，关闭聊天后使用 `ChatStore::rollback`。传入 `CheckpointPoint { turn, before }` 和 `dry_run`，从返回值读取 `files`、`conflicts` 和 `applied`。旧聊天从升级后的下一回合开始记录检查点。缺少结束快照的回合会标为不可用。

### 从选中的回合继续聊

`kar chat fork` 把工作区恢复到所选回合结束时的 Git 检查点，并从该回合的原生锚点创建新聊天。新聊天包含所选回合及之前的问答。原聊天的完整时间线可以用原聊天 ID 查看。两个聊天共用同一个工作目录。

```sh
kar chat history --chat <原聊天UUID>
kar chat fork <回合UUID> --chat <原聊天UUID> '从这里换一种实现'
kar chat resume
```

省略提示词会进入新分支的交互模式。退出后，`resume` 默认继续最近使用的聊天，也可以显式传入新聊天 ID。SDK 使用 `ChatSession::fork_at(turn)` 或 `ChatStore::fork_at(chat, turn)`，返回的 `ChatSession` 可以直接 `send`。需要重新提供运行时环境时可用 `ChatStore::fork_at_with`。子聊天的 `ChatInfo.forked_from` 记录来源，继承的回合可以继续作为分叉点。

按回合分叉支持 Codex CLI 0.143.0+、Claude Code 2.1.323+、Pi RPC 1.6.0、OpenCode 2（实测 2.2.7、2.3.2）和 DeepSeek Harness 的原生 Web 会话（实测 0.2.2-rc.5），所选回合需要已经结束并保存了最终工作区检查点与原生锚点。操作会核对整个快照范围；如果额外的本地文件或编辑妨碍精确恢复，会列出冲突。被 Git 忽略且未跟踪的文件沿用检查点的排除规则。恢复和分叉成功后，新聊天才会出现在列表中。首次从该检查点发送新消息时会再次核对文件，避免等待期间同一工作目录发生变化。

OpenCode 2 通过所选回合的原生消息或工具调用 ID 找到结束标记，核对分叉回执后才恢复文件。连续输入相同文本也能区分。插件或外部程序通过 API 设置的指令只有当前值可查询。如果会话有这类指令，或选中位置之后存在可见的系统、模型、agent 或工作区切换，操作会在写入文件前停止。外部插件配置的历史版本不在聊天检查点的覆盖范围内。

Pi 1.6 在回合结束时记录原生分支的末条记录 ID。选择最后一轮时，Rust 调用 Pi 的 `clone`。选择较早的回合时，Rust 调用 `fork`，复制到下一条用户输入之前，并在子会话重新打开后核对复制的内容。旧聊天只要保存了 Pi 输入回执和最终检查点，也可以用原生历史核对分叉位置。若中间插入了另一次压缩等上下文变更，Pi RPC 无法表示这个切点，操作会在恢复工作区之前返回错误。

如果宿主在分叉途中退出，再次运行同一条 `chat fork <回合UUID> --chat <原聊天UUID>` 即可继续未完成的操作。通过 `history --json` 的 `pending_fork` 可以找到待完成的回合。恢复期间，共用该工作区的其他聊天会拒绝新回合，完成这次分叉后可重新发送。

## 接入自己的 Rust 后端

在同级项目的 `Cargo.toml` 中添加依赖：

```toml
[dependencies]
kar = { path = "../kar" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust,no_run
use kar::{runtime, Outcome, SessionOptions};

#[tokio::main]
async fn main() -> kar::Result<()> {
    let session = runtime("codex")?.open(SessionOptions::new(".")).await?;
    let turn = session.prompt("概括这个项目的结构。").await?;
    let report = turn.report().await?;
    if report.outcome == Outcome::Completed {
        println!("{}", report.text);
    }
    session.close().await
}
```

UI 可以同时通过 `session.subscribe(None)` 读取历史和后续记录。传入上一条记录的 `cursor()` 时，从下一条继续。子进程退出后订阅会结束，等待中的调用会返回错误。适配器重新打开原生会话时会分配新的 `stream_id`，序号从零开始。持久化时请保留两个字段。`observe::SessionView` 可以把实时或保存的记录还原为按 agent 分组的消息、工具状态和用量视图。

失败回合可从 `TurnReport.failure`、`ChatTurn.failure` 或实时事件 `turn_failure` 读取失败分类和供应商原文。Codex、Claude、Pi、OpenCode、Grok 和 CodeBuddy 会优先采用原生错误类别。CodeBuddy 的任务失败即使通过成功的 RPC 响应返回，也会结束为失败回合。浏览器中的 Pi Durable 从对应输入的原生回执读取失败原因。缺少结构化证据时，ACP 与 Pi Durable 可按供应商原文分类，无法识别时保留 `unknown`。界面可用 `FailureClass::advice()` 获取重试时机和是否需要用户处理。

额度重置时间 `resets_at_ms` 使用 Unix 毫秒。Codex 可读取提供方错误体，或将当前回合收到的已耗尽窗口关联到原生额度错误。多个窗口的重置时间冲突或出现子会话活动时，省略窗口推算值。Antigravity 从原生错误或独立的额度通知读取倒计时，按接收该通知的时间换算。失败详情随聊天日志持久化，SSE 重连沿用已记录的时间。Codex 从原生历史补回失败回合时使用历史本身携带的证据。

前端可通过 `session.snapshot()` 一次读取当前根会话的状态、模型、推理强度、服务等级和父子会话图。正在运行时，状态可区分等待模型、思考、生成文本、工具执行和压缩。子会话结束后，根会话仍按自己的事件更新。`session.await_idle().await` 等待当前和已排队的工作结束。`observe::SessionReducer` 可从完整记录按序还原同一份快照。

需要逐条处理事件的界面可以调用 `session.events(None)`。一条原生记录可能产生多个事件，保存返回的 `EventCursor` 可在同一条记录内继续读取。浏览器提供对应的 `snapshot()` 和 `subscribeEvents(cursor)`。

`prompt` 成功表示输入已发给本地 agent，`turn.wait()` 或 `turn.report()` 等待这一轮结束。繁忙时重复 `prompt` 会返回 `Error::Busy`。`queue` 返回输入 ID 和等待句柄，`withdraw` 可撤回尚未发送的输入。底层 `Session` 的队列和实时事件存放在宿主进程内。聊天产品可使用 `ChatStore::enqueue` 持久化待发输入和投递回执。主动终止当前工作请调用 `abort`。同时支持自动选择发送方式的 `deliver` 和带图片的 `prompt_images`，CLI 通过 `--image <文件>` 附加图片。

Grok 支持普通回合中的图片。使用 Grok 1.0.92 和本地模型服务分别发送了图文消息和纯图片消息，两次模型请求都包含原始 PNG 数据。

Grok 支持用 `steer` 和 `steer_images` 在运行中追加输入。每条插话都有独立 UUID。`queued` 表示接口答复已排队，`received` 表示 Grok 的会话进程回显了这条输入。Grok 1.0.92 的双客户端实测还核对了工具调用后下一次模型请求中的文字和原始图片。Grok 自行启动的后续回合有独立时间线和原生回合 ID，客户端断线后可以从游标补收。同一子任务多次唤醒父会话时，每次执行各有一个本地回合 ID。

Claude Code 支持在运行中的回合调用 `Session::steer`、`ChatSession::steer` 或 `steer_images`。每次追加使用独立 UUID。写入后，两个订阅者都可以从聊天事件流看到 `submitted` 和后续原生回执。Claude 在工具调用之间接纳输入时，消息留在原回合。原回复先结束时，Claude 可能把多条追加输入合并为一个后续回合。时间线会把这些消息按原顺序移到实际执行的回合。该回合的 `steered_from` 指向提交时的回合，`native_result_inputs` 按原生顺序列出这次结果涵盖的输入。Claude Code 2.1.322 和 2.1.330 已实测带图追加、双客户端断线续收和独立回合的文件检查点。2.1.330 还实测了在前后两个回合分别分叉，核对工作区文件和模型实际收到的历史。按回合原生分叉需要 2.1.323 或更高版本。

`steered_inputs` 中的 `queued` 表示运行时报告已排队，`accepted` 表示运行时已接纳，`follow_up` 表示换到后续原生回合。`cancelled`、`refused` 分别表示开始前取消和拒绝。若控制器在取得明确结果前退出，持久化聊天会把这次追加标为 `unconfirmed`，保留输入和 UUID。需要下次启动时自动投递的消息可通过 `enqueue` 加入持久队列。

运行中的 OpenCode 回合可以通过 `Session::steer` 或 `ChatSession::steer` 追加指令。OpenCode 1 使用同一会话的并发 prompt。发送后，聊天时间线立即显示该输入，原生回执到达时原位更新为已接受或已拒绝。OpenCode 2 在握手声明支持时使用 `_session/steering`。原生答复 `injected` 后，输入归到发送时的回合。若原生答复要求另起一轮，`steer` 会报告回合已结束，`Session::deliver` 会转入普通队列。即使回执晚于下一轮的开始，所有客户端仍能按原回合和输入 ID 重放。每轮的 `steered_inputs` 保存这些状态，消息的 `provider_id` 可用于查找对应输入。图片可用 `steer_images` 发送，前提是运行时支持追加指令和图片输入。

OpenCode 1.19.6、2.2.7 和 2.3.2 已使用真实 CLI 与本地模型服务验证。三个版本都在读取工作区文件后的下一次模型请求中带上了追加文字和 PNG，原回合只结束一次。两个 HTTP 客户端订阅同一聊天，其中一个在追加前断开，凭上次游标重连后，两端都只显示一条追加消息、一次工具调用和一条最终回复。聊天日志保存追加文字与回执，图片内容随原生请求发送。

Antigravity CLI 1.5.0 起可用 `steer` 或 `steer_images` 在同一轮追加输入。原请求可能在工具执行前就返回，聊天会等所有已发出的提示请求结束，并按提交顺序采用最后一条成功接管执行的请求结果。原生拒绝的追加输入保留在时间线上，标为 `refused`。控制器退出时仍待确认的输入标为 `unconfirmed`。普通提示、追加输入和队列消息的文字上限均为 185,000 个 UTF-8 字节。已有队列里超长的待发消息显示为 `too_large`，后续消息仍可按序投递。

Antigravity CLI 1.5.10、1.5.11 和 1.5.15 已用本地 Gemini 服务验证。一次追加文字和 PNG 后，模型的下一次请求包含原始消息、追加内容和真实 MCP 工具结果。两个 HTTP 客户端从同一聊天收取事件，一端在工具审批前断开。最后两个客户端看到同一轮的消息、工具记录和检查点。

长会话可以把 `SessionOptions.retained_bytes` 设为正整数，限制原生 SDK 内存回放中原始帧和工具载荷的 UTF-8 字节数。最新一帧始终完整。超限时旧帧的原始内容、工具参数、输出和结果正文会被清除，回放事件和工具卡片带 `elided: true`。消息、退出码、文件清单和游标仍然保留。在线订阅者和持久化聊天接收完整记录，聊天 SSE 的断线续传使用持久化日志。

工具卡片的 `input_partial` 标记当前输入是否仍是原始文本片段。完整参数到达时替换片段。Claude 和 Qwen 按原生会话、父工具和内容块索引区分并行调用。`ChatTurn::tool_groups(runtime)` 返回原时间线中的分组范围、操作计数、失败数和最后一个仍在运行的调用；文本或 agent 切换会开启新的分组。Codex 在本次连接内补报旧工具的输出或结果时，聊天按原生回合和调用 ID 更新原卡片。

### 人工审批

Factory Droid 1.17.2 的问卷已通过双客户端实测。宿主通过 `answer_question` 提交问卷，保留原生题号、选项描述、多选顺序和自填答案。行选择器显示供应商的标签和预选项，通过行 ID 提交。审批可通过 `select_policy` 选取 Droid 列出的策略；Droid 允许编辑计划时，`question_form()` 提供原始计划，提交修改后的全文才会发送 `proceed_edit`。运行时通知某项审批或一批问题已处理时，订阅者会收到待办清除事件。两个客户端同时回答同一问卷时，只有首个有效答复会发送给 Droid。

Cursor CLI 根会话的 `cursor/ask_question` 支持单选和多选。客户端显示选项文字，控制器通过 `answer_question` 提交选项 ID，也可以跳过或取消整组问题。`cursor/create_plan` 的正文和任务列表保留在待办里，控制器通过 `decide` 接受、拒绝或取消本次计划。默认的无人值守模式会取消这两类请求。已通过官方 Cursor CLI 和本机 AgentService 验证两个 HTTP 客户端在回合内看到同一工具调用和问题，同时回答时只有一次原生回执。断线客户端从原游标补齐后，双方的消息、工具和回合状态一致。协议测试另覆盖请求 ID 复用、多选和计划审核。该版本的子代理不支持交互提问。

订阅同一聊天的客户端可以从 `GET /snapshot` 的 `history.pending_requests`、SSE 的 `requests_changed` 或 Rust 的 `ChatReplica::pending_requests()` 同步待答请求。每条请求的 `controls` 包含表单、审批选项和可单独授予的资源。控制器正常退出或被强制终止后，观察端会收到清空待办的更新。

Codex 的原生提问、Claude 的 `AskUserQuestion`、Pi 的选择、确认和编辑对话框，以及标准 MCP 表单和 URL 请求可用 `PendingRequest::question_form()` 渲染。控制器通过 `ChatSession::answer_question` 提交或取消。Claude 的多选回答以原始问题文本回传，Pi 的限时对话框到期后会从待办中移除。[Claude 用户输入协议](https://code.claude.com/docs/en/agent-sdk/user-input) · [Pi RPC 对话框协议](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-extension-ui.md)

细粒度审批通过 `policy_options()`、`permission_resources()` 获取原生请求的选项。用 `select_policy` 选择供应商提出的策略，或用 `grant_resources` 指定 Codex 申请的文件或网络权限及其作用域。选择在控制器内对照同一请求的原始内容重新核对。文件权限子集保留原请求的拒绝规则和扫描深度。

设置 `SessionOptions.approvals = ApprovalMode::Host` 后，消费事件流中的 `Event::ApprovalRequested`，从对应记录的 `native` 读取工具名称、输入和允许的选项，然后用 `session.respond(id, result)` 回传供应商要求的 JSON。Codex 的命令审批可回复 `{"decision":"accept"}`，ACP 则使用请求中的 `optionId`。同一请求只能回答一次。默认的 `Deny` 会拒绝收到的权限请求。agent 自身决定哪些操作发起审批，因此该设置只作用于实际收到的请求。

界面连接到仍在运行的宿主时，先读 `session.snapshot().pending_requests`，再从同一快照的 `cursor` 订阅后续记录。每个请求带原生 ID、来源会话、agent 路径、请求类别和展示用的请求体。权限弹窗按 `request.decisions()` 返回的选项渲染，用户点击后调用 `session.decide(&request, decision).await`。Codex 可把请求中的文件和网络权限按回合或会话授予，命令和文件审批支持本次或本会话。ACP 的长期允许和拒绝对应供应商列出的原生选项。对输入问题，按请求中的问题 ID 收集答案，再调用 `session.answer_request(&request, native_payload).await`。这两个接口验证事件流和请求序号，能拒绝旧页面对已取消请求或复用 ID 的答复。实时答复、默认拒绝和原生取消都会反映在待办清单中。JSONL 回放可复原当时的待办状态，实际答复需要仍在运行的会话。

长回合可用 `Session::observe_stalls(threshold)` 或 `ChatSession::observe_stalls(threshold)` 观察。静默超过阈值会返回回合起始序号、静默时长、最后一条记录的游标与类型，以及当时的运行阶段。每段连续静默只报告一次，收到新记录后重新计时。历史回放可用 `AgentStatus::silent_for(now_ms, threshold)` 按指定时刻计算。

### 上下文占用

`Session::current_context()` 返回根会话最近一次确认的占用 token 数、上下文窗口和百分比。Codex 使用最后一次模型调用的计数，Claude 使用最近根助手消息的输入与缓存 token。Pi 和支持该查询的 Qwen 在回合结束时读取原生数据。Qwen 只报告估值时，占用 token 数为空。持久化聊天的历史和 SSE 副本也能读取同一份数据。

`Session::context_breakdown().await` 和 `ChatSession::context_breakdown().await` 可按需读取 Claude 的上下文分类和条目，例如记忆文件、技能、MCP 工具和预留空间。同时请求明细的调用共享一次原生查询。完整原生响应保持在适配器内。

### 查看模型实际收到的请求

Claude Code 2.1.274 及以上版本、配有官方 SDK 和原生模块宿主的 Pi 均可显式开启请求观测。在 Rust 中设置 `options.settings.model_requests = Some(kar::ModelRequests::Latest)`，然后调用 `Session::last_model_request().await` 或 `ChatSession::last_model_request().await`。CLI 对应 `--model-requests latest`。持久化聊天会保存这个启动选项。每次重新打开会话后，第一次收到当前连接的根请求之前，读数为 `None`。

结果包含请求时间、模型、供应商、请求格式和经过凭据遮盖的原生请求体。Claude 部分隐藏推理、隐藏正文或只登记了请求时，`withheld` 分别标明 `thinking`、`body` 或 `not_captured`。已有助手消息而原生索引尚未到达时，读取最多等待两秒，超时返回错误。请求体仅供当前宿主按需读取，聊天日志、快照和 SSE 均不包含这份请求体。关闭会话会清理专用临时目录。已使用 Claude Code 2.1.330 和回环模型验证工具审批前后读到的请求，以及恢复后首次读取的边界。

Pi 使用所选 npm 安装自带的 SDK，或为独立版指定的同版本 SDK，从根会话读取用户扩展异步改写后的请求体。支持 Anthropic Messages、OpenAI Chat Completions 和 OpenAI Responses。首次发送前以及关闭后返回 `None`。后续请求切换到未支持的 API，或请求体无法编码为 JSON 时，读取会返回对应错误。请求体只存于当前 Pi 进程的内存，由 Rust 宿主按需读取。启用这项观测需要能够加载原生模块的 Node。已用 Pi 1.6.0 的两种安装方式与本机模型服务核对请求内容。

已用 macOS、Pi 1.6.0、Node 24.15.0 和本机 HTTP 模型服务，逐一核对三种格式在服务响应前的请求体、嵌套 SDK 会话隔离，以及断线客户端续收的公开事件。适配代码和 Node 原生模块均使用 Rust。

### Token 用量与子会话

`Session::usage()` 返回本次打开原生会话以来的用量。输入 token 包含缓存读写，供应商能够单列时还会返回缓存和推理明细。`total` 是该会话自身的用量，`by_agent` 按 agent 路径列出已归属的份额，`unattributed` 是供应商合计中尚未归属的部分。`with_children` 包含已关联且有独立计数的子会话。嵌套子会话只计入一次。可用 `Session::usage_for(id)` 查询单个原生会话。

持久化聊天可通过 `kar chat usage --chat "$CHAT_ID" --json` 读取，也可读 `ChatHistory.usage` 或订阅后的 `ChatReplica::usage()`。列表按原生连接保存最近一次有实测数据的快照，每项带游标和按子会话 ID 索引的 `children` 明细。恢复后的连接单独记账，回放同一个游标不会累加。供应商只提供可能包含子会话的合计时，保留其原始统计范围。恢复缺少历史基线时，对应计数为空，例如 Qwen 恢复后的累计数。旧聊天日志缺少用量记录时，列表为空。

### 本机进程资源

`Session::resources().await` 和 `ChatSession::resources().await` 按需返回运行时进程树的常驻内存字节数 `rss` 和进程数 `processes`。同一进程位于子孙树和进程组时只统计一次。并发读取共用当次采样，结果不写入聊天记录。macOS 使用 Darwin 进程接口，Linux 读取 `/proc`，两者都核对进程启动时间。

两个会话共用 Kar 启动的 Morph Console 时，结果带有 `scope: "shared_process"`。关闭其中一个会话后，只要另一个会话仍持有该 Console，仍可查询整棵进程树。外部连接、无法取得完整统计、已退出的独占进程和未实现读取器的平台会返回 `None`。

### 中止 Codex 回合时清理命令

在 `kar chat new codex`、`kar run codex` 或 `kar open codex` 上加 `--end-turn-processes-on-abort` 可启用回合级进程清理。Rust 对应 `SessionOptions.settings.end_turn_processes_on_abort = true`。持久化聊天会保存该设置。macOS 和 Linux 先在发起提示时记录进程树，再在发送中止前采样。原生线程确认该回合中断后，宿主重新核对进程启动时间及进程组，清理本轮前台或已转入后台的命令和可归属的子进程。更早回合的后台命令、启动时已有的 MCP 服务及其新派生的进程继续运行。

清理回执独立于回合终态。宿主可以订阅 `Event::TurnProcessCleanup`，HTTP 客户端从聊天 SSE 接收 `turn_process_cleanup` 并在 `ChatTurn.process_cleanups` 读取同一结果。回执带原生回合 ID、中止请求 ID、实际发出信号的进程数，以及权限不足或采样失败的原因。清理在后台完成，下一条提示可以先开始。Windows 会照常发出中止，并报告无法区分进程组。原生自主续跑、手动压缩等没有提示前采样的回合也会报告原因。采样时已脱离进程树或尚未出现的命令可能继续运行。

已在 macOS 以 Codex 0.171.0-alpha.13、回环模型和本地 MCP 服务实测前台命令、已转入后台的命令及关闭此选项时的默认行为。控制器保持原会话可用，重连客户端将清理结果归回被中止的回合。

### 中止 Grok 回合时清理终端

Grok 的 ACP 终端由 Rust 宿主创建。供应商可以读取有大小限制的输出、等待退出、终止或释放终端。每个终端绑定到创建它的原生会话。提供参数数组时按原样传入，没有参数数组时按 Grok 的协议执行整条 shell 命令。

创建聊天时加 `--end-turn-processes-on-abort` 后，Grok 确认当前回合中止，Rust 会结束这一轮在根会话创建的终端。此前回合和子会话的终端继续运行。Grok 仍然可以读到已停止终端的输出和退出状态。每次已发出的中止请求都有独立清理回执。如果原生回合抢先正常结束，回执报告清理数量为零。省略开关时，命令保留供应商默认的生命周期。Unix 按终端进程组发送信号，已脱离该组的命令可能继续运行。

macOS 实测使用 Grok 1.0.92 和回环模型。工具命令先写一份文件，等待后再写第二份文件。启用清理后，第二次写入被阻止。使用默认设置时，第二份文件出现。原聊天随后正常完成下一轮，两个 HTTP 客户端续收后得到相同的回合记录和清理回执。

### 原生子任务

`Session::tasks()` 和 `ChatSession::tasks()` 返回供应商报告的子任务，包括所属会话和 agent、状态、工具调用 ID，以及原生提供的描述、输出或结束时间。Codex、Claude、Qwen、Qoder、Kimi KAP、DSH、OpenCode 和 Grok 的原生任务通知会进入实时记录，持久化聊天的快照与 SSE 也包含任务列表。控制器关闭后可用 `kar chat tasks --chat "$CHAT_ID" --json` 读取最后记录的状态。

OpenCode 2.3+ 使用原生后台子会话通知。主回合结束或控制器重启后，新收到的子会话回复、工具输出和完成状态仍归到发起它的回合。后续回合显式续用同一个子会话时，新的输出归到续用它的回合。`ChatTurn.child_sessions` 保存子会话与父会话的对应关系，SSE 重放可恢复嵌套归属。旧版 OpenCode 的标准 ACP 消息按其携带的子会话元数据归属。

在线控制器调用 `stop_task(id)` 可停止 Codex 子线程当前回合，或按任务 ID 停止 Claude 和 Qoder 的后台任务。DSH 支持停止当前会话显式放到后台的 Bash 或 PowerShell 作业。Rust 会核对发起调用的工具、参数和任务归属，收到原生终止事件后返回。已用真实 Bash 作业验证。Codex 在发送中断前核对该线程属于当前会话，并使用亲自观察到的活动回合 ID。任务状态随供应商的后续通知更新。

### 工具时间线与会话分叉

`observe::SessionView` 按来源 agent 和调用 ID 聚合工具卡片。`Tool.input` 保存最新参数，`preview` 处理输出快照和增量，`result` 保存终态、退出码、文本或图片等结果块，以及供应商确认的文件变更。Codex 的失败文件操作不会进入已修改清单。`AgentView.turn_diffs` 按回合 ID 保存最新完整补丁。这些信息也出现在 `kar run --json` 和回放中。Codex、Claude、Qwen、ACP 和 Pi 的字段覆盖取决于各自协议。

通过 `runtime.fork(source_id, through, options).await` 取得可独立继续的原生会话。把所选回合的 `Event::ForkAnchor` 传给 `through`，子会话会包含该回合的回复。Codex 0.143.0 起使用回合 ID，Claude 2.1.323 起使用可截断的根助手消息 UUID，DSH Web 使用回合结束事件的序号，Pi 1.6 RPC 使用活动分支在该轮结束时的记录 ID。Pi 会核对原生协议能否精确复制到该位置。`through = None` 复制整段历史。标准 ACP 在握手声明支持 `session/fork` 时接受整段复制。已用 OpenCode 1.19.6 和 2.2.7 核对两条分支的模型请求，确认子会话继承了分叉前的消息，原会话续聊时保留自己的历史，也没有收到子会话的新消息。OpenCode 按指定回合复制需要 2.x 的原生消息锚点。

```sh
kar open codex --cwd . --fork "$SOURCE_SESSION" --through "$TURN_ANCHOR"
kar open opencode --cwd . --fork "$SOURCE_SESSION"
```

Claude Code 2.1.326 及以上可用 `kar chat ask '解释刚才的决定' --chat "$CHAT_ID"` 在原生临时分叉中提问。回答来自源会话上下文，单次提问禁用工具和 MCP，不向父聊天追加回合。Rust SDK 的 `Runtime::side_question` 支持指定模型、工作目录、环境和超时。

### 工具和多 agent

`SessionOptions.mcp_servers` 接受命名的 stdio 或 HTTP 工具服务器。Codex、Claude Code、Qwen Code、Kimi KAP 和 ACP 分别通过各自的会话配置接入，ACP 会检查 HTTP 能力。Claude 的配置通过临时私有文件传递。DSH 在每个会话的私有启动配置中注册 stdio 服务，等待所有服务激活后才打开聊天。服务名支持 1 到 32 个 ASCII 字母、数字、下划线和连字符。工作目录是聊天工作区，命令与参数分别传入。DSH 会拒绝 HTTP 服务和附带环境变量的配置。恢复时由宿主重新提供 MCP 服务，前一轮的工具结果保留在聊天记录中。已用两个版本验证工具调用、同时打开的会话互相隔离、恢复时更换服务，以及名称冲突或程序缺失时拒绝启动。

Pi RPC 支持会话级 stdio 和 HTTP MCP 服务。工具以 `mcp__服务名__工具名` 直接提供给模型，恢复原聊天时由宿主重新传入服务。Kar 托管的 Pi 聊天忽略用户和工作区的 `mcp.json`。每个会话独立注册，关闭会话会释放其连接。名称中的连字符会映射为下划线，映射后冲突会导致打开失败。RPC 会拒绝服务配置中的非空 `env` 或 `headers`。使用 npm 版 Pi 或为独立版配置同版本的官方 SDK，并在 PATH 上提供能加载原生模块的 Node。Rust 适配器通过 Pi SDK 自带的 CLI 启动 RPC 会话。已用 macOS 上的 Pi 1.4.0 和 1.6.0 核对两种传输、服务名冲突、双客户端续收、工具过滤和重启后的服务替换。

`agents::Crew` 负责子会话容量、并行任务、后续输入和结果读取。命令行的 stdio MCP 服务提供 `runtimes`、`spawn`、`run`、`send`、`wait`、`list`、`interrupt` 和 `close`。关闭客户端连接会释放该服务拥有的会话。

## 适配器与验证范围

| 协议 | 注册的运行时 | 实际二进制联调 |
| --- | --- | --- |
| Codex app-server | Codex | 0.171.0-alpha.13 |
| Claude stream-json | Claude Code | 2.1.330 |
| Agent Client Protocol，JSON-RPC over stdio | OpenCode、Cursor、Grok、Kimi、Devin、CodeBuddy、Antigravity、旧版 DeepSeek Harness、旧版 Rust 适配器保存的 Factory 聊天 | Cursor `2026.11.22-e9042b2`（模型参数、工具、交互提问、原生会话恢复及离线队列）、OpenCode 2.3.2、Grok 1.0.92（带图输入、回合插话和终端清理）、Kimi 官方 2.7.2、DSH 0.2.1-rc.5（创建及恢复会话）、Antigravity 1.5.10、1.5.11、1.5.15（带图追加、工具调用和重连，模型切换另有协议测试） |
| Factory 原生 stream-jsonrpc over stdio | Factory Droid | 1.17.2，实测目录、工具、输入中断、队列续投、崩溃恢复和手动压缩 |
| DeepSeek Harness 原生 Web，本机 HTTP 和 WebSocket | DeepSeek Harness | 官方 0.2.2-rc.5，实测工具、排队与撤回、崩溃恢复、分叉、目标续跑和手动压缩 |
| Kimi KAP，本机 HTTP 和 WebSocket | Kimi | 上游补丁版 2.7.2-botiverse.1，实测工具调用、重启续聊、队列和手动压缩 |
| Qoder 原生 worker / SDK daemon，stream-json | Qoder | 1.1.78 完成对话和重启续聊，1.1.83 验证 daemon 握手和会话路由 |
| Qwen SDK channel，stream-json | Qwen Code | 0.28.4 |
| Pi RPC | Pi | 1.6.0，实测工具调用、双客户端续收、崩溃恢复与按回合分叉 |
| Cybion Agent API，HTTP | Cybion（显式启用） | 本机 HTTP 协议测试 |
| Mister Morph Console Runtime API，HTTP 和 WebSocket | Morph（显式启用） | Morph 0.3.128 |
| 浏览器内嵌 Harness，WebAssembly | Pi Durable（浏览器宿主提供） | Pi Durable 1.6.0，Chromium 145，IndexedDB |

Codex、Claude Code、OpenCode、Pi、Qwen Code 和 Morph 均通过真实程序的发送、回复与恢复测试，模型服务运行在本机回环地址。Claude 的审批用例确认临时文件只在宿主允许后才创建。Qwen 的恢复用例检查了模型实际收到的历史消息，并验证了中断。九个真实程序用例均在 PATH 不含 Node.js 时通过，供应商程序可以自带运行环境。其余 ACP 注册项尚待逐个验证。可在 `kar open` 或 `kar run` 中用 `--bin <路径>` 选择供应商程序。SDK 也接受 `Runtime::new` 注册自定义 ACP 程序。

Qoder 新聊天按 CLI 版本选择传输。1.1.78–1.1.80 用 worker，1.1.81 起为每个原生会话启动独立 SDK daemon。以前按 ACP 保存的聊天仍用原传输恢复。1.1.78 的真实 worker 在隔离 HOME 中使用内置模型回放，两个独立进程通过同一原生会话 ID 对话，实测核对了同一份原生转录中的两轮问答。1.1.83 在匿名环境完成 daemon 握手和创建会话，随后要求有效凭据；该版本的发送、恢复、任务控制与手动压缩已通过 Rust 协议测试，尚待已登录的真实 CLI 验证。

Kimi 新聊天先按 CLI 版本和服务端公布的能力选择 KAP 或 ACP。可用 `kar open kimi --bin <路径>` 指定 CLI。恢复时沿用聊天创建时保存的协议和原生会话 ID。KAP 实测使用 Kimi 补丁版本和本机模型服务。两个订阅者在回合结束前看到了同一次 Bash 调用。重连后，两端的时间线一致。重启后，模型收到了前一轮问答，排队的输入只投递了一次。另一个测试只保留到工具开始时的本地日志，再用 Kimi 已完成的原生回合恢复。客户端从原游标收到了完整回复和工具输出，模型调用次数保持不变。官方 2.7.2 则通过 ACP 完成了重启续聊测试。[Kimi 补丁版本](https://github.com/botiverse/kimi-code-sdk/tree/4ca80ab5cdf3e51391804cb7cce4d0ccae32daff)

Kimi ACP 在原生握手声明支持时，可用 `--add-dir` 给新聊天加入额外目录。Kimi 只在创建时接收这些目录，持久化聊天重开时沿用原生会话。使用官方 2.7.2 和本地模型服务，已核对新建和重启后两次模型请求的系统消息均包含额外目录。

DeepSeek Harness 使用认证后的本机接口接收带序号的原生事件。可用 `kar open dsh --bin <路径>` 指定程序。DSH 0.2.2-rc.5 起选择 Web，较早版本选择 ACP。0.2.1-rc.5 的 ACP 声明了 `session/resume`，已验证关闭控制器后能重新打开原生会话。以前通过 ACP 保存的聊天仍按原协议打开。实测中，两个客户端在回合结束前都收到了 Bash 调用；断开并从各自游标补收后，两端的聊天历史一致。关闭控制器后排队的消息在原会话触发了一次模型请求。按回合分叉的测试同时核对了工作区文件和模型实际收到的消息。崩溃恢复测试把本地日志分别截到回执前、工具开始后和回复写入一部分之后。恢复时按请求 UUID 和已结束的原生回合补回时间线，随后提交相同 UUID 不会再次调用模型。控制器离线期间的工作区状态不能据此补建 Git 检查点。测试使用真实 DSH CLI 和本机模拟模型服务。会话文件路径取自 DSH 导出的元数据，可在文件生成后查询。[DSH 原生会话接口](https://github.com/deepseek-ai/deepseek-harness/tree/9c2670d3bce5275e0d1610272958121af87787bb/packages/api/session-controller)

`kar models factory` 和 `kar inventory factory skills --cwd <工作区>` 通过 Droid 自有协议读取目录。Rust SDK 可调用 `Runtime::catalog`。默认查询无需创建聊天，模型使用 Droid 可接受的 ID，技能结果包含所属范围及原生的 `projectAvailable`。新聊天通过 `droid.initialize_session` 创建，重启时用 `droid.load_session` 打开原会话并核对工作目录。老版本 Rust 已经保存的 ACP 聊天沿用 ACP。输入的 UUID 同时用作 Factory 的消息 ID，以同一 ID 的原生回合终态确认结束。Droid 原生上下文占用和包含缓存的 token 用量可在聊天快照中读取。

Factory 崩溃恢复使用 CLI 保存的 JSONL。Rust 核对会话文件头中的 ID 和目录，从指定请求的用户消息顺着消息序号与父 ID 读取，直到匹配的原生回合终态。双客户端实测分别保留了回执前和工具调用中的本地日志。两个恢复场景都补回了第一轮的回复与工具结果，即使原生会话已经完成第二轮，重试也没有增加模型请求。缺少对应终态的回合会保留为中断。

Pi RPC 恢复使用 v3 会话文件。控制器根据已保存的原生输入回执、提示词、工作目录和当前分支确认回合，再核对已有的回复片段和工具结果。两个客户端可以从不同游标收到同一次补全。旧日志缺少回执、分支有歧义或文件尾部不完整时，回合保留为 `interrupted`。离线期间的文件变更无法补建回合结束时的 Git 检查点。

Pi 1.6 的一次输入可以包含多次模型请求。控制器等待原生 `agent_settled` 事件，再发布回合终态、记录上下文占用并投递下一条消息。实测让模型端第一次返回 HTTP 503，Pi 在同一回合内重试成功。只保留到输入回执的本地日志后再恢复，也能补回这一轮的最终回复，恢复期间模型请求数保持不变。

Qwen 以 `initialize` 作为首帧，确认原生会话 ID 后再接受提示。它通过标准输入接收会话级 MCP 配置，权限请求遵循宿主设置。模型列表来自 `get_available_models`。支持上下文查询的版本还可通过 `session.context_usage().await` 返回原始报告，其中 `isEstimated` 标记估算值。

Kar 提供 Rust API，并用 JSONL 保存会话记录。目录、账号、更新和压缩接口按运行时提供，缺少实现的操作会明确返回 `Unsupported`。

### 后端能力与支持范围

截至 2026-11-24，各运行时支持的能力及待验证项如下。

| 领域 | Rust 已有 | 后续优先事项 |
| --- | --- | --- |
| 聊天时间线与断线恢复 | 本地工作区和原生会话绑定、按回合保存的消息与推理、工具卡片、补丁、回显去重、中断恢复、跨控制器重启的 SSE 多客户端续传、Rust HTTP 客户端按已提交游标自动重连，Factory、DSH、Kimi KAP、Codex、Pi RPC、Morph 和 OpenCode 2 可从原生历史补全已结束的回合，失败原因含 OpenCode、Grok、CodeBuddy 的专有分类、Antigravity 倒计时、Codex 当前回合的额度窗口及 Pi Durable 原生回执，打开会话时可报告原生会话丢失、工作区冲突和部分凭据或配置错误 | 其余供应商的专有失败原因、设置确认和更多原生历史补全能力 |
| 审批与提问 | 多客户端待办、跨供应商提问表单和带游标的原生答复，Codex 逐项授权及策略建议、Claude 原生策略建议、ACP 按选项 ID 授权、Cursor 根会话的单选、多选和计划审核、Factory 原生问卷、行选择器、授权策略和计划编辑，Kimi KAP 和 DSH 原生审批和提问 | 其余供应商的专有交互、非标准 MCP 表单和运行时策略更新 |
| 代码审阅 | Codex 回合补丁和已生效文件列表，多协议工具输入、输出及终态，Claude、Qwen、Qoder、Kimi KAP 和 DSH 流式工具参数、按 agent 分组、Codex 回合结束后的工具输出归属、OpenCode 后台子会话的迟到回复与工具结果、Pi Durable 的根工具和嵌套工具卡片、Morph 和 Cybion 的原生工具输入与结果 | Cybion 工具结果仍待真实部署验证 |
| 工作区与模型设置 | 启动时选择模型、设置并核实推理强度和服务等级，支持原生会话命名、额外工作目录、会话级 MCP、按供应商支持的系统指令与工具限制、Pi RPC 的 stdio 和 HTTP MCP 服务、DSH 原生标题及恢复、会话级系统提示词、工具限制、可写目录、日志上传控制及 stdio MCP 服务、Claude 用户级配置隔离、Qwen 后台记忆默认开关与生效状态及 Codex、Claude、Grok 的结构化输出，七个原生适配器的模型选择回读错误和 Pi Durable 的浏览器模型回读，十二个运行时的独立技能目录，包括 DSH 会话前的全局技能快照、三个运行时的 MCP 服务目录、Codex 和 Claude 的 MCP 工具目录、Pi npm 版及配有同版本官方 SDK 的独立版的注册工具目录、Claude 和 Pi RPC 命令目录、Pi SDK 的模型能力、默认模型和按供应商刷新、Grok 原生模型目录、Cursor 参数化模型目录及推理强度回读、Grok 设置表单、宿主自定义工具的 HTTP MCP 服务和工具列表变更通知、按会话移除继承的环境变量、按原生连接报告 MCP 传输类型、聊天创建前按安装版本查询能力和设置限制 | 其余供应商的专有设置与验证 |
| 会话续写与分叉 | 按聊天 ID 回到原工作区续聊、按请求 UUID 幂等发送，Codex、Claude、Pi RPC、DSH 和 OpenCode 2 可从指定回合同步恢复文件及原生上下文，声明能力的 ACP 可复制整段会话，原生持久化聊天和浏览器均可跨重启投递队列，OpenCode 1 和声明支持的 OpenCode 2 可在回合中追加指令并同步回执，Antigravity 支持同轮带图追加、请求交错结算和跨客户端续收，Claude 支持带图追加指令，可区分同轮接纳和另起一轮、重复输入、开始前取消及插入的原生通知，2.1.326+ 支持临时分叉提问，Grok 支持带图插话、原生排队回执和自主回合，Codex 和 Grok 支持中止回合时分别清理所属命令及托管终端 | 其余供应商按回合分叉取决于原生协议支持 |
| 子任务与并行 agent | 会话关系图、Crew 并行调度、Codex 和 DSH 原生目标读写和续跑，多协议原生任务列表，Codex、Claude、Qoder 和 Kimi KAP 定向停止子任务，DSH 定向停止后台 shell 作业，OpenCode 和 Cursor ACP 嵌套子会话、后台输出与原回合的归属 | 其余供应商的子 agent 归属及控制 |
| 用量与长任务 | 按原生连接、agent 路径和独立子会话统计 token，支持离线查询和 SSE 续收，六个运行时的聊天前账号额度查询、Codex 幂等兑换额度重置、Claude 原生重置查询和带持久化回执的兑换、多协议上下文占用、Claude 上下文明细、静默诊断、按回合保存压缩边界，Qoder 逐次累加根会话用量，Kimi KAP 按 agent 汇总逐步用量并核对回合终值，DSH 汇总原生请求的用量，Factory 按回合累加自身和子任务 token，Codex、Claude、Pi、OpenCode 2.x、Grok、Qoder、Kimi KAP、Factory 和 DSH 可关联手动压缩请求与最终结果 | 更多供应商的上下文占用及专有压缩协议 |
| 安装与运维 | 发现 PATH 多份安装及供应商安装目录、CLI 身份与版本就绪检查、安装、11 个运行时的账号状态、15 个运行时的凭据声明、7 个运行时的宿主登录、9 个运行时的结构化退出、OpenCode 2 的供应商登录与密钥管理、Pi 1.6 的独立凭据及模型配置、供应商登录菜单、密钥表单、SDK 原生 OAuth 交互和宿主身份、状态查询与退出、Qwen 的供应商配置、部分更新接口、Codex、Pi、Factory 和 DSH 原生会话文件定位、macOS/Linux 本机进程树的内存和进程数，Claude 和配有 SDK 的 Pi 根会话的原生模型请求观测 | 其余运行时的宿主交互式登录、部分账号资料查询、Windows 原生环境实测，以及更多供应商实机验证 |

应用可以通过 `serve_mcp_tools` 把 Rust 函数提供给本机的原生会话。保留返回的服务句柄，把 `host.entry()` 插入各会话的 `mcp_servers`。每次初始化分配独立 MCP 会话 ID，两个会话可以同时使用同一个调用 ID。处理函数可通过 `CancellationToken` 响应取消、客户端断开或服务关闭。

```rust,no_run
use kar::{SessionOptions, host_tools::HostTool, serve_mcp_tools};
use serde_json::json;

# async fn example() -> kar::Result<()> {
let schema = json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]});
let lookup = HostTool::new("lookup", schema.as_object().unwrap().clone(), |args, _cancel| async move {
    let name = args["name"].as_str().ok_or("expected a name")?;
    Ok(format!("Requested {name}").into())
});
let host = serve_mcp_tools("project", vec![lookup], None).await?;
let mut options = SessionOptions::new(".");
options.mcp_servers.extend([host.entry()]);
let session = kar::runtime("codex").unwrap().open(options).await?;
# session.close().await?;
# host.close().await;
# Ok(())
# }
```

`host.set_tools(...)` 原子替换工具列表，并向每条已连接的 MCP 事件流推送 `notifications/tools/list_changed`。进行中的调用继续使用原处理函数。模型何时看到新列表由运行时决定，可通过 `runtime.mcp_tools_list_changed()` 查询。Grok 会等待当前会话报告宿主服务就绪再返回。Codex 把宿主服务列为必需项，支持分会话报告的版本还可通过 `session.snapshot().mcp_startup` 查看各服务最新状态。服务监听回环地址，重启宿主后需要重新注册处理函数，并向恢复的会话提供新地址和令牌。

前端可以从 `session.capabilities().mcp_transports` 读取已确认的传输类型。缺省表示当前版本或连接尚未验证，空列表表示不支持。DSH 报告 stdio。已识别的 Qwen Code 0.26.0 之前的版本只接受 stdio，配置 HTTP 工具会在启动前返回错误。Pi 配置会话级 MCP 并完成 SDK 工具注册后报告 stdio 和 HTTP。Factory Droid 1.17.2 已验证新建会话调用 HTTP 工具、在原会话恢复后调用 stdio 工具，两次结果均回到模型。通过安装探测确定 Factory 版本时会报告相应的传输类型。Codex 的 `mcp_startup` 能力说明首个模型请求可能在部分服务仍在启动时发出，服务的实时状态可从快照读取。

当前 Rust 通过 RPC 驱动 Pi 原生进程，请求观测和会话级 MCP 使用 Pi SDK 的 Node 宿主。浏览器接入 Pi Durable，Cursor 使用 CLI 的 ACP。Pi 和 Cursor 的进程内 SDK 模式尚未完整支持。

产品服务可以复用 ChatStore 的会话绑定、每回合 Git 检查点、文件清单和工作区回滚。原生会话分叉复制 agent 的对话历史。多客户端可通过同一 SSE 端点订阅。

### 浏览器中的 Pi Durable

同一个 crate 可以编译为 WebAssembly。Rust 控制器直接使用浏览器宿主打开的 Pi Durable `Harness`、模型目录和存储。浏览器项目需提供版本对应的 `@earendil-works/pi-durable`、`@earendil-works/pi-ai` 和 `@earendil-works/chord`（均验证于 1.6.0）。构建命令如下。

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130 --locked
cargo build --lib --release --locked --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir target/browser target/wasm32-unknown-unknown/release/kar.wasm
```

将生成的绑定和 Wasm 文件随前端发布，并由打包工具解析宿主依赖。假设应用已经打开 `harness`，并向其传入了同一个 `models` 目录：

```js
import init, { PiDurable } from "./target/browser/kar.js";

await init();
const runtime = new PiDurable(harness, models);
const session = await runtime.open({ cwd: "/workspace", model: "provider/model-id" });
const submission = await session.prompt("解释当前任务", crypto.randomUUID());
const receipt = await session.wait(submission.id);
const records = session.records();
await session.close();
```

`queue(text, requestId)` 将后续输入写入 Harness 存储，`withdraw(requestId)` 撤回尚未开始的输入。再次提交同一请求 ID 时会返回原生回执。`close()` 解除观察，`abort()` 请求停止执行。宿主控制存储和 Harness 的生命周期。重新打开 `runtime.open({ cwd, resume: session.id })` 后可继续观察。原生进程和浏览器共用事件记录和游标格式。

同一个浏览器 Harness 上的多个观察会话可同时订阅一个对话。后加入的客户端能从 Pi Durable 当前状态读取正在执行的工具及其参数、已有输出。根工具的最终结果可包含文字、图片等内容，嵌套工具按原生摘要报告成功或失败。`subscribeEvents(cursor)` 可从同一控制器已确认的事件位置续收；重新 `open` 会创建新的观察流。已在 Chromium 和 IndexedDB 上用真实 Harness 验证中途加入、嵌套工具失败和断线补收。

浏览器集成测试需要 Node 24、上述三个宿主依赖、`esbuild@0.28.2` 和 `playwright-chromium@1.58.1`。构建 Wasm 后，安装该版本 Playwright 对应的 Chromium 并运行 `node tests/browser/live.mjs`。测试默认从项目的 `node_modules` 加载依赖。自定义路径见[浏览器测试入口](tests/browser/live.mjs)。测试只访问回环地址上的模拟模型。

### Mister Morph 本地会话

`kar models morph` 查询 Console 配置的模型档案。`kar run morph '任务' --cwd . --model default` 在当前目录打开持久化 topic 并提交任务。`--resume <topic-id>` 恢复同一个 topic，并将工作目录绑定到本次指定的位置。SDK 支持 `runtime("morph")`、指定状态目录的 `community::Morph::local(program, state_dir)`，以及通过已知本地地址和 token 接入的 `community::Morph::attach(url, token)`。

持久化聊天收到 Morph 的提交回执后，会保存任务 ID 和已显示的执行记录位置。控制器重启时，若该任务已结束，`chat resume` 会从原 topic 的任务档案补齐工具结果、回复和回合状态。多个客户端可用自己的聊天游标补收同一次恢复。同一请求 UUID 再次提交会返回已有回合。缺少任务回执或原生记录有缺口时，回合保留为中断。原任务仍在 Console 里运行时，`chat resume` 会按回执接回同一任务。HTTP 客户端收到 `morph_reattached` 更新后，用 `id` 找到原回合，将其状态改回 `running`，并将 `open_tools` 列出的工具恢复为执行中。Rust 的 `ChatReplica` 会自动应用该更新。原生任务结束后保存该回合的文件检查点，再按入队顺序投递后续消息。验证使用官方 Morph 0.3.128，在工具运行中强制结束控制器，保持原生 Console 运行，并用两个 HTTP 客户端核对了续收的时间线。

工具卡片显示 Morph 上报的调用参数、运行中输出和最终结果。`bash` 和 `powershell` 返回完整原生输出格式时，卡片也显示退出码。已在 Morph 0.3.128 上通过本机模型请求执行 `printf`，核对了命令、结果和退出码。

适配器优先连接状态目录中已运行的 Console；需要时启动本机进程，在最后一个持有它的会话关闭时停止该进程。任务通过 HTTP 提交和取消，WebSocket 推送的累积快照会去重，并在终态使用原生任务结果补齐缺失文本。Rust 宿主默认拒绝待审批操作；使用 `ApprovalMode::Host` 时，调用 `session.respond(id, json!({"decision":"approve"}))` 或提交 `"deny"`。已在 macOS arm64 上用官方 Morph 0.3.128、隔离状态目录和本机模型服务器验证双会话共享、消息、关闭及恢复。

### Cybion 远端会话

SDK 可通过 `kar::community::Cybion::new(base_url, api_key)?.open(options).await?` 获取同一个 `Session` 类型。CLI 从[连接配置](src/community/cybion.rs)读取部署地址和密钥，支持 `kar open cybion` 和 `kar run cybion '任务' --resume <thread-id>`。新建时可通过 SDK 的 `open_thread` 指定标题和推理强度。

工作目录和工具由远端 Worker 管理。文本、图片和运行中追加的输入均提交到 Thread；队列留在宿主进程，远端状态和新增历史通过轮询读取。函数调用和自定义工具调用均保留输入，非文本工具结果会完整保留原始值。追加输入仍在等待执行时，后续排队消息会等待当前回合结束。恢复后只记录这次连接观察到的新历史。`abort()` 会向远端请求取消，`close()` 只释放本地连接。回环测试覆盖这两种行为以及原生 HTTP 拒绝响应、请求鉴权、图像、队列撤回和 CLI 恢复。尚未在真实 Cybion 部署上联调。

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

协议测试使用 Rust 子进程模拟五种供应商接入方式，覆盖多轮控制、审批、排队、撤回、取消、错误、会话恢复、事件回放以及 CLI 和 MCP 的正常入口。真实 CLI 测试需要对应的可执行文件，测试创建独立的配置目录。可单独选择已安装的运行时。实机测试使用的程序路径和环境变量见 [tests](tests) 下对应的测试文件。

```sh
cargo test --locked --test native_live actual_codex_turn_and_resume -- --ignored
cargo test --locked --test codex_process_cleanup -- --ignored
cargo test --locked --test native_live actual_claude_turn_and_resume -- --ignored
cargo test --locked --test native_live actual_opencode_turn_and_resume -- --ignored
cargo test --locked --test native_live actual_pi_turn_and_resume -- --ignored
cargo test --locked --test native_live actual_claude_waits_for_host_approval -- --ignored
cargo test --locked --test claude_request_live -- --ignored
cargo test --locked --test qwen_live -- --ignored
cargo test --locked --test morph_live -- --ignored
cargo test --locked --test kimi_live kap_tool -- --ignored
cargo test --locked --test kimi_live official_cli -- --ignored
cargo test --locked --test grok_live -- --ignored
cargo test --locked --test factory_live -- --ignored
cargo test --locked --test dsh_live native_ -- --ignored
cargo test --locked --test dsh_live legacy_ -- --ignored
cargo test --locked --test skill_inventory_live pi_registered_tools -- --ignored
```

