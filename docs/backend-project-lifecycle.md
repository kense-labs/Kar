# Kar 项目生命周期

工作台固定提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。项目变更对照 `apps/server/src/project/ProjectMutation.ts`、`ProjectService.ts` 及 `packages/contracts/src/project.ts`。暂存目录、新项目及就地晋升对照同目录的 `ManagedProjectFolders.ts` 和 `ScratchThreadPromotion.ts`。

| Kar 入口 | 操作与期望 |
| --- | --- |
| `POST /projects/mutate` | `type` 取 `project.create`、`project.update` 或 `project.delete`。携带 `commandId` 和 `projectId`。同一命令重取原回执，同 ID 改内容返回错误。标题、物理根、设置、脚本、固定、归档、跟踪目录及增删同事务提交。归档可恢复。 |
| `POST /projects/ensure-scratch` | 返回带 `id` 和 `root` 的 Scratch 项目。重复取得时 ID 相同。从 Scratch 创建的独立聊天各有单独目录，同目录分叉沿用该目录。 |
| `POST /projects/create-new` | 发送 `{"name":"My App"}`。在临时目录写 README 和图标、初始化 Git 并尝试首次提交。macOS 以禁止覆盖的原子改名发布，重名自动选择后缀。提交失败时仍返回项目和 `commit_error`，保留暂存文件。 |
| `POST /projects/create-from-thread` | 发送 `{"chat":"<uuid>"}`。同物理目录里所有 Scratch 聊天连同已归档分叉，以同一目录事务晋升。未完成回合、队列、原生请求或正在修改的会话会阻止晋升。 |
| `GET /projects/{id}/chats`、`POST /projects/{id}/chats` | 项目改根后新聊天用新根，老聊天保留原目录、原生 ID 和历史。任务工作树也从相同的已存归属识别。重启的队列扫描不依赖客户端先打开会话。 |
| `project.delete` | 有聊天时须明确给出 `force: true`。删除工作台入口并关闭会话。文件和聊天日志保留。再次注册该路径获得新项目 ID，删除项目的聊天不会出现在新项目。 |

Kar 在第一个工作台授权根下创建 `Kar/scratch` 和 `Kar/projects`。该位置不依赖私有聊天存储的父目录。原生会话、工作树标识和每聊天的实际目录保存在聊天日志；工作台项目归属及命令回执保存在 `workbench/projects.json`。历史日志在项目改址或删除前按原根和已登记工作树收编。Kar 工作流定义以项目为所有者，工作流步骤运行创建的聊天保留运行记录中的原始 ID。

## 验收

macOS 编译命令为 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests`。集中复验运行 `cargo test --locked --test chat_restart -- workbench-projects`。克隆中的项目删除另由 `cargo test --locked --test workbench_forge` 内的 Git smudge 阻塞场景覆盖，核对取消后无法发布、重复删除及用户已有文件保留。

其中 `tests/support/project_lifecycle.rs` 走独立 Kar HTTP 服务，检查带重试的目录迁移、重启后无人打开页面时自动投递且只发一次、Scratch 目录隔离、同目录归档分叉就地晋升、Git 初次提交成功与失败回执及安全删除后的文件和重新注册可见性。Codex stdio 对端为协议替身。该用例尚待执行。
