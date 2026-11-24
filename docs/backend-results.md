# Kar 后端验收记录

固定范围见 [backend-acceptance.toml](backend-acceptance.toml)。以下记录对应 2026-11-24 的实际运行。每行的状态只覆盖该行列出的供应商、版本、平台和操作。测试名称是 Cargo 可接受的过滤器。

运行平台为 macOS 27.2.1 arm64，Rust 与 Cargo 版本均为 1.100.0。下表的 Kar 可执行行为对应提交 `f3f7e5ec653afe3c866dd3c3d2e384825d752f79`，格式门禁对应 `dc68531c73f125149bbc933718f7b5c771333575`。真实供应商程序的模型流量由本机 HTTP 模型服务响应。输入 UUID、工具结果、模型请求次数及重连游标均由测试读取。

## 已验收的具体行为

| 上游位置 / 公共用例 | Kar 验收编号与范围 | 供应商、版本、传输及证据类型 | 操作与期望 | Kar 测试与实际证据 |
| --- | --- | --- | --- | --- |
| `docs/spec/session-graph-and-cursor.md`、`session.cursor-replays-without-loss-or-duplication` 对应的聊天事件行为 | realtime · 两个独立 HTTP 客户端 | Kimi 2.7.2-botiverse.1，KAP HTTP/WebSocket，实机 | 两端订阅同一聊天，分别在工具参数仍不完整和第一轮结束时断开；停止 SSE 服务，关闭并重开原生会话，另一个聊天也产生事件。两端以各自的 Last-Event-ID 重连 | K1。本次断线前分别收到 17、5 条，续收后各 32 条。两端补齐后的事件与磁盘日志在游标、前驱、内容、顺序和数量上逐项相等。重复应用同一事件返回 false。最终两轮时间线相同；另一个真实聊天的游标返回 HTTP 400。 |
| `tests/kimi/kimi-kap-projection.test.ts` 的三个工具输入断言 | tools · 流式参数与最终输入 | Kimi 2.7.2-botiverse.1，KAP 实机；另有 Rust 协议投影测试 | 分两段返回 Bash 参数。在工具运行期间观察已收到的文本；原生结构化参数到达后清除 partial 标记。重复快照、缺少 updated、ended 重放均须归入同一个调用 | K1、K3。工具卡保留完整的 command 和 description。原生结果存在，半截 JSON 保持 partial；结束帧提供的最终参数只发布一次。 |
| `docs/runtimes/crash-resume.md`、`session.resume-or-loud-rejection` | restart / queue · 恢复有原生证据的回合并续聊 | Kimi 2.7.2-botiverse.1，KAP 实机 | 复制截至真实工具执行中的宿主日志前缀，重新打开，读取原生历史。随后恢复原聊天，用同一排队 UUID 连续两次 drain | K1。第一轮恢复出的消息、工具卡、状态与完整运行一致，恢复期间模型调用计数不变。排队输入只新增一次模型请求，保留原生会话 ID 和队列 UUID。此次故障切点通过日志前缀重建。 |
| `tests/kimi/kimi-kap-controls.test.ts` 的 steer handoff / cancelled false / server interrupt / late reply 断言 | queue / lifecycle · 中止与后续 FIFO | 固定 OAR v0.130.1 的 KAP 协议对端，macOS 原生 Rust 控制器，协议实验 | 在父回合和追加回合交接时中止。分别返回取消成功、没有活动回合以及原生主动中断；再测试延迟回执之后的连续插话 | K2。修复前 cancelled=false 时队列等待 5 秒超时。修复后，后续输入各只发一次，原生 POST 的 UUID 等于 queue 返回值。晚到的取消回执不影响下一回合；空闲时再次中止在本地得到拒绝。 |
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
| K2 | `KAR_OAR_SOURCE` 指向含固定运行时提交的 Git 仓库；Node 在 PATH | `cargo test --locked --test kimi_controls -- --ignored` |
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

## 尚未核销的范围

- **未完成或失败。** 对固定提交逐一读取了公共 suite 注册的 8 个模块，提取出的 20 个不同用例 ID 与清单完全相同。覆盖核销仍须对 20 个用例逐供应商记录结果，检查 40 个契约目录条目、公开入口、CLI 和 149 个 `.vendor.test.ts` 文件中的适用断言。各文件中的参数化断言可能适用于多种供应商。
- **未完成或失败。** 实时推送、重启和队列已有上述平台证据。其余供应商、完整强制终止切点、三轮检查点及分叉、并行工作树、原生目标、子任务模型归属、用量、凭据传递以及运行时专有目录仍须按清单各自核销。
- **供应商不支持。** 本批次没有把未实测的功能登记为供应商不支持。
- **缺环境待验证。** 表中实机结果来自 macOS arm64。Linux 原生运行仍须提供可执行这些供应商程序的 Linux 环境及对应架构安装包，设置同表环境变量后逐项复跑。交叉编译只验证编译。
- **本轮排除：Windows 实机验证。** 此排除项沿用固定清单。
