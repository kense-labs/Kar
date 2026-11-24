# macOS 工作台综合验收

`docs/backend-workbench-acceptance.toml` 的 `final_acceptance` 对应同一个测试入口。场景使用一个临时 Kar 状态目录、一个注册的源项目和一个从 PR 创建的管理工作树。工作树注册为第二个项目，两个客户端始终在该项目的同一个聊天里协作。测试在完整保留状态和工作区的情况下终止并重启 Kar 服务。

## 两阶段候选交接

本次复核的功能提交为 `61650ba7437510369ac2907f8ae268d1a2948564`。此前的 `ad7e47868f0f54c250dcf4638093c9118b901994` 是较早的交接点。后续文档提交与功能提交的源码树相同。源码复核发现的缺口见[功能对照报告](backend-functional-review-2026-11-24.md)和[逐项清单](backend-source-parity.toml)。

macOS 27.2.1 arm64、rustc 1.100.0 上的 `cargo check --locked --offline --all-targets` 已完成。固定源码映射检查也已执行，核对 233 个上游来源文件、192 项断言映射及 50 项 CLI 映射。工作台清单收录 257 个 RPC，模型工具表有 95 个入口（含工具搜索）。各项实测判定见下述命令及供应商记录。 [静态检查复跑说明](backend-coverage.md)

同一功能提交上，以下命令编译并链接了 16 个测试目标，耗时 1 分 11 秒，退出码为 0。macOS 链接器报告调试版主程序的 `__eh_frame` 超过 16 MB，部分展开信息无法压入 compact unwind 表。本命令仅生成可执行文件。

```sh
cargo test --locked --offline --no-run --lib \
  --test chat_restart --test setup_cli --test cybion --test workbench_mcp \
  --test workbench_handoff --test workbench_acp_registry --test workbench_provider_chat \
  --test provider_queue_crash --test workbench_remote_access --test workbench_oauth \
  --test workbench_terminal_preview --test workbench_configuration \
  --test workbench_managed_providers --test workbench_acceptance --test workbench_forge
```

本次交接优先复验以下三种故障窗口，每项记录客户端游标、Kar 输入/回合 ID、供应商会话/回合 ID、实际收到次数以及磁盘状态。测试使用可丢弃项目、独立聊天存储及测试账号。

| 场景 | 接手方的检查 |
| --- | --- |
| 两端实时聊天与断线续收 | 两客户端同时订阅同一回合，在第二端断线时继续生成和编辑。用旧游标重连，核对两端内容、顺序、无重复，且在线端在回合结束前已收到事件。 |
| 原生输入跨重启 | 排两条不同消息，分别在供应商接收前、接收后而 Kar 尚未记账时终止 Kar。比较原生历史与 Kar 回复。恢复原队列顺序，证据不足时只将队首标记待确认。 |
| 按回合恢复工作区和模型 | 在三个回合中分别改文件并告诉模型不同事实。选择第二回合，核对 Git 字节、原生模型绑定、下一轮能访问的事实；重启后重试相同 command ID。 |

本轮新增的分场景复验命令如下。未配置真实供应商、宿主或账号的命令只能提供本地协议证据。

```sh
cargo test --locked --test chat_restart -- workbench-thread-family
cargo test --locked --test workbench_mcp -- apps
cargo test --locked --test workbench_mcp -- provider-controls
cargo test --locked --test workbench_mcp
cargo test --locked --test workbench_configuration
cargo test --locked --test provider_queue_crash
cargo test --locked --test workbench_handoff
cargo test --locked --test workbench_acceptance
```

按 [运行时 12 项](backend-acceptance.toml) 和 [工作台 8 项](backend-workbench-acceptance.toml) 的用户操作判断结果。运行时另含表内 20 个公共用例及适用的供应商行为。先保存 `git rev-parse HEAD`。在仓库根目录用 `cargo run --locked --example backend_suite` 查看命令、用例和环境。可按供应商或测试名筛选，执行模式会把版本、命令、返回码和日志保存在新建的输出目录。

| 优先操作 | 从正常入口复验 |
| --- | --- |
| 两客户端各自断线续收，强杀后回原会话，核对 FIFO 与待确认的输入 | 配置 `KAR_QWEN_BIN` 后执行 `cargo test --locked --test qwen_live queue_crash:: -- --ignored --nocapture`；OpenCode 设置 `KAR_OPENCODE_BIN` 并执行 `cargo test --locked --test native_live opencode_fork::queue_crash::two_clients_and_fifo_after_native_receive_before_host_receipt -- --exact --ignored`。这两组使用真实 CLI 和回环模型端点。 |
| 查看逐轮文件变化、恢复文件树，并从中间回合继续聊 | `cargo test --locked --test chat_restart` 覆盖 CLI、Git 工作区与原生协议夹具；配置 `KAR_PI_BIN` 后执行 `cargo test --locked --test native_live pi_history::three_native_writes_fork_at_middle_and_reopen_both_contexts -- --exact --ignored`，复查实际 Pi 父子会话。 |
| 当前模型的图片能力、只发图片、上传失败及模型切换 | 见 [DSH 测试和环境](backend-input-images.md)。 |
| 项目、工作树、交接、PR、终端预览、工作流、分享及文本和 Office 双写 | `cargo test --locked --test workbench_acceptance` 贯通本地流程。下文另列专用 GitHub 仓库、真实 CLI 与跨设备操作。 |

**已有证据的边界。** Kimi 与 Codex 的现有用例通过真实原生历史与截短的 Kar 日志构造“原生已接收、Kar 尚未记录回执”的现场。要核销这两个供应商在该时刻真实强杀的行为，还需在实际投递窗口终止控制器，保留双方日志，核对每条消息的身份、次数和顺序。Cybion 的远端关闭及运行中收尾需有实际部署、凭据和可识别版本；现有部署测试 `deployed_thread_creation_and_resume` 覆盖创建和恢复。按供应商分别报告实际执行、协议夹具和环境缺失。Linux 后移，Windows 实机排除。

## 本地跨模块场景

在 macOS 仓库根目录运行 `cargo test --locked --test workbench_acceptance`。环境需能运行 Git、PTY 及本机回环 HTTP。测试使用锁定版本的 Kantor Word、Excel、PPT Rust 引擎真实生成、编辑和重新打开 Office 文件。工作流步骤在工作树中真实执行，Git 推送写入临时裸仓库。供应商 stdio 和代码托管 API 使用 Rust 协议对端。

| 操作 | 核对内容 |
| --- | --- |
| 从 PR 1 准备工作树，在工作树内从 Issue 7 打开聊天 | 重试准备返回原工作树 UUID 和初始提交，聊天工作区与工作树目录一致 |
| 两个客户端观察正在运行的 A 回合，A 给出两个事实及一次工具结果，切换到 B | 双方在完成前读到同一回合的 running 事件。B 实际接收包含此前事实和工具结果的原生输入；同一消息 UUID 重试只提交一次 |
| 在工作树 PTY 内运行 Rust HTTP 服务，创建浏览器预览 | 预览必须经由该 PTY 所拥有的监听端口，响应正文与工作树内 Markdown 的实际内容一致 |
| 人通过正常保存改第一行，模拟 Codex 原生文档工作进程在私有副本上改第三行 | 在模型工作进程等待时保存人的修改，双方的改动合并到修订 2。再从老基准提出修改第一行的候选文件，核对冲突回执，磁盘仍保留已合并内容 |
| 在同一目录生成 Word、Excel 和 PPT，各以两个 HTTP 客户端并发提交不同目标，再提交一次相同目标的旧基准修改 | 每种格式两个兼容操作各生效一次，两次修订不同。Word 逐段、Excel 逐单元格、PPT 逐形状核对内容。冲突回执引用修订 2，冲突提议持续可恢复 |
| 从工作台暂存 Markdown 与三个 Office 文件，提交、推送，再创建关联 PR 并标记 Markdown 已审 | 裸仓库的分支指向实际提交。分别比对远端 Markdown 正文和三个 Office 文件的 Git blob。按 PR 的基准与头提交读取补丁，已审状态固定到该头提交 |
| 同一签名 Webhook 重发，访客提议重发，由主人接纳 | Webhook 返回同一个运行 ID，工作流对磁盘仅写入一个字符。访客得到同一条提议并无法访问项目文件。接纳后出现一个新的用户回合 |
| 服务重启，断线客户端从原游标续收，再在 B 原生会话发送新消息 | 双方回合及 ID 一致，最新原生输入的会话和目录仍是 B 与原工作树。原生文档编辑、工具调用、工作流和 PR 创建的次数维持预期，Git 远端分支也保持原修订 |
| 从重启后的服务取回 Markdown 冲突及候选字节，显式解决三个 Office 冲突 | 保留已接受版本，重试解决收到同一回执，三种文件以各自引擎从磁盘重新打开。访客权限、已审文件和工作树身份在重启后重新核对。旧 PTY 标为 interrupted，旧预览授权已回收 |

成功执行时测试输出一条 JSON，其中记录项目、工作树、候选分支、两个供应商的会话、聊天、事件游标、PTY、预览授权标识、Webhook 运行、访客提议、提交、PR 和三个 Office 文件各自的接受修订与冲突解决回执。此行由运行时产生，文档不预填任何通过结果。

## 单独的实机验收入口

| 操作 | 前提 | 复验命令与实际影响 |
| --- | --- | --- |
| 真实 Codex 与 Qwen 两端来回接力 | 两个已安装且可运行的供应商 CLI 绝对路径，对应模型认证及 Codex 在隔离工作区使用读取工具的权限 | `KAR_HANDOFF_LIVE=1 KAR_CODEX_BIN=/absolute/path/to/codex KAR_QWEN_BIN=/absolute/path/to/qwen cargo test --locked --test workbench_handoff`。核对实际 CLI 接收的用户包、模型回答、工具结果和切回后的原生 ID |
| 已安装的 Codex 实际修改原生文档副本 | macOS，Codex CLI 能运行且支持该入口的写入策略。测试使用隔离供应商配置与回环模型 | `cargo test --locked --test workbench_coediting actual_codex_merges_a_native_file_edit_with_a_concurrent_owner_save -- --ignored`。由已安装的原生 CLI 在工作副本中实际执行文件工具 |
| 真实 GitHub 项目 | 明确选择可写的临时 `owner/repository`，准备 `GH_TOKEN` 和可推送该仓库的 Git 认证 | `KAR_FORGE_TEST_REPO=OWNER/DISPOSABLE_REPO cargo test --locked --test workbench_forge -- hosted-github`。会推送测试分支、创建并关闭一个 PR。分支和已关闭 PR 会作为远端记录保留 |

## 同一状态下的真实 GitHub、Codex 和 Qwen

在 macOS 显式选择专用 GitHub 仓库和一条已有 PR。测试读取源 PR 的提交，在同一个 Kar 状态目录新建受管工作树，沿用同一个聊天 UUID 直至服务重启后复核。两个供应商使用本机安装的 CLI 和其真实模型。运行时记录 CLI 实际版本、原生输入总数和会话 ID。

| 环境或入口 | 要求 |
| --- | --- |
| `KAR_FORGE_TEST_REPO`、`KAR_FORGE_SOURCE_PR` | 提供 `OWNER/REPO` 和仓库内已有 PR 编号。选用允许测试推送、新建及关闭 PR 的专用仓库，运行期间保持源 PR 头提交稳定。 |
| `GH_TOKEN`、Git 凭据 | 为 GitHub REST 提供该库读写权限。Git 本身还需 HTTPS credential helper 的克隆、推送权限以及有效提交者配置。令牌由运行环境注入，供应商转发器会移除进程环境中的 `GH_TOKEN` 和 `GITHUB_TOKEN`。 |
| `KAR_CODEX_BIN`、`KAR_QWEN_BIN` | 传入已安装 CLI 的绝对可执行路径，准备其模型凭据和可用额度。Codex 需要读随机文件的工具权限，以及在文档编辑入口确认隔离副本的 workspace-write 策略并实际保存文件。 |
| 源工作树 | 根目录的 `notes.md`、`plan.docx`、`plan.xlsx`、`plan.pptx`、`.kar-preview-port` 和 `webhook-count` 均须空缺；对同名文件、目录、符号链接立即退出。测试在新的临时状态和工作树上运行。 |

设置上表环境后，单独复验的入口为 `cargo test --locked --test workbench_acceptance -- hosted-codex-qwen`。也可用 `cargo run --locked --example backend_suite -- --runtime workbench --filter hosted-codex-qwen --execute --output /tmp/kar-hosted-evidence` 保存退出结果和输出。普通 `cargo test --locked --test workbench_acceptance` 始终走本地协议夹具。

- 主人和经配对的设备分别订阅同一回合的进行中事件。Codex 实际读取随机文件并返回工具证据，Qwen 凭转交内容复述两个事实。Rust 转发器核对输入 UUID、原生会话和实际调用次数。第二个客户端断开，服务重启后从原游标补收，并与主人和访客逐回合核对。
- PTY 启动本地服务，预览请求读回 Markdown。主人修改日期行，真实 Codex 在已固定的基准副本中修改作者行。正常文档接口合并兼容提议。Word 段落、Excel 单元格和 PPT 对象由两个作者身份通过正常 HTTP 文档接口提交结构化提议，使用 Kantor 真文件。三种 Office 格式的第二个客户端提交确定性结构化修改；该断言验证 Kar 双写行为。
- Git 只暂存这四个文件并创建一个新提交，检查推送目的地和新分支名后推送 `kar/review-<源PR>-<UUID>`。GitHub REST 独立读取远端分支、提交树、四个 blob 和新 PR。Markdown 逐字节对照，Office 从下载的远端字节重新用 Kantor 打开。随后按头提交标记已审路径。
- 测试向 Kar 正常 Webhook 路由发送签名 HTTP 请求，复用同一事件检查脚本只执行一次。经真实 Qwen 接纳访客提议，检查提议、聊天回合和原生输入三处 UUID 一致。重启后再次查询审阅、冲突候选、Kantor 文件、工作流和订阅，继续在原 Qwen 会话发言并核对回答。
- 只有本次 Kar 操作标记、仓库、分支、提交和目标分支全部匹配的新增 PR 才会关闭。断言失败或测试超时也会尝试这项清理。保留专用分支及关闭的 PR 作为证据。若进程被外部直接终止或 GitHub 拒绝清理，可用 stderr 记录的仓库、分支与 `kar-operation` UUID 定位运行。工作流事件在本机主动签名投递，真实跨设备连接与 GitHub 主动出站 Webhook 另需各自环境。

该综合实机入口已在 `aarch64-apple-darwin` 通过 `cargo check --locked --all-targets`，整条链路和远端实际结果待统一验收。

## 本批证据

工作台的 macOS 集中入口为 `cargo run --locked --example backend_suite -- --runtime workbench`。默认列出命令和环境要求。两份清单集成后，在仓库根目录以同一候选提交执行 `cargo run --locked --example backend_suite -- --execute --output /tmp/kar-macos-候选提交`，可在同一证据目录记录两个阶段。保留每条实际命令、进程退出状态、输出和供应商安装探测。真实托管场景需要显式指定专用测试库 `KAR_FORGE_TEST_REPO` 与凭据，真实接力场景需要设置两个供应商程序路径并能访问模型。办公室文档引擎通过独立 Cargo manifest 运行。

在 macOS 开发分支运行过 `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo check --locked --all-targets`，退出码 0。三个 Office 引擎是测试依赖，继续使用项目现有固定提交 `9b780379e32d3fa4d293499f33fbc2483655567d`。按当前安排，综合场景、各实机入口和全量回归均待统一执行。本轮未进行 Linux 或 Windows 实机验收。
