# macOS 工作台综合验收

`docs/backend-workbench-acceptance.toml` 的 `final_acceptance` 对应同一个测试入口。场景使用一个临时 Kar 状态目录、一个注册的源项目和一个从 PR 创建的管理工作树。工作树注册为第二个项目，两个客户端始终在该项目的同一个聊天里协作。测试在完整保留状态和工作区的情况下终止并重启 Kar 服务。

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

同一组实际托管凭据和两个真实供应商共同运行整条链路仍待实机验证。上表三个入口提供分别可复核的原生供应商和真实托管证据。

## 本批证据

工作台的 macOS 集中入口为 `cargo run --locked --example backend_suite -- --runtime workbench`。默认列出命令和环境要求。两份清单集成后，在仓库根目录以同一候选提交执行 `cargo run --locked --example backend_suite -- --execute --output /tmp/kar-macos-候选提交`，可在同一证据目录记录两个阶段。保留每条实际命令、进程退出状态、输出和供应商安装探测。真实托管场景需要显式指定专用测试库 `KAR_FORGE_TEST_REPO` 与凭据，真实接力场景需要设置两个供应商程序路径并能访问模型。办公室文档引擎通过独立 Cargo manifest 运行。

在 macOS 开发分支运行过 `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo check --locked --all-targets`，退出码 0。三个 Office 引擎是测试依赖，继续使用项目现有固定提交 `9b780379e32d3fa4d293499f33fbc2483655567d`。按当前安排，综合场景、各实机入口和全量回归均待统一执行。本轮未进行 Linux 或 Windows 实机验收。
