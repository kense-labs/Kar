# 多仓库工作目录与 Git 设置进度

固定工作台后端为 `2808e878de91aacc7fb82887f18414976be7ae34`。对应契约在 `packages/contracts/src/workspaceFolders.ts`、`packages/contracts/src/worktreeSetup.ts`，源实现位于 `apps/server/src/workspace/{WorkspaceFolders,TaskFolders,TaskFolderStore}.ts` 和 `apps/server/src/project/WorktreeSetupTracker.ts`。以下路径相对 `/projects/{project}`。

|操作|路径和结果|
|---|---|
|扫描和跟踪|GET `/folders/scan` 可指定 root 查询参数。优先读取按文件名排序的第一个有效候选 `.code-workspace`，支持注释和尾逗号，目录保留声明来源和名称；直接仓库和一级容器下的仓库标记 child。返回时排除重复、越权、父目录、自身和声明的子模块。GET `/folders` 读取实际仓库与显式目录，PUT 同一路径提交 `[{ "path":"...","name":"..." }]` 跟踪显式目录。项目 `kar.json` 的 ignoreRepositories 控制自动发现。|
|建立任务|PUT `/tasks/{id}/folder` 提交 directory、branch、可选 base 和 repositories。directory 是已授权父目录内此前不存在的目标。省略仓库列表时，最多八个仓库会全部选入，超过则需要主动选择。根据所有仓库的公共父目录保持相对路径。普通项目文件使用链接，选中仓库使用 Kar 持有的 Git 工作树。|
|查询及增补|GET `/tasks/{id}/folder` 读取持久记录；GET `/folders/containing?path=...` 支持任务子目录。POST `/tasks/{id}/folder/repositories` 提交 repository_path。已在任务内、未跟踪、与其他目录冲突的请求会被明确拒绝；原成员和占用目标的用户文件继续保留。|
|打开和恢复聊天|创建聊天时使用同一项目和任务 UUID，Kar 将任务的实际 project_path 保存为聊天 cwd。完成的任务、各仓库身份和 Kar 工作树记录都落盘。重启后读回工作区时重新核对物理路径和工作树标记；未完成的任务保持原创建参数，可继续完成。|
|进度与取消|GET `/tasks/{id}/setup/events` 订阅 SSE，收到初始快照或 null，再跟随阶段、百分比、状态和序号。POST `/tasks/{id}/setup/cancel` 对正在创建的 Git 工作树持久登记取消，终止 Kar 启动的 Git 作业及钩子，等待清理结果后回答。终态再次取消返回 false。实际未执行的 fetch、submodules、setup-script、agent 步骤在快照中标为 skipped。普通工作台 Git worktree_add 也以相同 UUID 提供设置进度；继续返回原 ManagedWorktree 结构。|

本轮 macOS 使用 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests` 编译。联合复验为 `cargo test --locked --test chat_restart -- workbench-task-folders`，清单既有 `cargo test --locked --test chat_restart -- workbench-projects` 同样会带上这组操作。场景实际启动 HTTP 工作台和 Git，检查扫描来源、两个独立仓库、重复及冲突拒绝、普通 Git 入口、阻塞 Git 钩子的取消和保留数据、重启后查找任务。聊天使用 Codex 协议替身，只核对 Kar 工作区绑定；该部分不计为供应商实机。另可运行 `cargo test --locked --lib workbench::task_folders::folders::tests::scan_merges_workspace_entries_and_repository_wrappers_with_scope -- --exact`。以上场景按统一验收安排，源码移植期间未执行。
