# Qwen 固定源码对照与待验收项

上游运行时截止提交为 `2b15859a13dcc50e8b66cbaee5a007832aa00e08`。下列改动先按源码实现，完成后的实机结果以统一验收为准。

## 推理强度

对照 `sea-trial/cases/session-effort.ts` 第 43、57 行，以及 Qwen 的 `list-models.ts`、`session.ts`。Kar 保留原生模型目录字段，同时向客户端提供 `displayName`、`effortLevels` 和 `defaultEffort`。开启或恢复会话时，Kar 核对原生 initialize 回执的实际档位、应用状态和覆盖状态，并核对模型目录。没有显式指定模型时，可查询原生上下文中的当前模型名。

| 场景 | 实现前观察与当前状态 |
| --- | --- |
| 明确拒绝不存在的档位 | Qwen 0.27.4 的原生错误仅列出所支持的名称。Kar 在这个特定原生错误上补入调用方的档位名称。节奏调整前，macOS 27.2.1 arm64 的 0.27.4、0.28.4 与 Ubuntu 24.04.5 aarch64 的 0.27.4 均通过无效档位用例，模型请求数为零 |
| 两个原生目录档位，新开、恢复 | 在隔离配置中，Qwen 原生目录暴露 `gpt-5.1` 的 `low/medium/high`，默认 `medium`。前置观察中，首次请求和恢复后的请求分别带 `reasoning_effort=low` 和 `medium`。三个版本/平台组合的正式用例随后在请求总数断言失败，实际为 4，测试预期为 2。两轮的同一原生会话 ID、实际回执和完成断言在此之前已执行。已改为用各轮的用户输入识别模型请求，等待统一复验 |
| 目录未提供档位 | 0.27.4 的原生目录可同时返回空能力对象、初始化回执 `applied=true`。Kar 在开始模型回合前拒绝菜单外的值，原因包含模型与档位。无档位分支在上述请求计数失败之后，待统一实测 |

统一复验时，设置 `KAR_QWEN_BIN` 为对应发行包中 `qwen` 的绝对路径，运行 `cargo test --locked --test qwen_live effort:: -- --ignored --nocapture`。夹具启动真实 Qwen CLI 与本地模型服务。

## 会话启动参数与模型目录

Qwen 的 `launch.ts`、`options.ts` 和公共 `shared/launch-args.ts` 定义了每次启动独立传参的行为。Kar 的 Rust API 使用 `SessionOptions.launch_args`，CLI 的 `--launch-arg` 可重复。恢复会话时可重新提供参数。保存的聊天和录制均不保存这个字段。协议、审批、会话 ID、提示词及模型由 Kar 管理，参数检查同时识别长选项的等号形式、原生驼峰别名和带值短选项。允许的值原样传递。版本探测和自定义启动器仍使用已有的 `Runtime.args`。Qwen 的默认审批沿用 Kar 的原生确认流程。

| 固定源码与供应商断言 | Kar 实现 | macOS 统一验收操作与期望 |
| --- | --- | --- |
| `list-models.ts` 与 `qwen-models.vendor.test.ts` | 启动阶段精确匹配原生未选择认证方式的根结果，生成带认证类别的错误。模型表过滤缺少非空 ID 的记录，解析原生目录中的显示名、档位及默认值 | `cargo test --locked --test qwen_live catalog_launch::catalog_distinguishes -- --ignored --nocapture`。用隔离目录构造未认证、已配置环境但无模型、已配置模型三种状态。预期分类分别为未认证、空、可用，整个目录查询没有模型请求 |
| `launch.ts`、`options.ts`、`native-memory.ts` | `src/options/launch_args.rs`、`src/session.rs`、`src/options/qwen_memory.rs`。额外目录、禁用工具紧跟自定义参数，内存设置采信判断涵盖本次参数 | `cargo test --locked --test qwen_live catalog_launch::session_launch_arguments -- --ignored --nocapture`。预期拒绝被管理的选项，错误中没有其私有参数值。重启后允许重新提供参数、回到同一原生 ID |
| `session.ts`、`context-usage.ts`、`projection.ts`、`tasks.ts` | 收到根回合结果时，只有活动且未请求取消的回合才发一秒限时上下文查询。Qwen 已发送的中断绑定发起时的回合。子任务仅投影嵌套在 `data` 的原生字段和 `completed/failed/stopped` 三种终态 | `cargo test --locked --test qwen_live actual_qwen_turn_catalog_history_and_interrupt -- --ignored --nocapture`，以及 `cargo test --locked --lib qwen_task_notifications`。分别检查原生中断、恢复后的当前上下文与累计计数，及未知终态的投影。单元断言仅使用协议数据 |

本批在 macOS 27.2.1 arm64、rustc 1.100.0 上执行 `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo check --locked --all-targets`，退出码为 0。上述实机命令待统一运行。前述档位场景中，若原生省略当前模型且目录上各模型的档位不一致，Kar 无法确认默认模型的档位，会拒绝启动。其他供应商各自还需接入每次启动参数的验证与传递。

## 其他 Qwen 供应商断言对应路径

| 固定供应商断言 | Kar 路径与待验状态 |
| --- | --- |
| `qwen-native-memory.vendor.test.ts` | `src/options/qwen_memory.rs` 和 `tests/qwen_live.rs` 核对系统默认、用户覆盖、临时文件隔离和真实模型请求。统一复验 `cargo test --locked --test qwen_live native_memory_default -- --ignored --nocapture` |
| `qwen-provider-auth.vendor.test.ts` | `src/providers/qwen.rs`、`src/account/qwen.rs` 和 `tests/qwen_provider_live.rs` 分别处理原生提供商配置、认证状态和存储密钥复用。统一复验 `cargo test --locked --test qwen_provider_live -- --ignored --nocapture` |
| `qwen-skills.vendor.test.ts` | `src/inventory/skills.rs` 通过 ACP 调用 `qwen/status/workspace/skills` 获取原生列表。统一复验 `cargo test --locked --test skill_inventory_live qwen -- --ignored --nocapture` |
| `qwen-tool-ids.vendor.test.ts` | `src/wire/tool_activity.rs`、`src/wire/argument_stream.rs` 和 `src/transport.rs` 按原生调用 ID 和工具路径记录工具事件。跨恢复重用 ID、同一回复的三路重复 ID 和参数分片四种形态尚缺本批实机断言 |
| `qwen.vendor.test.ts` | 恢复后的历史、根上下文、原生初始化、工具排除和提示词组合在 `tests/qwen_live.rs`。MCP 双传输带凭据跨重启、工具结果与原生记忆提醒的边界、扩展存储锁冲突、真实后台子任务的逐事件断言均列为待统一验证 |
