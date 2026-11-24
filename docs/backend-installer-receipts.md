# 安装计划、安装回执与更新回执

对照上游运行时 `v0.130.1` 的固定提交 `2b15859a13dcc50e8b66cbaee5a007832aa00e08`，定位为 `packages/oar/src/contracts/{install,update}.ts`、`shared/{install,install-npm,update}.ts` 和各供应商的 `install.ts`、`update.ts` 或 `upgrade.ts`。Kar 继续使用自己的 CLI 名称和 JSON 字段。

| 操作 | Kar CLI | Rust SDK |
| --- | --- | --- |
| 只读预检 | `kar install qwen --plan --json` | `Runtime::inspect_install(line, &InstallationOptions)` |
| 实际安装 | `kar install opencode --line v2 --json --timeout-ms 600000` | `Runtime::install_receipt(line, timeout, &options)` |
| 查询版本 | `kar update factory --check --json` | `Runtime::check_update(timeout)` |
| 执行更新 | `kar update factory --json --timeout-ms 600000` | `Runtime::upgrade_receipt(timeout, &options)` |

预检返回 `kind=plan` 或 `unsupported`。计划包含供应商说明地址、实际程序与参数、步骤环境、写入目录、网络需求及 `privileges=false`。脚本下载与执行分成两个子进程，计划中的临时文件用占位符显示。Kar 检查当前用户对目标或最近已存在上级目录的写入和目录访问权限；npm 通过带独立临时缓存的本地 `prefix -g` 查询读取真实全局前缀，删除临时缓存后返回。工具缺失、需要权限、位置未知这三种拒绝保留步骤和来源；配置冲突、平台不支持、缺少或不识别发布线等拒绝给出各自原因。npm 的诊断信息和原始前缀错误输出不会进入报告。Kar 可按构建平台提供的附加安装方式沿用原有方法描述，本次验证平台为 macOS。公开预检枚举也容纳供应商仅提供 GUI 的原因，本基线已实现的本地方法没有这个拒绝条件。

安装前已发现可用程序时返回 `already_installed`。OpenCode 附带实际发现的 `v1` 或 `v2`，即使请求的是另一条线，也保留原有安装。每次实际执行均记录退出码和输出。执行结束后，Kar 重新探测；发现可用程序时返回 `installed` 及完整安装快照，仍不可用时返回 `failed`、原生退出码和输出。超时或被信号终止对应 `exit_code=null`。执行以当前用户身份进行，关闭 stdin，清除包脚本标记及工作台宿主令牌，保留 npm 前缀等设置。输出按每一步 stdout、stderr 的顺序拼接，各流保留最后 1 MiB，超时附加 Kar 说明并停止子进程组。

更新对所选可执行文件读取版本，原地运行原生更新器，再读取同一路径。已是目标版本为 `current`，实际版本变化为 `upgraded`。退出 0 但版本相同为 `unchanged`，更新失败或版本已不可读为 `failed`。缺少可更新的安装或供应商没有对应更新器返回 `unsupported`，旧版 Kimi 需要交互终端时原因为 `requires_terminal`。更新检查本身仅调用已知的只读入口。结构化安装或更新的实际失败及更新未生效产生非零 CLI 退出状态；预检可正常返回拒绝理由。原有 Rust `install[_with]`、`update[_with]` 和 CLI 紧凑结果仍可使用；同步 `install_plan_for` 继续提供静态方法描述。

## macOS 本地复验

- `cargo check --locked --all-targets` 编译 Rust 库、CLI 与测试入口。
- `cargo test --locked --test setup_receipts` 在临时 HOME、PATH 和当前工作目录内启动 Kar。Rust 测试程序扮演下载器、解释器、npm 及供应商程序。覆盖丢失工具、受限目标目录、不可用 npm 前缀及诊断脱敏，验证 Qwen 目的地、OpenCode 发布线、Kimi 终端拒绝、脚本和 npm 的实际参数、安装去重、非零退出但实际安装或升级、零退出但程序缺失或版本未变、另一条路径出现新版本、超时输出和真实退出码。
- `cargo test --locked --test setup_cli release_line_planning_requires_an_explicit_supported_choice` 复验原有计划入口。
- 这些命令使用本地夹具，不下载或运行真实供应商安装器，不更改真实账户或软件。供应商发布物、真实主机安装和 Windows 权限行为各自需要对应的实机条件。
