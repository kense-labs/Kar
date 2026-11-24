# Kar 后端验收记录

固定范围见 [backend-acceptance.toml](backend-acceptance.toml)。以下记录对应 2026-11-24 的实际运行。每行的状态只覆盖该行列出的供应商、版本、平台和操作。测试名称是 Cargo 可接受的过滤器。

运行平台为 macOS 27.2.1 arm64，Rust 与 Cargo 版本均为 1.100.0。下表的 Kar 可执行行为对应提交 `f3f7e5ec653afe3c866dd3c3d2e384825d752f79`，格式门禁对应 `dc68531c73f125149bbc933718f7b5c771333575`。真实供应商程序的模型流量由本机 HTTP 模型服务响应。输入 UUID、工具结果、模型请求次数及重连游标均由测试读取。

## 已验收的具体行为

| 上游位置 / 公共用例 | Kar 验收编号与范围 | 供应商、版本、传输及证据类型 | 操作与期望 | Kar 测试与实际证据 |
| --- | --- | --- | --- | --- |
| `docs/spec/session-graph-and-cursor.md`、`session.cursor-replays-without-loss-or-duplication` 对应的聊天事件行为 | realtime · 两个独立 HTTP 客户端 | Kimi 2.7.2-botiverse.1，KAP HTTP/WebSocket，实机 | 两端订阅同一聊天，分别在工具参数仍不完整和第一轮结束时断开；停止 SSE 服务，关闭并重开原生会话，另一个聊天也产生事件。两端以各自的 Last-Event-ID 重连 | K1。本次断线前分别收到 17、5 条，续收后各 32 条。两端补齐后的事件与磁盘日志在游标、前驱、内容、顺序和数量上逐项相等。重复应用同一事件返回 false。最终两轮时间线相同；另一个真实聊天的游标返回 HTTP 400。 |
| `tests/kimi/kimi-kap-projection.test.ts` 的三个工具输入断言 | tools · 流式参数与最终输入 | Kimi 2.7.2-botiverse.1，KAP 实机；另有 Rust 协议投影测试 | 分两段返回 Bash 参数。在工具运行期间观察已收到的文本；原生结构化参数到达后清除 partial 标记。重复快照、缺少 updated、ended 重放均须归入同一个调用 | K1、K3。工具卡保留完整的 command 和 description。原生结果存在，半截 JSON 保持 partial；结束帧提供的最终参数只发布一次。 |
| `docs/runtimes/crash-resume.md`、`session.resume-or-loud-rejection` | restart / queue · 恢复有原生证据的回合并续聊 | Kimi 2.7.2-botiverse.1，KAP 实机 | 复制截至真实工具执行中的宿主日志前缀，重新打开，读取原生历史。随后恢复原聊天，用同一排队 UUID 连续两次 drain | K1。第一轮恢复出的消息、工具卡、状态与完整运行一致，恢复期间模型调用计数不变。排队输入只新增一次模型请求，保留原生会话 ID 和队列 UUID。此次故障切点通过日志前缀重建。 |
| `tests/kimi/kimi-kap-controls.test.ts` 的 steer handoff / cancelled false / server interrupt / late reply 断言 | queue / lifecycle · 中止与后续 FIFO | 固定上游运行时 v0.130.1 的 KAP 协议对端，macOS 原生 Rust 控制器，协议实验 | 在父回合和追加回合交接时中止。分别返回取消成功、没有活动回合以及原生主动中断；再测试延迟回执之后的连续插话 | K2。修复前 cancelled=false 时队列等待 5 秒超时。修复后，后续输入各只发一次，原生 POST 的 UUID 等于 queue 返回值。晚到的取消回执不影响下一回合；空闲时再次中止在本地得到拒绝。 |
| `packages/oar/src/shared/abort-fallback.ts`、`tests/kimi/kap-protocol-spawn.test.ts` | lifecycle · 有界中止与启动失败 | KAP 协议对端及 macOS 实际子进程 | 反复中止一个只确认取消、一直不发终态的回合；另测版本探针成功后执行权限消失、子进程退出未给端点、原配置目录立即重开 | K2。首次中止的 10 秒期限生效，重复中止没有延期，每对已确认请求和回执 ID 匹配，观察到控制器退出。启动失败在 4 秒断言内返回，使用同一配置成功重开。 |
| `docs/runtimes/kimi.md`、`session.resume-or-loud-rejection` | restart / interaction_settings · 旧版本恢复及工作目录 | 官方 Kimi 2.7.2，ACP stdio，实机 | 按能力选择 ACP；在新会话添加一个工作目录，在恢复后再提交一轮，核对模型收到的上下文 | K4。两轮均结束，模型在恢复后的请求中同时看到了前后两轮的标记，并在两次请求中看到了指定目录。只允许在创建时设置的操作得到了明确拒绝。 |
| `docs/runtimes/claude.md`、`docs/runtimes/steer-delivery.md` | realtime / restart · 中断时归并 Claude 原生内容 | Claude Code 2.1.330，stream-json，实机 | 在普通回合及包含多条追加指令的回合截断宿主日志，恢复消息、推理、工具及输入归属，再重开一次 | C1。两个客户端重建的内容与原始时间线相同，未知输入没有冒充已接纳；缺少 SDK 终态时仍为 interrupted。包括第一条指令已移到子回合而第二条尚未移走的切点。 |
| `docs/runtimes/pi.md`、`docs/runtimes/crash-resume.md` | restart / queue · Pi 重试中的恢复 | Pi 1.6.0，RPC stdio，实机 | 第一轮模型请求先返回 503；排队后续输入，等待原生 settle，按输入回执重建历史 | P1。先完成原生重试再投递下一条，重建期间不重发原请求。两个 HTTP 客户端都能收到原生历史补全。 |
| 工作台 `apps/server/src/orchestration-v2/ThreadForkService.test.ts`、Kar 清单的 Pi 回合分叉 | fork · 按 Pi 原生回合分叉 | Pi 1.6.0，RPC stdio，实机 | 建立两个带不同标记的父回合，从第一轮分叉并让子会话续聊，检查模型收到的历史、工作区及原生 ID | P1 中的 `fork_at_turn_restores_workspace_and_native_context` 通过。子会话保留所选轮上下文并获得新的原生 ID。清单指定的三轮取中间轮仍独立验收。 |
| `docs/runtimes/factory.md` | restart / queue · 工具与排队输入 | Factory Droid 1.17.2，原生 stdio，实机 | 两个客户端跟踪工具回合，在不同日志切点恢复，再在原会话投递队列 | F1 中的 `native_tool_feed_and_queued_resume` 通过，工具回合和排队输入的恢复断言通过。使用的是 Rust 本地订阅。 |
| `docs/spec/config-form.md`、`docs/runtimes/factory.md` | interaction_settings · 双客户端答复 | Factory Droid 1.17.2，原生 stdio，实机 | 重连到待答表单，提交非法输入，再让两个客户端并发答复同一个请求；另测工具审批和原生设置 | F1 中的 `concurrent_question_answers_reconcile_on_reconnect`、`native_tool_approval_reaches_both_clients`、`native_control_receipts_and_settings` 均通过。并发答复只有一项成功，两端最终待办清空。 |
| `docs/runtimes/factory.md` | inventory / tools · 目录和 MCP | Factory Droid 1.17.2，原生 stdio，实机 | 创建聊天前查询模型及技能；创建、恢复会话并调用配置的 MCP 工具 | F1 中的 `model_and_skill_catalogs_before_session_creation`、`native_mcp_transports_on_create_and_resume` 均通过。 |

## 复验命令

在 Kar 仓库执行以下命令。Kimi KAP 实机使用 Kimi 源码提交 `4ca80ab5cdf3e51391804cb7cce4d0ccae32daff` 构建的 `apps/kimi-code/dist/main.mjs`，Node 24.15.0 加入 PATH。官方 2.7.2 使用对应版本发行包的可执行文件。把下列路径环境变量设置为所述程序的绝对路径。

| 编号 | 所需环境 | 命令 |
| --- | --- | --- |
| K1 | `KAR_KIMI_BIN` 指向 2.7.2-botiverse.1 | `cargo test --locked --test kimi_live kap_tool_turn_replay_and_native_resume -- --ignored --nocapture` |
| K2 | `KAR_UPSTREAM_SOURCE` 指向含固定运行时提交的 Git 仓库；Node 在 PATH | `cargo test --locked --test kimi_controls -- --ignored` |
| K3 | Rust | `cargo test --locked --lib kimi::projection::tests` |
| K4 | `KAR_KIMI_ACP_BIN` 指向官方 2.7.2 | `cargo test --locked --test kimi_live official_cli_falls_back_and_resumes_over_acp -- --ignored` |
| C1 | PATH 中的 `claude` 为 2.1.330 | `cargo test --locked --test native_live claude -- --ignored --test-threads=2` |
| P1 | `KAR_PI_BIN` 指向 Pi 1.6.0 | `cargo test --locked --test native_live pi_history -- --ignored --test-threads=2` |
| F1 | `KAR_FACTORY_BIN` 指向 Droid 1.17.2 | `cargo test --locked --test factory_live -- --ignored --test-threads=2` |

## 回归结果

`cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings` 和 `cargo test --locked` 均通过。Cargo 汇总行共报告 116 项通过（含 3 项文档测试）、137 项默认忽略。多进程聊天与协议测试另有独立测试入口，命令退出码为 0。显式运行 K1/K4 共 2 项、K2 共 4 项、C1 共 7 项、P1 共 4 项、F1 共 6 项，均通过。K2 的 4 项计入协议及本机进程实验。

## ACP 历史恢复与超时

以下行为对应 Kar 提交 `677d69390c7cf2f208296d90dee11be6791a5cab`。上游依据是固定 v0.130.1 的 `tests/acp/acp-open-timeouts.test.ts` 和 `packages/oar/src/shared/acp/profile.ts`。平台为 macOS arm64。

| 验收项 / 上游位置 | 供应商、版本、传输 | 操作与期望 | 命令及实际证据 | 状态 |
| --- | --- | --- | --- | --- |
| restart、lifecycle / release_delta.resume | Kimi、Grok、通用 ACP，按 v0.130.1 实现的 Rust 协议对端及实际 stdio 子进程 | 用可控时钟延迟原生回复。Kimi 新建、轻量恢复、分叉在 60 秒可成功，630 秒超时；Kimi、Grok 的历史载入分别在 45、40 秒可成功，静默至 60 秒失败。认证、工作区查询、模式、模型和推理设置采用各自普通请求时限。 | `cargo test --locked --test acp_deadlines`。31 个场景连续运行 12 次通过。超时返回原生方法及毫秒数，检查 PID 已回收、控制连接 EOF。对历史加载超时在同目录以同一原生 ID 重试，后续输入完成一次且返回预期文本。 | 已验收，协议和本机进程实验 |
| restart、tools / Grok MCP readiness | Grok ACP，按 v0.130.1 实现的 Rust 协议对端 | 第 25 秒答复载入请求，第 30 秒发出本会话 discovery 和宿主 MCP endpoint 已就绪，另一次让 endpoint 保持静默。 | 同上。第一种返回原会话；第二种自开始载入计 60 秒返回 `_x.ai/mcp_initialized (60000 ms)` 并回收进程。自定义载入 45 秒、普通请求 90 秒、会话启动 7 秒分别验证。 | 已验收，协议实验 |
| restart、interaction_settings / Kimi ACP configureSession | 官方 Kimi 2.7.2 ACP，另有 2.7.2-botiverse.1 KAP 对照 | 在隔离目录创建聊天并明确选择原生 default 模式，关闭后以原聊天 ID 重开，再发一轮。 | K4 同时核对两次 `session/set_mode` 的目标会话、模式及成功回执，验证模型看见两轮输入；K1 复验两个独立 SSE 客户端分别补齐到 32 条事件。K1、K4 共 2 项通过。Kar 只在调用方显式选定模式时发该请求，上游 Kimi 会自动选 yolo。 | 已验收上述实机行为，默认模式选择有此差异 |
| realtime、restart、tools / Grok 与 Antigravity ACP 回归 | Grok 1.0.92 (b06756fd6256)、Antigravity 1.5.15，真实程序，模型流量走回环服务 | 检查 Grok 的目录、图像、双客户端插话、中止、宿主工具首轮可用，以及 Antigravity 的工具回合、双客户端恢复。 | 分别设置 `KAR_GROK_BIN`、`KAR_ANTIGRAVITY_BIN` 为上述二进制，运行 `cargo test --locked --test grok_live -- --ignored --test-threads=2`（6 项），`cargo test --locked --test antigravity_live -- --ignored`（1 项）。均通过。 | 已验收上述实机行为 |

该提交通过格式、Clippy 和完整 Cargo 测试门禁。标准汇总 116 项通过、137 项默认忽略；包含 31 个子场景的 `acp_deadlines` 使用独立测试入口。上述模拟时限与供应商程序运行结果分别记账。

## 子任务、技能目录与浏览器 / CLI

以下测试运行在同一台 macOS arm64 主机。源码断言均指固定的上游运行时 v0.130.1。

| 上游契约 / Kar 验收项 | 供应商、版本及传输 / Kar 提交 | 操作及期望 | 复验命令与实际结果 | 状态 |
| --- | --- | --- | --- | --- |
| `tests/claude/claude-session-model.test.ts`、`tests/replay/codex-collaboration.test.ts`、`tests/acp/acp-grok-wire-shapes.test.ts` / agents | Claude stream-json、Codex app-server、Grok ACP 协议对端；`163c29c` | 交错发送根、子、兄弟和孙任务的模型帧，同时混入未上报值及重复帧。模型只在对应的会话或调用路径变动。 | `cargo test --locked --test protocol` 的 `child_models::exercise`。Claude 收到 5 个模型变化，包含根初始化和 3 条不同子路径。Codex 子会话由 selected 改为 later，根模型保持原值。Codex、Grok 的任务列表保留启动时的 initial，兄弟任务的缺失值为 None。 | 已验收，协议实验 |
| `tests/dsh/projection.test.ts` / agents | DSH Web 协议、0.2.2-rc.5 官方原生 Web；`163c29c` | 同一子会话的 descriptor 和后续请求头按字段更新；重复和迟到的 descriptor 不覆盖请求头。另用真实子任务核对源会话 ID 和模型值。 | `cargo test --locked --lib child_descriptor_and_headers`；置 `KAR_DSH_BIN` 后运行 `cargo test --locked --test dsh_live native_session_log_upload -- --ignored --nocapture`。原生子任务上报 deepseek-official/deepseek-flash，路径、原生请求和模型记录对应，父会话查询仍为父模型。 | 已验收，协议和实机分别执行 |
| `sea-trial/vendor/antigravity-skills.vendor.test.ts` 中工作区、插件及登录状态断言 / inventory | Antigravity 1.5.14 与 1.5.15，官方 macOS arm64 程序；`324ad5f` | 未登录查询空目录，再创建工作区技能和插件。用原生 `--print=/skills` 对照 Kar CLI 返回的名字、来源、调用标记、部分目录标记和隐藏项，检查目录查询没有创建聊天。 | 分别设置 `KAR_ANTIGRAVITY_BIN`，运行 `cargo test --locked --test skill_inventory_live antigravity_skills -- --ignored`。两版均通过。1.5.14 下载文件的 SHA-512 与固定源码 `.github/runtime-versions.json` 一致。未登录时对空目录的查询没有创建 .gemini。 | 已验收，两个真实版本 |
| `packages/cli/src/mcp{,-tools}.ts` / coverage、lifecycle | Kar MCP stdio CLI、官方 Kimi 2.7.2 ACP、官方 DSH 0.2.2-rc.5 Web；`0979c85` | 从同一进程查询已提供的两个运行时。分别选 Kimi 和 DSH，向两个独立的回环模型端点发送不同标记；比对返回文本、模型请求及实际 DSH 会话 ID。 | 设置 `KAR_KIMI_ACP_BIN`、`KAR_DSH_BIN`，运行 `cargo test --locked --test dsh_live cli_routes_to_the_selected_native_runtime -- --ignored`。两次调用均完成，仅各自端点看到对应标记。另以 `cargo test --locked --test protocol` 检查 Codex/Claude 协议对端各收到一次、深度为 1，未提供的运行时得到拒绝。 | 已验收，实机及协议分别执行 |
| `tests/pi-durable/projection.test.ts` / tools、lifecycle | Pi Durable 真实 Harness、Chromium / IndexedDB、Kar Rust Wasm；`cd3e19e` | 重新打开 IndexedDB 后让模型调用外层工具，再由原生 Harness 调用子工具。核对原生实收参数和 Rust 输出的两条工具记录，参数含引号、换行和数字。 | 按 README 构建 Wasm 后运行 `node tests/browser/live.mjs`（Node 24，`KAR_BROWSER_DEPS` 和 `PLAYWRIGHT_BROWSERS_PATH` 指向安装目录）。实际 7 次模型调用；外层 root-lookup 与内层 root-lookup/child 各产生一次开始和一次完整参数，内层保留独立值。 | 已验收，真实浏览器及 IndexedDB，模型端点为回环服务 |

DSH 另以 Node 24.15.0 运行 `cargo test --locked --test dsh_live -- --ignored --nocapture --test-threads=2`。当时的 23 个用例全部通过，包括两种传输的提示词、创建 hook、MCP、标题、工具限制与日志上传；Web 的多客户端队列、工具 ID 重用、缺失会话、目标重启、压缩和定向停止后台作业；另覆盖工作区可写目录。对应 `KAR_DSH_BIN` 为 0.2.2-rc.5 的 Web，`KAR_DSH_ACP_BIN` 为 0.2.1-rc.5，`KAR_DSH_ACP_ROOTS_BIN` 为 0.2.2-rc.3。上表的跨供应商 CLI 用例随后单独执行。桌面 app 随附的 Node 首次运行没有载入 DSH 原生模块，换用上述 Node 后通过。DSH 的实机证据以通过的这次运行计。

## Pi 启动凭据

Kar 提交 `663f582`。

| 供应商、版本和平台 | 操作与期望 | 命令与实际结果 |
| --- | --- | --- |
| Pi RPC 1.4.0、1.6.0，macOS arm64，Node 24.15.0 | 每个版本同时打开两个会话，用不同的 HTTP header 访问各自的 MCP，再打开无 MCP 的会话。重启后替换或省略服务。 | 将 `KAR_PI_BIN` 分别设为各 npm 安装的 `node_modules/.bin/pi`，运行 `cargo test --locked --test pi_mcp_live -- --ignored --nocapture`。两次均通过。接收端按 URL 校验独立的随机值；模型分别看见对应工具，空会话的列表为空，恢复保留原生 ID。工作目录、聊天与运行时文件、进程参数和捕获的模型请求中均未找到 header 原值。 |
| 同上 | 恢复时向 stdio 子进程传入包含起始 `!` 和字面 `$HOME` 的独立凭据，分别保留、移除继承的 HOME。 | 同一测试两次均通过。子进程对收到的值运行 `cksum`，与原值的校验和和长度相符。扩展目录只留下原生模块、租约和注册回执；套接字已移除。 |
| Kar Rust 一次性交接，macOS arm64 | 指定错误的接收进程和正确的接收进程，分别读到 EOF 和完整内容；交接后无法再次连接。 | `cargo test --locked --lib pi_mcp` 两项通过。Linux x86_64 与 Windows gnullvm 的 release CLI 交叉检查通过，平台原生执行分别列为待验和本轮排除。 |

## Claude 与 Qoder 的 MCP 配置交接

Kar 提交 `1444fe3`。下列两种 CLI 的工具都由真实供应商进程调用，模型回复分别来自回环服务和供应商自带的离线回放模式。

| 供应商、版本和平台 | 操作与期望 | 命令与实际结果 |
| --- | --- | --- |
| Claude Code 2.1.322、2.1.330，macOS arm64 | 同时创建两个会话。首个提示前检查启动目录已移除。stdio 工具在首次回复后退出；HTTP 工具令用过的 MCP 会话返回 404。原会话再次调用两种工具，另一个会话使用不同的凭据。 | 逐版本设置 `KAR_CLAUDE_BIN`，运行 `cargo test --locked --test protocol -- claude-mcp-native`。两版本各收到五次带独立调用标记和正确凭据摘要的工具结果，含两次 stdio 进程启动、三条独立的 HTTP MCP 会话及实际 404。初始化后进程参数只含已删除的配置路径。模型请求、Kar 记录及隔离 HOME 均未出现原始凭据。 |
| Qoder worker 1.1.78，macOS arm64 | 用原生离线模型循环在同一回合依次调用 stdio、HTTP、stdio、HTTP。检查首轮前配置路径已经移除，工具仍能收到原始凭据。 | 设置 `KAR_QODER_WORKER_BIN`，运行 `cargo test --locked --test protocol -- qoder-mcp-native`。四个工具结果都包含指定标记和预期摘要；进程参数、Kar 记录及隔离 HOME 未出现原值。另以 `cargo test --locked --test qoder_live -- --ignored` 回归 1.1.78 原生恢复和 1.1.83 匿名 daemon 握手，两项通过。 |
| Kar Rust 交接层，macOS arm64 | 验证私有目录和文件的权限、排他创建、保留活跃租约、清理失去宿主的读者、慢读者传输大于管道缓冲区的数据，以及供应商未读取时的超时与清理。 | `cargo test --locked --lib mcp_file` 三项通过。等待中的读者收到 1 MiB 完整数据或在回收后收到 EOF。普通文件后备路径在 macOS 运行；Windows 原生执行仍属本轮排除项。 |

## 三轮检查点与中间回合分叉

Kar 提交 `bc905c7`。测试分开记录文件操作、Git 断言和原生模型上下文。

| 平台、供应商和传输 | 操作与预期 | 命令与证据 |
| --- | --- | --- |
| macOS arm64、Ubuntu 24.04.5 arm64，Kar Rust CLI 与 Git，对端为 Rust Codex 协议夹具 | 连续三轮新增、删除、修改文件，含二进制、符号链接、可执行位、忽略项和已有暂存区。对照三个回合所有路径与变更类型；回滚到第二轮，逐路径比对文件内容、模式和链接目标。 | `cargo test --locked --test chat_restart` 两个平台通过。工作区 HEAD、index 字节不变，第二轮树和第三轮起始树相等；冲突路径阻止覆盖，部分恢复可重试。 |
| Pi RPC 1.4.0、1.6.0，macOS arm64、Ubuntu arm64 | 三轮分别让真实 Pi write 工具写入 first、middle、late。选第二轮分叉，重开控制器后让子会话继续写 child，再重开父会话读上下文。 | 设置 `KAR_PI_BIN`，运行 `cargo test --locked --test native_live three_native_writes -- --ignored`。每轮只有一个新增文件；回滚后 late 文件消失，子回复为 `first/middle/child`，父回复为 `first/middle/late/inspect`。记录的父子原生 ID 不同。 |
| OpenCode 2.2.7 ACP，macOS arm64、Ubuntu arm64 | 通过 Kar CLI 连做三轮、从第二轮分叉新会话，对照模型收到的父消息、附加工作目录工具读取与再次分叉。 | 设置 `KAR_OPENCODE_BIN`，运行 `cargo test --locked --test native_live native_turn_fork_rolls_back_workspace_and_context -- --ignored`。两端通过，第三轮的用户和助手标记都未进入子请求，父回合记录保持原值。 |

## Linux 原生验证

在隔离的 Lima 2.3.0 / Apple VZ 虚拟机运行 Ubuntu 24.04.5 LTS、内核 6.8.0-147-generic（aarch64），Rust 1.100.0、Node 24.15.0。虚拟机使用独立 HOME 和文件系统。以下记录在 Linux 内编译、运行并读取供应商进程结果。

| 提交、供应商和版本 | 操作与命令 | 实际证据 |
| --- | --- | --- |
| `bc905c7`，Kar，Ubuntu arm64 | `cargo test --locked --all-targets -j1` | 118 项由标准 Rust 测试框架计为通过，139 项标有环境条件而跳过。另有独立可执行测试通过，包括跨进程聊天恢复、队列、双订阅与检查点。首次以默认并行度链接时内核终止了链接器；降低并行度并添加虚拟机临时交换空间后通过。 |
| 同提交，Codex 0.169.0 app-server、Claude Code 2.1.326 stream-json | 分别设置 `KAR_CODEX_BIN`、`KAR_CLAUDE_BIN`，运行 `cargo test --locked --test native_live actual_codex_durable_chat_across_processes -- --ignored`、`actual_claude_durable_chat_across_processes` | 两项均通过。独立 Kar CLI 进程新建、按 ID 重开及续聊，校验原生历史、模型请求和持久化回合。 |
| 同提交，Pi RPC 1.4.0、1.6.0，Claude 2.1.326 | 分别执行上面的 `pi_mcp_live`、`claude-mcp-native` | 两个 Pi 版本分别通过凭据隔离、双客户端工具回放与重启；Claude 两种工具在路径删除后再次认证成功。同时运行 `cargo test --locked --test native_live pi_history:: -- --ignored`，Pi 1.6.0 的五项原生历史、重试及分叉测试全部通过。 |
| 同提交，Qwen Code 0.27.4，Grok 1.0.70 ACP，Kimi 官方 2.7.2 ACP | `cargo test --locked --test qwen_live -- --ignored`；Grok 分别运行 `native_image_reaches_the_model`、`host_tool_is_ready_on_the_first_turn`；Kimi 运行 `official_cli_falls_back_and_resumes_over_acp` | Qwen 两项、Grok 两项和 Kimi 一项均通过。分别校验 Qwen 目录、记忆、续聊与中止，Grok 模型实收图像和首轮工具调用，Kimi 官方 CLI 的 ACP 续聊。复验时指定 `KAR_QWEN_BIN`、`KAR_GROK_BIN`、`KAR_KIMI_ACP_BIN`。 |
| `bc905c7` 与 `c6d2341`，DSH Web 0.2.2-rc.5、ACP 0.2.1-rc.5 / 0.2.2-rc.3 | 设置 `KAR_DSH_BIN`、`KAR_DSH_ACP_BIN`、`KAR_DSH_ACP_ROOTS_BIN`、`KAR_KIMI_ACP_BIN`，运行 `cargo test --locked --test dsh_live -- --ignored --test-threads=1` | 首次完整执行 24 项，23 项通过；重复工具 ID 用例碰到供应商的 Landlock stderr，原断言将它一并当作 stdout。修正后单跑该项通过，分别核对两次 stdout，再将含 stderr 的完整工具结果与回放、重启恢复逐项比对。 |
| `0b13edf`，Pi 1.4.0、Claude 2.1.326、Qwen 0.27.4、Factory 1.12.1、DSH Web 0.2.2-rc.5、DSH ACP 0.2.1-rc.5 | 各自运行 `public_prompt_readbacks`（Pi 位于 `native_live`，其余位于对应 `*_live`），Claude 运行 `cargo test --locked --test protocol -- claude-mcp-native` | 通过公共用例 `session.query-readbacks-carry-stream-seq` 的两次普通输入。给定 UUID 原样记录，省略 UUID 时自动生成；逐条重放到模型、用量和上下文各自的 seq，返回值分别一致。 |

macOS 同一公共用例通过 Pi 1.6.0、Claude 2.1.330、Qwen 0.28.4、Factory 1.12.1 / 1.17.2 和上述两种 DSH 传输。真实 Chromium 的 Pi Durable 还用不透明字符串 `lookup` 和自动生成的 UUID 核对输入记录，三项查询值及序号均与快照相符；`node tests/browser/live.mjs` 的整套模型请求数为 8。Rust 协议用例另在 Codex、Claude、Qwen、ACP 和 Pi 五种传输执行。

## 固定源码索引与逐项证据

[源码索引](backend-source-index.tsv) 固定到上游运行时 v0.130.1 的 `2b15859a13dcc50e8b66cbaee5a007832aa00e08`。通过 TypeScript 语法树与包导出表提取了 40 个契约文件、221 个顶层契约声明、481 个具名接口成员、849 个按入口区分的导出、12 条 CLI 命令声明、32 个选项、2 个显式参数和 4 个 inventory 选择。20 个公共用例与 suite 注册表一致。149 个供应商测试文件内有 381 个测试定义，另有 53 组文件级共享断言。供应商部分索引了 2621 处直接断言或断言辅助函数调用。参数化测试在运行时可产生多个实例；类型别名和共享辅助函数须结合引用处逐项验收。

[逐项证据](backend-evidence.tsv) 用固定源码路径和行号关联实际运行。每行给出供应商、版本、平台、操作、期望、Kar 提交、命令和观察结果。索引中的“待逐项核销”表示此表尚未登记对应证据。此前按场景执行的结果仍见上文。

- **安装与额度。** 在 25 组 macOS、16 组 Ubuntu 的真实版本/程序上指定 `--bin` 查询安装，41 组返回 available，文件系统确认 command 指向指定的可执行文件。按能力从正常 CLI 入口查询空配置账号额度，保留每组的响应类型和原生原因。复验时使用独立的空 HOME、USERPROFILE 和 XDG 目录，清除供应商凭据变量，把对应版本程序的绝对路径传给 BIN、空目录传给 WORK，并把其 Node 解释器加入 PATH。有效付费账号的额度窗口另行核销。
- **实时、回放与三轮会话。** `c747118` 在 macOS / Ubuntu 各跑了六组。Pi、Claude、Qwen、Factory 及 DSH Web / ACP 均连续完成三轮。一个订阅任务在提示出现时故意 panic，另一个订阅逐条收到日志中的完整记录。从第一轮提示后重新订阅，补齐该轮后继续接收后两轮，到会话关闭时逐条核对游标和内容，两端均无多余记录。修复前 Claude、Pi、DSH 的实机以及 Codex 协议用例均观测到 fork_anchor 越过 turn_ended。修复后回合结束是该轮的最后事件，原生包仍完整保留。设定所测版本的 `KAR_PI_BIN`、`KAR_CLAUDE_BIN`、`KAR_QWEN_BIN` 或 `KAR_FACTORY_BIN`；DSH 用例同时需要 `KAR_DSH_BIN` 和 `KAR_DSH_ACP_BIN`。
- **私有配置与能力声明。** `12690fd` 验证真实 Claude 在 POSIX FIFO 的读端已打开后等待一秒，写入配置之后才完成初始化，配置路径删除后仍能调用该工具。macOS 的 2.1.322、2.1.326、2.1.330 与 Ubuntu 的 2.1.326 均通过。`1fee586` 对 Factory 1.12.1 与 1.17.2 逐项验证 stdio、HTTP 实际调用和能力声明。macOS 两个版本各运行 `cargo test --locked --test factory_live -- --ignored` 全部七项通过，Ubuntu 1.12.1 同样通过七项。1.12.1 的实机 AskUser 帧缺少较新版本提供的上下文字段，Kar 保留字段缺失，两个版本均通过各自的表单断言。

## 强制结束真实 Pi 控制器后的恢复

在 macOS 27.2.1 arm64 和 Ubuntu 24.04.5 arm64 上分别使用 Pi 1.4.0、1.6.0，四组均从正常 Kar CLI 强制结束进程。此前已完成两个回合，包括一次原生写文件。第一条排队消息到达模型且 Kar 已持久记录为 sent 后，将两条新消息加入队列，保持模型请求未完成，强杀 Kar。新控制器打开同一个工作区与原生会话，恢复后重复提交相同 UUID 并再次 drain。

四组测试里，模型都按原顺序收到三个不同标记，各一次。先后从不同游标断线的两个 HTTP 客户端，在 SSE 服务重启后各自重建了全部 48 条事件，内容与持久化日志逐条相同。测试还直接读取 Pi 原生会话文件，按被中断输入之后的助手 stopReason 校验宿主终态。此实验覆盖已记录发送和两条未发送的切点；供应商收到输入到本地写入回执之间的独立切点仍需另验。

在提交 `ed1e637`，设置 `KAR_PI_BIN` 后运行 `cargo test --locked --test native_live pi_history:: -- --ignored --nocapture`，四组各通过全部 7 项。macOS 的 `cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked` 通过。

## 原生接收与 Kar 回执写盘之间的恢复

以下运行均使用真实供应商程序、隔离配置和本机模型服务，分别在 macOS 27.2.1 arm64 与 Ubuntu 24.04.5 aarch64 执行。

| 供应商、版本、提交 | 操作及复验命令 | 观察结果 |
| --- | --- | --- |
| DSH Web 0.2.2-rc.5，`fe7c6d0` | 设置 `KAR_DSH_BIN`，运行 `cargo test --locked --test dsh_live queue_crash:: -- --ignored --nocapture`。排队三个不同 UUID。锁住 Kar 日志，等待 DSH 原生日志出现第一条的 UUID 且模型已收到请求，分别在原生回复完成和模型仍等待时强杀控制器。 | 每个平台两项均通过。强杀前 Kar 日志字节保持相同，重开前队列为 uncertain、pending、pending。重开原工作区与原生会话后，第一条由 DSH 原生日志补回回执，后两条依序各发送一次。重复三个 UUID 并再次清空队列后，模型请求和原生输入均只含种子输入及最初的三个 UUID。DSH 对运行中被强杀的输入给出 interrupted 原因，Kar 保存为 RuntimeExited 的失败回合。 |
| Kimi KAP 2.7.2-botiverse.1，`f3e4587` | 设置 `KAR_KIMI_BIN`，运行 `cargo test --locked --test kimi_live kap_tool_turn_replay -- --ignored --nocapture`。把 Kar 日志重建到 Begin，分别保留原 UUID 或换成新 UUID，两组使用相同文本和真实 KAP 会话历史。 | 每个平台的匹配组都恢复了原生回合 ID、已发送状态、完整回复和工具结果。异 UUID 组保留 interrupted 和未确认状态。两组均未新增模型请求。各平台两个 SSE 客户端分别从第 17 和第 5 条事件断线，最终都收到全部 32 条。此项采用日志前缀重建，未在此切点强杀 Kimi。 |

## 关闭请求和回执

提交 `a0bda94` 核对了上游运行时 的 `session.dispose-is-recorded` 和 `session.dispose-mid-turn-settles`。修复前，ACP 协议回归在查找 dispose 请求时失败。修复后，显式关闭记录请求 UUID，观察到进程退出后记录同一 UUID 的回执及操作系统退出码。信号终止的退出码为 null。自发退出不会生成关闭请求。

| 平台与程序 | 操作与实际证据 |
| --- | --- |
| macOS 27.2.1 arm64、Ubuntu 24.04.5 aarch64 | 七组供应商或传输各执行两种关闭。Pi RPC 在 macOS 为 1.6.0，Ubuntu 为 1.4.0。两端的 Claude Code 2.1.326、Qwen Code 0.27.4、Factory Droid 1.12.1、DSH Web 0.2.2-rc.5、DSH ACP 0.2.1-rc.5 和 Kimi KAP 2.7.2-botiverse.1 版本相同。慢提示提交后并发关闭，在 15 秒上限内两个调用都返回，按请求 UUID 只有一个退出回执。原会话拒绝新提示。Kimi 在模型已收到请求且仍在等待时执行关闭。逐版本命令见证据表。 |
| Morph 0.3.128，两端 | 在已经运行的 Console 上断开外部连接及共享租约，回执均为 accepted。剩余客户端仍可调用工具。最后一个持有者关闭后，Kar 取得进程退出状态并给出 exited 回执，健康检查确认进程已停止。Ubuntu 使用官方 Linux arm64 发布程序并核验了校验和。 |
| Pi Durable 1.6.0，macOS 上的 Chromium、IndexedDB 与 Kar Wasm | 慢回合中并发关闭两次，检查请求和唯一 accepted 回执及 released 事件。相同宿主重新挂接后，原任务仍成功完成。复验按 README 构建 Wasm 后运行 `node tests/browser/live.mjs`。 |
| Cybion，macOS 与 Ubuntu 上的协议测试 | 在关闭宽限期内完成的原生回合，先记录终态再应答关闭。远端仍运行时，控制器在三秒后释放，期间排队的提示没有提交。四项协议测试通过。实际部署的 URL 和凭据待提供。 |

macOS 的格式检查、全目标 Clippy 和默认 Rust 测试套件通过。Ubuntu 的原生 `protocol` 和 `cybion` 回归通过。Linux x86_64 和 Windows x86_64 的 release 检查通过编译。打开会话时的执行位置声明及前两条记录、普通输入接纳回执的完整关联仍待核销。

## 普通输入、繁忙拒绝与 Pi 原生回执

提交 `875c707` 给每次普通输入尝试分配请求 ID。慢回合期间重复两次同一输入 UUID，两个拒绝回执各自对应一个请求，原回合继续运行。macOS 与 Ubuntu 上的 Pi、Qwen 和 Factory 均完成原回合、回到空闲并成功接受下一条输入。两端的 Pi、Factory、Claude 和 DSH Web 还验证了已空闲时调用 `steer` 返回拒绝。测试使用真实供应商进程与本地模型服务。

| 操作 | 结果及复验 |
| --- | --- |
| Pi 扩展处理输入 | 使用发行包自带的 `examples/extensions/input-transform.ts`，设置 `KAR_PI_BIN` 和 `KAR_PI_INPUT_EXTENSION`。连续发送两次 `ping`，模型收到零个请求，两个等待都凭各自的 handled 回执结束，没有模型开始或结束事件。随后发送普通输入可正常结束。 |
| Pi 重启后首条遭拒、后续输入由扩展接收 | 建立有原生历史的会话后关闭。移除隔离的测试模型凭据，依次排队普通输入和 `ping`，再打开原工作区与会话。第一条保留缺少凭据的原文与 refused 状态，第二条为 handled。恢复测试凭据，再次入队原来的两个 UUID 并重启，模型调用次数保持不变。新 UUID 可以继续聊。 |
| Pi 强杀与双客户端续收 | macOS arm64 的 1.6.0、Ubuntu arm64 的 1.4.0 和 1.6.0 分别运行 `cargo test --locked --test native_live pi_history:: -- --ignored --nocapture`，每组 9 项通过。强杀后两端均逐条核对了全部 53 条持久事件，模型观察到三个队列输入各一次且顺序一致。 |
| 浏览器 | macOS Chromium、Pi Durable 1.6.0 和 Rust Wasm 验证重复输入复用原生回执，每次尝试可独立关联。构建 Wasm 后执行 `node tests/browser/live.mjs`，会话探针输出 29 条记录，整个测试中模型共被调用 8 次。 |

Pi 上游 handled 系列还包含预先取消的调用者、扩展独立启动模型及压缩与排队竞争等断言，本表所列测试仅覆盖相应输入和重启路径。公共用例的完整打开记录和执行位置也仍待核销。每项对应的版本、命令与限制见 `backend-evidence.tsv`。

## Pi 原生接纳检查期间排队

提交 `e8d223c` 在 Pi 已进入扩展的输入检查、尚未答复 A 时提交 B。固定上游运行时 的测试扩展在隔离目录移除 A、B 的模型凭据，处理 C 时恢复。修复前，原生程序已拒绝 A、B，Kar 发布的队列丢弃事件数为 0。修复后，A 有对应的 `runtime_refused` 拒绝回执，B 有且仅有一次带自己输入 ID 的丢弃事件，两次都保留 Pi 的缺少凭据错误。此期间没有模型请求和模型回合。空闲时中止返回 `no_active_turn`，C 完成且模型只收到一次请求。

macOS arm64 的 Pi 1.6.0 和 Ubuntu arm64 的 Pi 1.4.0、1.6.0 均通过。设置 `KAR_PI_BIN` 和 `KAR_PI_PREFLIGHT_FIXTURE` 后，执行 `cargo test --locked --test native_live admission_holds -- --ignored --nocapture`。后一个变量指向固定上游运行时 源码中的 `sea-trial/vendor/support/pi-preflight.ts`，测试核验文件摘要，并使用其中的原生 Pi 扩展。完整运行 `pi_history::` 还需要前述发行包自带的 `KAR_PI_INPUT_EXTENSION`。三组各通过 10 项。相同提交还复验了两个平台上的 Pi、Factory、Claude 和 DSH Web 在空闲时拒绝插话，每次都有一对可从日志关联的请求与回执。

## Kar 录制导入与回放

提交 `8e9d19c` 修正 `6c07423` 放宽文件头检查的问题。Kar v3 录制使用 `kar-voyage/3`。在 macOS arm64 和 Ubuntu arm64，使用实际 Kar CLI 从三条输入记录生成 JSONL，完整回放三条，从第一条之后的游标回放两条。已有目标得到保护。旧 v3 文件头在发布目标前返回格式错误，目标目录没有输出或临时文件。断序输入同样被拒绝。运行 `cargo test --locked --test kar_voyage`（2 项）及 `cargo test --locked --lib voyage::`（2 项）可复验。

同一提交在两个平台运行了 `cargo test --locked --test chat_restart` 和 `cargo test --locked --test protocol`，覆盖 Kar 自有聊天的导入、恢复、实时回放与断线游标，以及 `kar run --record` 的常规录制。聊天续接命令统一为 `kar chat resume`。固定源码测试夹具改用 `KAR_UPSTREAM_SOURCE`（Kimi）和 `KAR_PI_PREFLIGHT_FIXTURE`（Pi）。分别运行 `cargo test --locked --test kimi_controls -- --ignored --test-threads=1`（4 项协议测试）及 `cargo test --locked --test native_live admission_holds -- --ignored`（1 项原生程序测试）。文中的复验命令使用当前变量名。

## Pi 扩展自己发起的模型回合

提交 `7ea1163` 在 macOS arm64 的 Pi 1.6.0 和 Ubuntu arm64 的 Pi 1.4.0、1.6.0 上分别通过 6 项真实进程测试。模型使用本机 HTTP 服务。设置 `KAR_PI_BIN` 后执行 `cargo test --locked --test native_live extension_runs:: -- --ignored --nocapture`。

- 命令先返回时，宿主收到 handled 回执，独立模型仍然运行。模型先结束时，其 completed 终态先写入，挂起的命令仍保持繁忙直到处理器返回。两种顺序下，模型回复都保留在独立活动中。控制器接受在命令预检期间发起的中止，原生模型一启动就收到该请求，最后只有一个 aborted 终态。
- A 结束时扩展自行启动模型，已排队的 B 在原生预检期间遭拒。B 带有自己 UUID 的丢弃回执，输入钩子只观察到一次 B。独立模型正常结算，会话继续接受输入。
- 双 HTTP 客户端分别保存游标，独立活动和排队输入完成后重开会话再重连。每端逐条比对了全部 32 条持久事件。另在同一流程的模型挂起位置强杀 Kar，观察端只将独立活动标为 interrupted，原命令维持 handled。恢复原生会话后，队列继续发送下一条，重复两个 UUID 均取回既有回执。

## 适配器排队与撤回

提交 `87b1637` 验证了公共用例 `session.queue-runs-after-turn` 和 `session.withdraw-before-dispatch`。慢回合中排队的输入先收到与该请求对应的 accepted 回执。原回合结束后才派发排队输入。撤回一条尚未派发的输入有独立回执，原排队请求与回执可重读；再次撤回和撤回未知 UUID 都会返回 `not_queued`。结束原回合后再观察 1.5 秒，已撤回输入没有进入模型。

复验使用 `cargo test --locked --test <target> public_adapter_queue_and_withdraw -- --ignored --nocapture`。在 macOS arm64 和 Ubuntu arm64 上分别运行了 Pi RPC（`native_live`）、Factory Droid 1.12.1（`factory_live`）和 Qwen Code 0.27.4（`qwen_live`）。Pi 在 macOS 验证 1.6.0，在 Ubuntu 验证 1.4.0 和 1.6.0。每组均使用真实供应商进程和本地模型服务，版本与实际断言见证据表。

Pi 还验证了 A、HANDLE、C 三条连续输入。模型处理 A 和 C，扩展单独处理 HANDLE，回放中的三条输入各显示一次。另一个抛出异常的扩展命令留下原生错误帧与 handled 回执，模型调用数为零。三个 Pi 平台与版本组合完整运行 `pi_history::` 各通过 19 项。此次验收使用 Rust 原始会话流和持久聊天流各自的公开接口。

## 运行中插话与迟到的回执

提交 `a55d912` 保留每次插话的输入 UUID、请求标识和宿主决定。Pi 扩展消费插话后，原生回执关联到具体请求，正在运行的模型保留原回合及其输入标识。同一输入 UUID 的两次显式调用各有自己的请求标识。在真实扩展中延迟回执，曾复现原回合结束后日志报错、下一轮无法发送。修复后迟到的回执写回原回合，下一轮正常完成。

在 macOS arm64 和 Ubuntu arm64 分别运行 Pi 1.4.0、1.6.0，四组各通过 21 项聊天测试。两个 HTTP 客户端订阅扩展独立发起的模型活动，其中一端在插话前断线。重开会话并续收后，两端各自重建的 34 条事件逐条匹配持久日志。另在原回合已经完成、插话仍待确认时复制日志前缀。独立恢复这份副本只将插话标记为未确认，保留了原回合的完成状态。原生程序使用本机模型服务，前缀恢复和强杀进程的测试分别记账。

`session.steer-mid-turn` 的实测还包括两平台的 Factory Droid 1.12.1 和 Kimi KAP 2.7.2-botiverse.1，以及 macOS 的 Factory Droid 1.17.2。Factory 接受插话后按请求中止原回合，Kimi 接受插话后完成原回合并继续投递队列。Rust 协议对端另行测试了错误的命令、外形相似的 RPC 编号、原生拒绝和缺失成功标记的回执。具体版本、平台与命令见 [逐项证据](backend-evidence.tsv)。

## 中止回合、重复中止与浏览器恢复

提交 `1807d7e` 为每次中止分配独立请求 ID，记录接受或拒绝的回执。macOS arm64 和 Ubuntu arm64 对 Codex 0.169.0、Claude Code 2.1.326、Pi RPC 1.4.0 / 1.6.0、Qwen Code 0.27.4、Factory Droid 1.12.1、DSH Web 0.2.2-rc.5、DSH ACP 0.2.1-rc.5 和 Kimi KAP 2.7.2-botiverse.1 都运行了真实程序并接入回环模型。macOS 还验证了 Factory 1.17.2。每组均核对了活动中止的接受回执、原输入的单一终态，以及空闲后再次中止得到的 `no_active_turn`。Codex 子会话同时到达的完成事件按原生会话 ID 区分。Kimi 用独立提示标记等待目标模型请求，排除了后台请求先完成造成的测试误判，中止后可继续发送新消息。逐版本命令见[证据表](backend-evidence.tsv)。

Pi Durable 1.6.0 在两平台均运行了真实 Chromium、IndexedDB 和官方 Harness。模型响应保持挂起期间，旧观察者可以释放，新观察者接入相同会话后看见原生运行，并能中止它。重试已完成输入时只返回之前的提交结果，另一个活动输入继续运行到放行。五个专有测试位置覆盖慢回合中的排队与撤回、已完成输入的重试、接管未输出的运行、新回合中的旧输入重试、以及接管时拒绝改动设置。两个平台各运行 `node tests/browser/live.mjs` 一次用于最终复验，每次观察到 12 个模型请求，重试文字的调用次数为零。

Morph 与 Cybion 另以 Rust 测试对端核对控制回执。Pi RPC 的协议测试注入编号类型错误、非规范编号及命令不符的回执，随后原生拒绝仍只归入原请求。Codex 和 Claude 在原生进程完全不回应中止时的限时退出，以及专有用例要求的退出记录与结果回放，仍待逐项验收。

## Factory 在投递回执写盘前强杀后的恢复

提交 `661854c` 在原生程序接收 UUID、Kar 仅持久化 Begin 时分别测试两次强杀。第一次模型仍在等待。第二次原生程序已经写出该回合的完成记录。修复前第一次重开后漏记了投递回执。修复后，同一工作区和会话中均按原 UUID 确认已发送，第一种回合显示 interrupted，第二种显示 completed。之后两个排队输入依次完成。模型一共收到四次请求，依次是种子消息和三个排队输入；重试全部 UUID 后没有新的模型请求。两个从不同游标断线的 HTTP 客户端分别在两种情况下逐条匹配了全部 32 条及 31 条持久事件。

在 macOS arm64（Factory 1.12.1、1.17.2）和 Ubuntu arm64（Factory 1.12.1），指定 `KAR_FACTORY_BIN` 后运行 `cargo test --locked --test factory_live -- --ignored --test-threads=1 --nocapture`，三组各 10 项通过。早先 macOS 1.12.1 的一次并行运行有一项进程资源读数未返回，该项单独重跑及整套串行重跑通过。另以 `cargo test --locked --test chat_restart` 验证持有共享日志读锁期间第二个 HTTP 客户端仍可读取活动回合快照。此处验证有可匹配原生 UUID 的两个切点。供应商未保存收件凭据的情形仍需按运行时核验。

## 尚未核销的范围

- **未完成或失败。** 对固定提交逐一读取了公共 suite 注册的 8 个模块，提取出的 20 个不同用例 ID 与清单完全相同。覆盖核销仍须对 20 个用例逐供应商记录结果，检查 40 个契约目录条目、公开入口、CLI 和 149 个 `.vendor.test.ts` 文件中的适用断言。各文件中的参数化断言可能适用于多种供应商。
- **未完成或失败。** 实时推送、重启和队列已有上述平台证据。其余供应商、其余强制终止切点、其他适配器的检查点及分叉、并行工作树、原生目标、其余供应商的子任务归属、用量、其他供应商的凭据传递以及未覆盖的运行时专有目录仍须按清单各自核销。
- **供应商不支持。** 本批次没有把未实测的功能登记为供应商不支持。
- **缺环境待验证。** 逐表标注 macOS arm64 与 Ubuntu arm64 的实机结果。Linux x86_64 仍只有交叉检查记录，未在该架构执行的供应商用例继续单列。
- **本轮排除：Windows 实机验证。** 此排除项沿用固定清单。

## Qwen 与 OpenCode 在原生收件后丢失 Kar 回执

所有场景运行真实供应商进程，模型响应来自隔离的本机 HTTP 服务。每种版本及平台的操作、预期、命令和实际结果分别列入 `backend-evidence.tsv`。

| 供应商和 Kar 提交 | 平台与实测 | 恢复结果 |
| --- | --- | --- |
| Qwen Code · `9163958` | macOS arm64 的 0.27.4、0.28.4，Ubuntu arm64 的 0.27.4。每组 `qwen_live` 8/8 | 在原生收到输入、原生写完回答和 Kar 写入回执后分别强杀。前两个断点保留待确认，第三个断点保留已发。恢复到原工作区和原生会话，两条后续输入各发送一次。两客户端从各自的游标补收，再实时收到余下的输入。逐个断点和原生 UUID 重试证据见 [Qwen 恢复实验](backend-qwen-recovery.md)。 |
| OpenCode · `5c30b90` | macOS arm64 和 Ubuntu arm64 各跑 1.19.5、2.2.7。四组均通过原生已收件、Kar 回执未写入的强杀场景 | 独立查询原生历史，首条输入的原生消息 ID 有别于 Kar 提交的 UUID，恢复后保留待确认。队列继续处理其余两条。原生历史与模型请求均按预期顺序且各一次。两客户端分别重建了 v1 的 32 条和 v2 的 36 条持久事件，含此前的文件读取结果。 |

Qwen 直接重投同一个 UUID 会产生两个模型请求，且原生日志生成不同的用户记录 UUID。这项原生幂等能力的限制另行登记。上述测试保留了 Kar 已发送但回执丢失的证据边界。
