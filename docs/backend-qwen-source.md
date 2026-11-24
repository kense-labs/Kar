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
