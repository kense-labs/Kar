# 同一聊天跨供应商转交

工作台行为对照提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。对照位置为 `ProviderSwitchService.ts`、`ContextHandoffService.ts`、`ProviderSwitch.integration.test.ts` 和 `packages/provider-core/src/server/handoffBudget.ts`。对应工作台验收项为 `handoff`。

## 调用和持久化

`POST /projects/{project}/chats/{chat}/handoffs` 接收调用方生成的 UUID `id`、目标 `runtime`、可选 `model` 和 `budget_bytes`。预算覆盖 UTF-8 历史正文，默认 65536，允许 512 至 1048576。返回值包括源和目标原生会话、包含首尾的回合范围、历史正文、未纳入条数及状态。`GET` 同路径可读取操作和首条输入归属。继续使用原路径 `POST .../messages` 发消息。

工作台先持久化 prepared，再启动或恢复目标。Ready 与新的原生 ID 一起写入同一聊天日志。恢复 prepared 时继续同一操作。目标拒绝启动时写入 failed，保留原绑定及错误。重试同一操作 UUID 返回其结果。准备期间控制器死亡可能留下一个未收到任何输入的空供应商会话，重试可打开另一个空会话。独立于此，在 ready 后首条用户输入接纳前，只有对该空目标原生 ID 的明确不存在错误允许透明替换，替换的 ID 进入历史。

切到新目标时，从最新回合反向挑选预算内的完整消息、工具结果、文件变更和错误，再按原时序组装。切回已用目标时，从目标上次有投递证据的回合之后计算增量。首条新消息的原生输入携带该历史，控制器以原始用户消息和 UUID 提前写入接纳记录。历史工具是文本证据，Kar 不执行其中的操作。供应商若已接收输入而本地未收到回执，沿用该供应商的原生恢复和投递证据语义。

## macOS 统一复验

| 模式 | 操作和期望 | 命令 | 当前状态 |
| --- | --- | --- | --- |
| Rust 编译 | 编译全部目标、HTTP 验收程序及原生 stdio 转发器 | `cargo check --locked --all-targets` | 在该分支 macOS 编译通过 |
| Codex 和 Qwen 协议夹具 | A 两个事实和工具结果传至 B，真实 HTTP 服务在 prepared 与 ready 两个切点重启。检查 B 的原生请求、回复、同 ID 重试和可见时间线，再切回 A 并核验增量。测试也覆盖活动回合、目标拒绝和 512 字节预算 | `cargo test --locked --test workbench_handoff` | 已编译，执行留待统一验收 |
| 已安装的 Codex 和 Qwen | A 在隔离工作区以工具读出随机文件，B 应凭交接提供两个随机事实和文件内容。重启后 A 应在原生会话内回答 B 的新事实。Rust 转发器检查实际送入 CLI 的用户包和次数 | `KAR_HANDOFF_LIVE=1 KAR_CODEX_BIN=/absolute/path/to/codex KAR_QWEN_BIN=/absolute/path/to/qwen cargo test --locked --test workbench_handoff` | 待统一实测。需两个可执行入口、它们各自可访问的模型和认证、A 的只读工具权限。运行时应记录两个可执行入口的实际版本 |

协议夹具写入各自临时工作区和聊天目录。实机模式沿用调用进程的供应商配置，建议统一验收提供独立的供应商配置目录。测试按随机输入验证回复，省去凭据、模型流和真实聊天记录的复制。其它供应商组合需要按其各自的恢复与身份确认能力补验。Linux 和 Windows 未在此轮运行。

## 在旧供应商的历史回合创建分叉

`POST /projects/{project}/chats/{chat}/forks` 可以选中供应商切换前的回合。Kar 用持久化切换记录中源绑定的原生身份、启动程序、模型、设置和切换时的回合数确认该回合的归属，并核对时间线根会话和检查点。继承自早期分叉的回合会沿原聊天记录逐层核对。原始记录缺失、身份不一致、没有本地可验证原生锚或供应商不支持时，会在修改工作区前拒绝。新子聊天保存选中回合的原生源会话和源工作目录。

父聊天保留当前供应商、模型、原生身份、切换记录与全部回合。原地分叉按既有行为恢复同一工作区的文件；独立工作树入口可保留父工作区当前文件。切换后的父进程私有环境、MCP 与启动参数不会自动传给早期供应商，可重放的启动设置来自选中回合。

| 模式 | 场景与命令 | 状态 |
| --- | --- | --- |
| 跨进程 HTTP 协议夹具 | `cargo test --locked --test workbench_handoff`；Codex 完成两轮后切到 Qwen，在 Qwen 上续聊后直接选择第一轮，核对 Codex 原生 fork 的父 ID 与包含式 lastTurnId、仅继承该回合、父聊天仍属 Qwen、子聊天独立续聊。切回 Codex 后再选缺少原生按回合分叉能力的 Qwen 轮次，应明确拒绝。 | macOS 测试目标已编译，待联合运行。 |
| 原生程序与真实模型 | `KAR_HANDOFF_LIVE=1 KAR_HANDOFF_FORK_LIVE=1 KAR_CODEX_BIN=/absolute/path/to/codex KAR_QWEN_BIN=/absolute/path/to/qwen cargo test --locked --test workbench_handoff`；需 Codex ≥0.143.0、可用 Qwen、两个已认证模型、临时工作区的只读工具权限。子模型应能复述所选 Codex 历史中的两个随机事实，无法返回后来只在 Qwen 出现的随机事实。 | 实机回复与模型上下文待联合验收。 |
