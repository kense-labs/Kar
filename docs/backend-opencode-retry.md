# OpenCode 原生重试通知与续收

对照提交为 `2b15859a13dcc50e8b66cbaee5a007832aa00e08`。上游运行时的实现位于 `packages/oar/src/runtimes/opencode/retry.ts`，版本与协议定位见 `docs/runtimes/opencode.md` 的 “Prompt, steering, queueing, and abort”。固定断言位于 `tests/opencode/retry.test.ts`，原生对照记录位于 `experiments/opencode-retry-status/controls.json` 与 `projected.json`。

| 层级 | 版本与平台 | 操作与预期 | 本轮证据 |
| --- | --- | --- | --- |
| Rust 协议投影 | OpenCode 2.x 与 1.x，macOS 编译 | v2 收到 scheduled 通知后，仅投影一次原生尝试次数、相对接收时刻的剩余毫秒与公开错误类别。空通知、无效次数、别的会话和 v1 不产生事件。保留旧录制仅含 reason 的重试。 | 用例已编译，待联合运行。 |
| 普通 Rust ChatStore、持久历史与游标续收 | OpenCode 2.3.2，macOS 原生 ACP，回环模型 | 模型首次返回一次 529，原生重试成功。持久时间线包含一条有原始接收时间的重试；客户端从旧游标重放，重复投递同一帧后历史与重开存储一致。 | 可选供应商用例已编译，待联合运行。 |
| 1.x 对照 | OpenCode 1.19.5，macOS 原生 ACP，回环模型 | 同一 529 场景能从模型请求计数观察到重试；原生 ACP 无 scheduled 通知时，Kar 时间线没有 retry。 | 同一可选用例已编译，待联合运行。 |

本轮已执行 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests` 和 `git diff --check`。联合验收分别使用以下命令。第二项对两条发行线各运行一次，环境变量指向所选版本的原生可执行文件。用例会创建独立 HOME、工作区和本地模型端点。

```sh
cargo test --locked --lib wire::opencode::tests::scheduled_retry_uses_the_original_record_clock_and_native_category -- --exact
cargo test --locked --test native_live opencode_fork::native_retry_is_projected_once_and_replayed_at_its_original_time -- --ignored --exact --nocapture
```

供应商命令需要设置 `KAR_OPENCODE_BIN`。公开事件保留的 reason 只来自原生错误类别，错误消息文本不进入该投影。既有原生队列与双客户端实测见 `docs/backend-evidence.tsv` 的 OpenCode 条目；它们覆盖的操作与此次 529 情形分别登记。

## 会话重启后的原生指令

固定专有用例 `sea-trial/vendor/opencode-cleanup.vendor.test.ts:139` 使用自定义默认主 agent 和被禁用的 plan agent。`packages/oar/src/runtimes/opencode/prompt-config-v2.ts` 要求开场回显的实际 agent 必须处于本次覆盖的集合中。Kar 保留实际名称与期望名称；当新的模式字段类型错误时，旧字段即使带有可识别的名称，也不足以确认这次开场。

macOS 协议用例覆盖未报、畸形以及实际与期望不符的模式。macOS 原生可选用例对 OpenCode 2.2.7、2.3.2 分别新建会话、检查默认 agent 与模式列表、向回环模型发送真实请求，再按原 ID 续聊。模型应接收新的替换指令，旧附加指令由原生用户更新撤回。两条命令已编译，留待联合运行。原生用例需要 `KAR_OPENCODE_BIN` 指向对应发行版。

```sh
cargo test --locked --lib options::opencode::tests::instructions_require_the_authoritative_mode_report -- --exact
cargo test --locked --test native_live opencode_fork::native_primary_prompt_replacement_and_retraction_across_resume -- --ignored --exact --nocapture
```
