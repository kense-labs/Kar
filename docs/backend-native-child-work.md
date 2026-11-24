# 回合内的 Antigravity 原生子任务

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。对应操作为 `NativeChildWorkQuery.ts` 的子任务查询及 `AntigravityChildWork.ts` 的 `readAntigravityChildWork`。Kar 的入口为 `GET /projects/{project}/chats/{chat}/turns/{turn}/tools/{tool}/children`。工具 ID 可从该回合的 `GET .../turns/{turn}/items` 工具行取得。调用只读本机原生记录。

| 情形 | 归属依据及响应 |
| --- | --- |
| ACP 自动批准的旧版批次 | 用回合的原生根会话与工具的 `root:plannerIdx` 找 planner 的唯一 `invoke_subagent` ID，再匹配随后调用记录的 ID 与有序子项。 |
| ACP 请求批准的旧版批次 | 使用角色和提示全文核对唯一原生批次。排除其他自动批准工具已认领的批次。同文、缺记录、超限、不能排除其他批次时返回空数组。 |
| Antigravity 1.5 detached 子项 | 父会话列出的子代理中，只有子收件箱同时记录该父会话和这个启动调用且匹配唯一时才采用。随后读取该子代理自己的步骤及工具参数。 |
| 供应商交接、重启、继承历史 | 按回合保存 Kar 来源聊天、原生根及本次启动的配置目录。macOS 用目录设备号和 inode 识别配置实例。继承的回合回看原聊天的完整批次。旧录制若缺原生实例证据，返回空结果。 |
| 工具和文件边界 | 核对 ACP 标题、kind 和 MCP 标识。只读归属目录内的普通文件，逐层拒绝链接，并校验 SQLite 结果保存的子日志 URI。无法读取日志时仍返回已确认的子 ID 和角色，步骤为 null。 |

Kar 保留 Antigravity 原生默认 `antigravity-cli`，也识别启动指定的 `--app-data-dir antigravity-acp` 与 `--gemini-dir`。回合查询沿用该次启动的路径。不会扫描其他供应商或同机其他账号。

## macOS 复验

编译命令为 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests`。本批已编译。以下两个运行入口留给统一验收。

- `cargo test --locked --test chat_restart -- workbench-native-child-work` 使用真实 Kar SDK、工作台 HTTP 与 Git，搭配隔离的 ACP peer、SQLite/protobuf、子收件箱和步骤文件。核对重启时改传配置目录、切换到 Codex 后原生分叉继承历史、planner 与随机工具 ID、重复批次、同名 MCP、错误回合、重复 detached 收件人、链接日志。
- `cargo test --locked --test chat_restart -- workbench-native-child-work-live` 只读已经运行的 Kar 工作台中的真实回合。需要回环地址 `KAR_CHILD_WORK_URL`、`KAR_WORKBENCH_TOKEN`、`KAR_CHILD_WORK_PROJECT`、`KAR_CHILD_WORK_CHAT`、`KAR_CHILD_WORK_TURN`、`KAR_CHILD_WORK_TOOL` 和 `KAR_CHILD_WORK_EXPECTED`（用逗号按原生顺序列出子会话 ID）。该回合须由记录实例归属的 Kar 版本创建，至少有一个子会话保留了步骤。它核对 HTTP 读回的全部子会话身份与步骤是否存在，不发送消息。

SQLite 查询先读表叶页上的列类型和长度，确定候选步骤后才读取该步的 protobuf 字节与溢出页。单步上限 1 MiB，候选调用最多 200 个，调用字节合计上限 8 MiB。库级复验用 `cargo test --locked --lib unrelated_overflow_is_never_followed_and_bounded_blobs_cross_pages_intact`，核对无关大字段的损坏溢出链不会影响选中小字段的完整读取。
