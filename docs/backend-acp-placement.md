# OpenCode 与 Grok 的生产会话进程

本项对照上游运行时 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 的公开入口。Kar 的 `auto` 与 `isolated` 都会给每个会话单独启动原生 ACP 进程。显式 `shared` 在启动供应商程序前返回 `Error::Unsupported`，原因包含请求的 placement 与该供应商的原生子进程限制。

| 供应商 | 固定源码与边界 | 原生证据定位 |
| --- | --- | --- |
| OpenCode，全版本、全平台 | `packages/oar/src/runtimes/opencode/index.ts:63` 将生产入口绑定到 `sharing-admission.ts:7`。仅实验入口在原生 2.2.3、2.2.5 特定平台上使用共享适配器，并再次核对 initialize 返回的精确版本。 | `docs/runtimes/opencode.md:182–202` 和 `experiments/sharing/opencode-plugin-environment-2026-10-31.md` 记录了插件子进程在共享 HOME 启动的反例。`sea-trial/vendor/opencode-sharing.vendor.test.ts:7` 导入实验入口。 |
| Grok，全版本、全平台 | `packages/oar/src/runtimes/grok/index.ts:55` 和 `sharing-admission.ts:5` 使生产入口也拒绝显式共享。原生 1.0.70 位于实验白名单。 | `docs/runtimes/grok.md:49–68` 与其原生实验记录说明，用户配置的 stdio MCP 进程可能从共享 HOME 启动。`sea-trial/vendor/grok-sharing.vendor.test.ts` 验证实验适配器。 |

上述限制来自公开上游的产品准入与原生隔离反例。原生协议在实验条件下能创建多个会话。Kar 未实现这两个实验进程池。

| 验证层级 | 目标操作 | 期望和状态 |
| --- | --- | --- |
| Rust 公开 SDK | 给两个运行时配置不存在的可执行文件，分别尝试显式 shared 的 open、resume 和 fork。 | 三个入口均先返回各自的隔离原因。用例已编译，待联合运行。 |
| macOS 实机 OpenCode 和 Grok | 在同一个临时 HOME 下打开两个不同工作区，A 用 auto、B 用 isolated。并发各发一个独有标记，关闭 A，在 B 继续聊天，再按原原生会话 ID 重开 A。 | 进程 ID 集合互不重合。两端 ACP 开场的 cwd 与各自请求一致，事件只出现本会话的提示；A 的进程退出，B 的进程和对话持续可用；B 关闭后 A 能续聊。两个用例均使用真实供应商 ACP 与本地脚本模型，已编译，待联合运行。 |

已执行 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests` 和 `git diff --check`。以下命令留给联合验收。先将 `KAR_OPENCODE_BIN` 或 `KAR_GROK_BIN` 指向相应已选原生发行版，保留 `--exact` 单独运行 PID 对照。

```sh
cargo test --locked --lib placement::acp_admission_tests::production_shared_refusals_precede_open_resume_and_fork -- --exact
cargo test --locked --test native_live opencode_fork::native_process_placement_keeps_two_workspaces_independent -- --ignored --exact --nocapture
cargo test --locked --test grok_live native_process_placement_keeps_two_workspaces_independent -- --ignored --exact --nocapture
```
