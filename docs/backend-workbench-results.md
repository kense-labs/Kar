# Kar 工作台后端验收记录

## 无界面 Office 编辑

对应 [第二阶段清单](backend-workbench-acceptance.toml) 的 `coediting`。Kar 主干提交为 `6c347b1`。Kantor 三个 Rust 引擎及工作簿计算依赖均锁定在 `9b780379e32d3fa4d293499f33fbc2483655567d`。测试以实际二进制 Office 文件为输入，按指定类型修改并用对应引擎重新打开。以下结果覆盖引擎和候选文件接口。

| 平台 | 环境 | 复验命令 | 实际结果 |
| --- | --- | --- | --- |
| macOS 27.2.1 arm64 | rustc 1.100.0 | `cargo test --locked --manifest-path crates/kar-documents/Cargo.toml` | 9/9。包括 1 个段落单元测试和 4 组各 2 个文件集成测试。 |
| Ubuntu 24.04.5 aarch64 | Lima 内的 Linux 文件系统及 Rust 工具链 1.100.0 | 同上，在 Ubuntu 内重新编译并执行 | 9/9。独立生成、保存和重开所有测试文件。 |

| 验收对象 | 操作与期望 | 观察结果与 Rust 测试 |
| --- | --- | --- |
| Word 段落 | 文首插入一段，在含相同文字的段落间定位原对象，互换两次兼容修改的到达顺序，保留样式 | `tests/word.rs` 重开后定位正确，独立解析正文 XML 仍能找到加粗标记。同段的日期替换改变长度后仍能修改负责人。对同一范围的竞争替换得到基准、当前和提议值；删除或拆分目标段落触发冲突。 |
| Excel 单元格 | 在「预算」修改 B3 的值和填充，并修改 D3 公式。重排工作表、再在旧目标上方插行 | `tests/office.rs` 与 `tests/grid_history.rs`。重开并核对目标值、公式、样式和另一工作表的同地址单元格。输入结构变更记录后，B4 为 12，D4 公式为 `=B4*3`，结果为 36，新 B3 保留 99。删除目标列触发冲突。同一文件具有不明变更且未提供结构历史时返回冲突。 |
| PPT 对象 | 在两个同名对象中选一个，在重排幻灯片和图层后修改文字与位置。核对其他形状、媒体和关系 | `tests/office.rs` 检查指定形状的文字和横坐标，独立读取 ZIP 包内 XML、关系和图片字节。竞争写入同一坐标或目标已删除均返回冲突。 |
| 复合操作与候选文件 | 从基准和当前文件生成候选，重试已接受的修改，并在多项编辑中制造冲突 | `tests/candidates.rs` 从文件字节调用 `prepare`，两种兼容提交顺序得到相同的文档内容。等价重试返回无变更，复合操作冲突时返回结构化错误且未产生候选文件。 |

服务验收还需覆盖操作 ID 及冲突持久化、双客户端订阅与游标重连、显式解决、外部编辑器保存和原生工具整文件写回。上面的结果只登记对应的引擎断言。


## macOS 工作台协作入口（已实现，待统一验收）

`kar workbench` 的 `/projects/{p}/documents` 直接使用无界面文档服务。普通文件保存和整文件提议共用一个写入器、修订、冲突和游标。已登记的文档跨服务重启保留协作状态。工作文件通过工作区目录句柄读取与发布。

统一验收时运行 `cargo test --locked --test workbench_coediting`。用两个独立 HTTP 客户端和实际 Kar 进程核对 Markdown 普通保存、旧基准整文件合并、冲突、进程重启后的操作去重和冲突解决。各 Office 格式的实际文件样本、外部保存及双客户端订阅另运行 `cargo test --locked --manifest-path crates/kar-documents/Cargo.toml`。本次开发仅做 macOS 编译检查。


macOS 的原生文档入口已接入 Codex app-server。统一验收命令为 `cargo test --locked --test workbench_coediting actual_codex_merges_a_native_file_edit_with_a_concurrent_owner_save -- --ignored`。场景使用本机 Codex CLI、隔离的供应商配置和回环模型，由原生文件工具改写副本，同时在工作台保存人的修改。重启工作台后用原操作 ID 核对同一回执与工具调用次数。该场景已编译，尚待实际执行。写入策略与恢复读取参考 Codex 0.169.0 的 [thread/start 返回契约](https://github.com/openai/codex/blob/rust-v0.169.0/codex-rs/app-server-protocol/schema/json/v2/ThreadStartResponse.json) 和 [分页历史契约](https://github.com/openai/codex/blob/rust-v0.169.0/codex-rs/app-server-protocol/schema/json/v2/ThreadTurnsListParams.json)。


## macOS 设备与访客入口（待统一验收）

对照工作台固定提交 `2808e878de91aacc7fb82887f18414976be7ae34` 的配对、授权与访客服务。Kar 实现持久的一次性配对凭证、设备撤销、限时单聊天分享、访客快照及 SSE、附件权限和主人接纳/驳回。复验命令为 `cargo test --locked --test workbench_remote_access`，使用两个独立客户端、回环 HTTP、正常 Kar 工作台进程和原生协议替身。涵盖服务重启、凭证重用、跨聊天及跨访客拒绝、两次接纳只形成一个回合、过期和撤销活动订阅。本轮仅做编译检查。

跨设备场景在同一候选提交补验。需要第二台客户端和能访问宿主 Mac 的 SSH 连接。按 README 建立本地转发，从第二台设备执行一次配对、加入访客、订阅、提议、接纳和撤销，记录两端版本、连接方式、输入 ID、事件游标与撤销后结果。


## macOS Git 与托管平台（待统一验收）

主干已经接入仓库克隆与项目注册、状态和变更订阅、提交和推拉、独立工作树。PR 的基准/头提交用于逐文件差异，已审标记绑定头提交。PR 创建与 Issue 状态操作在远端写入前持久化调用 ID，未取得确定回执时支持对账。GitHub 的创建回执可用正文中的操作标记及分支对核实。

* `cargo test --locked --test workbench_forge` 使用真实本地 Git 裸库、回环 GitHub REST 替身、两个 Kar 进程，涵盖推送、PR/Issue 聊天绑定、差异、持久已审文件、较旧远端结果、限流及写入后丢回执。该场景是本地协议场景。
* `cargo test --locked --test workbench_forge -- hosted-github` 使用 `KAR_FORGE_TEST_REPO=owner/repository` 指定的可写测试库、`GH_TOKEN` 和已配置的 Git 凭据。通过工作台创建独立分支并推送，然后创建 PR、标记文件、按原请求 ID 重试，最后关闭此次创建的 PR 并输出远端地址。GitHub 会留下测试分支及已关闭 PR。该场景须在获准的专用仓库执行，尚未登记真实远端通过。
