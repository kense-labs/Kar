# Kar 后端断言与证据对照

在 Kar 仓库根目录运行以下命令。需要 Rust、Git，以及包含固定提交的 Kar 和上游运行时仓库历史。将 `UPSTREAM_CHECKOUT` 指向本机上游运行时检出目录。

```sh
rustc --edition=2024 -D warnings tools/backend-coverage.rs -o /tmp/kar-backend-coverage
/tmp/kar-backend-coverage --upstream "$UPSTREAM_CHECKOUT" > /tmp/kar-backend-coverage.tsv
/tmp/kar-backend-coverage --filter session.cursor-replays-without-loss-or-duplication
```

完整命令按验收清单的截止提交 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 读取上游 Git 对象，检查索引的源码位置、断言行范围和公共用例 ID。筛选参数匹配用例或文件位置。省略 `--upstream` 时只检查 Kar 侧关联。每次运行均校验已关联的 Kar 测试文件、固定提交内的断言定位文本和对应的历史证据行。

输出的每行保留供应商、版本、传输、平台、操作、预期、测试命令、实际观察及原始验收状态。同一断言可对应多个平台或运行记录。`assertion` 是上游行号和行内序号，空值表示索引未列直接断言。`kar_test` 是指定 Kar 提交内用于复查的测试片段位置。每条映射在 `backend-assertion-map.tsv` 列出实际比对的行为和供应商范围。

- `原生程序与回环模型` 表示历史测试运行已安装供应商程序，模型响应由测试端点提供。
- `Chromium / 原生 Harness 与回环模型` 表示历史测试在真实浏览器中运行指定版本的 Harness，模型端点使用脚本响应。
- `同址待核对` 表示证据表中有人核对过这一源码位置，报告仍保留逐条核对所需的断言、操作、命令及结果。`待核对` 表示证据表尚无以该位置为键的记录。供应商不支持、真实部署缺失等结论以对应记录的 `ledger_status`、`transport` 和 `observed` 为准。

## 固定 CLI 的 50 个位置

`docs/backend-cli-map.tsv` 将固定上游的 12 个命令、32 个选项、2 个位置参数和 4 种库存查询逐项对应到 Kar 主干 `f4a54ae` 的实现。每行列出适用环境、Kar 命令、操作和预期结果。模型选择器、四种工作区清单、账户与安装更新、MCP 委派和日志都从常用 CLI 入口检查。Kar 默认输出 JSON 的命令直接使用默认输出。

统一报告用「CLI 实现位置」标出这些行，位置同时匹配源码文件、上游行号和选项名称。当前检查依据是固定提交的两端源码。集中验收时需在真实 macOS 安装上补充版本、命令输出及退出码。登录、退出、安装和更新操作使用可恢复的测试账号或安装环境。表内 `$WORK`、`$SESSION`、`$MODEL`、`$EFFORT`、`$TIER`、`$IMAGE1`、`$IMAGE2`、`$LOGS` 和 `$NEW_RECORDING` 由该次运行提供。

## 首批核对范围

在 Kar `661854c` 的索引和历史证据上复跑时，脚本读取了 233 个上游文件、2,095 条索引和 283 条历史证据，发现 2 条完全重复的历史记录。18 条人工核对映射关联了 10 个公共用例中的 40 个直接断言，以及 5 个 Pi Durable 专有用例中的 21 个直接断言。固定索引中的 20 个公共用例和 381 个供应商测试全部进入报告。复跑报告核对历史证据的出处，当前环境中供应商的结果需运行报告里的测试命令另行记录。

以下公共用例仍缺以其位置为键的历史证据。

| 公共用例 | 要核对的问题 |
| --- | --- |
| `session.capabilities-match-declaration` | 能力声明与会话方法、原生会话文件、上下文用量是否吻合 |
| `session.effort-never-substituted` | 不支持的推理级别是否被静默替换 |
| `session.effort-listed-levels-apply-across-resume` | 列出的推理级别是否在恢复后准确生效 |
| `session.images-travel-with-input` | 图片路径是否随输入记录，无效图片、纯图片输入及回合终态是否符合约定 |
| `session.framing-and-attribution` | 记录边界和事件归属是否正确 |
| `session.resume-or-loud-rejection` | 是否成功恢复或给出可观察的拒绝 |

已关联的位置也包含剩余断言。例如队列撤回后的会话状态、Pi Durable 原生会话 ID 的正整数检查，仍需单独核对。安装、账户用量、回合结束后引导和回合中引导已有同址证据，首批尚未把它们关联到具体断言。报告逐行保留这些项和其余供应商测试，供后续追加有适用范围的证据。

## 候选提交的逐条关联

Pi RPC 的原生输入处理、预执行拒绝、独立扩展回合和插话已有对应历史记录。映射按输入身份、回合终态、请求次数和等待状态分别定位。同一固定源码行出现多处断言时，行号后的 `.1`、`.2` 标识从左至右的序号。Kar 测试定位可用 `fn ... >> ...` 指定函数内的断言。

Codex 与 Claude 中止无应答的实现和原生测试位于 `f876d7a`，Pi、Qwen、Factory 的撤回状态和浏览器原生会话 ID 断言位于 `924463c`。这些新条目的状态保留为「未完成或失败」，观察栏记载编译情况和复验条件。浏览器第 74 行检查的是传给 Harness 的正整数会话 ID。

## macOS 统一入口

在 Kar 仓库根目录列出候选场景。供应商表登记独立集成测试目标和 libtest 完整测试名，另有两项原生 MCP 测试、浏览器宿主、固定源码对照、格式检查、Clippy 和全量 Cargo 回归。新加入的测试应同步更新 `backend-macos-tests.tsv`。

```sh
cargo run --locked --example backend_suite -- --runtime pi
cargo run --locked --example backend_suite -- --runtime codex --filter unanswered_interrupt
```

集中验收时设置计划列出的原生程序路径和附加条件，例如 Pi 独立发行包配套的 SDK，或 Cursor 对照观察程序。每个版本单独运行并使用新的输出目录。下面的 Codex 示例使用 `KAR_CODEX_BIN`，版本参数按 Kar 安装探测实际报告的完整字符串填写。

```sh
cargo run --locked --example backend_suite -- --runtime codex --filter unanswered_interrupt --execute --output /tmp/kar-codex-abort-result
cargo run --locked --example backend_suite -- --runtime kar --execute --output /tmp/kar-core-result
```

每条记录保存实际提交、工作区状态、macOS 版本、架构、探测到的供应商程序和版本、操作命令及日志。完整 libtest 名称必须命中一个成功的测试。缺程序、版本不符、测试未命中、命令失败和命令完成有独立结果；任何未完成的操作都会使套件以非零码结束。外部条件在 `requires` 中列出。不带 `--execute` 只打印计划。

`kimi-peer` 是固定上游 KAP 协议对端，需设置 `KAR_UPSTREAM_SOURCE`。浏览器 `pi-durable` 场景另需依 README 准备 Wasm、Chromium、固定 Harness 和 Node。记录浏览器与 Harness 版本时使用该宿主的运行日志。CLI 与部署 API 的逐项操作、外部认证和未建立 Kar 测试的上游断言，仍在固定源码对照和证据表中逐项核对。统一入口的命令成功后，按实际日志登记供应商断言结果。

`kar` 的 `ignored-inventory` 项检查当前候选实际编译出的第一阶段标准 libtest 名称。它读取 Cargo 清单排除自带入口的测试程序，调用标准测试程序的列举功能，报告遗漏与失效的名字。若供应商可执行文件存在但版本探测未确认，入口继续保留实际场景输出并标为 `command_succeeded_version_unconfirmed`；显式要求某版本时须匹配主程序的探测结果。

在统一入口使用 `--runtime <供应商> --filter backend_public` 可以列出该供应商的公共契约场景。14 个本地进程适配器均登记了安装快照、能力声明、未知推理级别、原生会话恢复四项。声明账户用量的适配器另有账户快照。每条命令单独建立临时工作区，使用所选原生程序，可由 `KAR_PUBLIC_<供应商大写>_MODEL` 指定模型。能力测试按声明检查无鉴权 HTTP MCP、真实回合后的上下文或会话文件。命令记录原生会话 ID 或明确拒绝的原因。登记及编译仅表示用例可以进入集中验收。

公共回合场景复用同一套 Rust 断言。每个本地适配器登记了显式与自动 UUID、带图与空输入、活动回合互斥、排队、撤回、插话、中止、关闭、原生恢复和订阅回放的现场命令。对同一会话的多回合、观察者隔离、游标续收、回合边界、结束后插话和关闭回执用 `session.framing-and-attribution` 合并采集，以便比较同一批原生记录。也可以将其中任一固定公共 ID 传给 `cargo run --locked --example backend_public -- --runtime <runtime> --case <id>` 重跑。模型菜单提供至少两档时，`session.effort-listed-levels-apply-across-resume` 完成一个真实回合，换档并在同一原生会话续聊。供应商未声明的能力和实际拒绝会保留在结果，编译和列出命令均不能代替现场证据。

集中执行时，未显式指定的标准供应商程序由 Kar 的安装发现入口查找；解析出的实际路径传给子进程并写入每项记录。指定 daemon、worker、ACP 旧版本等变体仍用各自的环境变量指向对应发行包。Chrome 预览接受 `KAR_TEST_CHROME` 或 macOS 标准应用路径，缺少浏览器会保留为待配环境。

### 社区服务的复验条件

固定上游的 Cybion `community/cybion/thread.ts` 将新建时指定的标题和推理强度传给线程，恢复时读取已有线程，HTTP 404 携带请求位置和线程 ID。Kar 的普通 SDK、CLI 和聊天入口现共用这条路径。本轮 macOS 开发环境未提供 Cybion 部署地址、访问密钥和部署版本。协议用例使用本地 HTTP 服务。真实服务复验需由部署提供可用的模型 Worker 和可创建测试线程的凭据，设置 `KAR_CYBION_BASE_URL`、`KAR_CYBION_API_KEY` 及可核对的 `KAR_CYBION_DEPLOYMENT_VERSION`，执行 `cargo test --locked --test cybion deployed_thread_creation_and_resume -- --ignored --exact --nocapture`。可选 `KAR_CYBION_TEST_MODEL` 和 `KAR_CYBION_TEST_EFFORT` 应取自该部署支持的值。本轮已编译用例，实际创建跨会话的两轮与 404 证据待收集。

固定上游的 Morph `community/morph/projection.ts` 从 Console 的重试轨迹读取当前尝试、最大尝试次数和可选等待毫秒数。Kar 已具备共享或外部 Console 的三条 macOS 原生恢复与投递用例，版本线索保存在原有台账中。本轮新增的重试投影覆盖秒、毫秒、缺省等待与缺失尝试字段。集中验收时用 `cargo test --locked --lib retry_uses_the_native_attempt_counts_and_delay_units` 检查投影，再用可触发重试的真实 Console 和模型服务核对原生轨迹与 Kar 事件。新增字段尚待真实重试观测。
