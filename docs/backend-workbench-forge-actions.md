# 托管 Issue 和 PR 审阅

对照工作台提交 `2808e878de91aacc7fb82887f18414976be7ae34` 的 `packages/contracts/src/{issue,pullRequest}.ts`、`apps/server/src/ws.ts`、`apps/server/src/pullRequest/PullRequestService.ts` 和各驱动的 `src/server` 实现。Kar 使用纯 Rust，将公开动作接到现有工作台项目、聊天和操作记录。

`POST /projects/{project}/forge/operations` 需要 `chat` 和稳定的 `id`。同一个 UUID 和请求可以取回已经保存的回执。写到一半，或托管平台未返回可确认的回执时，操作保留待对账状态；重复同一个 UUID 继续返回原状态。评论和 Issue 的 Markdown 保留原文。每次读取评论或线程都从托管平台获取。

| Kar 操作 | 输入及回读 |
| --- | --- |
| `create_issue` | `title`、`body`、`labels`、`assignees`，返回所属项目或团队中的编号、显示编号和链接。 |
| `comment_issue` / `issue_comments` | 写入带 `number`、`body` 的评论，随后在 `/forge/query` 按 `number` 和可选 `cursor` 查询作者、ID 和正文，`nextCursor` 用于续页。 |
| `submit_review` | `number`、`verdict`（`approve`、`comment`、`request-changes`）、`body`、`comments`。行评论支持 `path`、`oldPath`、`position`，支持行区间的驱动另接受 `startPosition`。位置分别为 `{"kind":"added","newLine":4}`、`{"kind":"deleted","oldLine":1}`、`{"kind":"context","oldLine":1,"newLine":4,"side":"right"}`。 |
| `reply_to_thread` / `set_thread_resolution` | 指定 `number`、`thread_id`，请求分别带 `body` 或 `resolved`。 |
| `review_threads` / `thread_comments` | 在 `/forge/query` 指定 `number` 读取远端线程、解决状态、评论 ID、作者与正文。GitHub 的线程列表和单个线程的评论有各自的续页游标。 |

| 驱动 | Issue 及评论 | PR 审阅及线程 | 固定源码依据 |
| --- | --- | --- | --- |
| GitHub | 创建、评论、分页；写前确认编号指向 Issue | REST 提交及区间行评；GraphQL 线程回复、解决和重开。写线程前核对所属仓库、PR 和账号权限。 | `source-control-github/{GitHubIssueTracker,GitHubPullRequestApi}`、`pullRequestMutations.graphql` |
| GitLab | 创建前将 assignee 名称换成 ID；排除系统 notes | MR discussions 支持行和区间、回复及解决。请求修改先加自己为 reviewer，再提交 verdict；批准在每条评论成功后发出。 | `source-control-gitlab/{GitLabIssueTracker,GitLabPullRequestCli}` |
| Forgejo | 标签名换成数字 ID，评论在本地切页 | 支持三种 verdict 和单行评论。固定驱动的 `replyToThread` 和 `setThreadResolution` 均返回 unsupported。 | `source-control-forgejo/{ForgejoIssueTracker,ForgejoPullRequestProvider}` |
| TanStack Source | 创建后写独立的标签和指派路由，保留部分成功的编号；跳过隐藏的评论 | 行评固定到 `headSha`，支持区间、回复和解决。 | `source-control-tanstack-source/{TanStackSourceIssueTracker,TanStackSourcePullRequestProvider}` |
| Azure DevOps | 按项目选可用工单类型，请求工单前确认所属项目，原生评论按 token 续页。最多一个指派人。 | 固定驱动显式拒绝提交、回复和解决，返回空的行评线程列表。 | `source-control-azure-devops/{AzureDevOpsIssueTracker,AzureDevOpsPullRequestProvider}` |
| Linear | 工作区及团队绑定；验证 label 和单一 assignee，查询真实 GraphQL comment connection。 | 固定注册为 Issue tracker。 | `issue-tracker-linear/{LinearIssueTracker,LinearApi.graphql}` |
| Bitbucket | 固定注册无 Issue tracker。 | 先发单行评与评论，再发批准或请求修改；以根评论编号回复、解决及重开。 | `source-control-bitbucket/BitbucketPullRequestApi` |
| GitCafe | 固定注册无 Issue tracker。 | 提交检查 head 和版本，源码不支持在 review 中创建行评；现有原生线程支持回复、携带根评论版本的解决和重开。 | `source-control-gitcafe/GitCafePullRequestProvider` |
| Cursor Origin | 固定注册无 Issue tracker。 | 提交、单行评、回复、解决及重开；操作仓库范围的 thread ID 前核对 PR 归属。 | `source-control-cursor-origin/OriginPullRequestProvider` |

## macOS 统一验收

| 场景 | 命令 | 待观察的证据 |
| --- | --- | --- |
| 离线 GitHub 协议与真实 Kar HTTP | `cargo test --locked --test workbench_forge` | 真实项目/聊天，检查唯一 UUID、Markdown 空白、作者、对象 ID、权限拒绝、复读和线程解决状态。 |
| 单个选定平台的 Issue | `cargo test --locked --test workbench_forge -- hosted-issue-actions` | 在指定仓库或团队创建带唯一标记的 Issue；发布及重试同一条评论，回读核对仅一次，再用指定的关闭状态关闭本次创建的 Issue。 |
| 单个选定平台的 PR | `cargo test --locked --test workbench_forge -- hosted-review-actions` | 在指定的专用 PR 提交带唯一标记的审阅、回复一句，解决再重开，反复回读作者、正文和 ID。 |

两个外部场景都要提供 `KAR_FORGE_ACTION_DRIVER`、`KAR_FORGE_ACTION_REMOTE`、`KAR_FORGE_ACTION_SCOPE`（身份接口返回的 `host/repository`，Linear 为 `host/workspace/team`）、`KAR_FORGE_ACTION_EXPECT`（`success`、`permission`、`unsupported`）。程序先核实驱动与身份，再向该目标写入。平台凭据与普通工作台相同。可显式提供 `KAR_FORGE_ACTION_CONFIG` 配置模板，Linear 的项目绑定写 `project="PROJECT_UUID"`，仅在临时配置中替换。TanStack 需要 `KAR_TANSTACK_SOURCE=1`。

Issue 成功场景还需 `KAR_FORGE_ACTION_CLOSE_STATE`（例如 GitHub 的 `closed`、Linear 的本团队状态）。PR 场景需要 `KAR_FORGE_ACTION_PR`，成功场景使用专用测试 PR 的 `KAR_FORGE_ACTION_PATH` 和新增行号 `KAR_FORGE_ACTION_LINE`。GitCafe 另选已有、未解决的专用线程 `KAR_FORGE_ACTION_THREAD`，它只能把回复关联到已有线程。每次实测会保留远端带标记的 Issue / 审阅 / 评论。Forgejo 单测其源端明确拒绝的两个动作。只读验证需明确提供实际只读账号及对应的 `permission` 预期。

当前证据为固定源码对照及 macOS Rust 全目标编译。离线与外部场景都已登记，实际作者、ID、结果和时间等待统一执行后记录。供应商未支持的操作见上表；缺少授权的专用仓库/团队、具备权限的账号或匹配行的 PR 属于实测条件不足。

## Git 发布目标与分叉 PR

组合操作通过 `POST /projects/{project}/git/actions` 接收 `create_pr`、`commit_push_pr`、`push`。发布先读取当前分支的 `pushRemote`，其次 `remote.pushDefault`。跟踪同一头分支时保留拉取远端。以另一个分支为比较基线时，Kar 用 `branch.<name>.kar-merge-base` 和 `kar-merge-base-ref` 记录旧基线，再将新分支设为跟踪目标。结果里 `branch` 是本地分支，`published_branch` 和 `upstream_branch` 标明实际发布位置。多个推送 URL 指向不同托管仓库时拒绝创建 PR。

PR 在主仓库中查询和创建，匹配源仓库和源分支。同名的外部 PR 不会被误认。GitHub 发送 owner、分支及源仓库名，GitLab 从源项目创建并传递目标项目 ID，Bitbucket 传完整源仓库名，GitCafe 读取源仓库 ID，Forgejo 发送 owner:branch。TanStack Source 的固定驱动明确拒绝跨仓库创建。Cursor Origin 和 Azure 固定创建路径只接收分支名，Kar 在跨仓库请求下返回 unsupported。此限制取自固定驱动的请求契约。

固定工作台源码 `apps/server/src/vcs/GitVcsDriverCore.ts` 的 `resolvePushDestination`、`pushCurrentBranch` 和 `apps/server/src/git/GitManager.ts` 的 `resolveBranchHeadContext`、`openPrFromBranch`。macOS 统一复验命令为 `cargo test --locked --test workbench_forge`。用例在本地裸仓检查跟踪配置、分叉删除后的重新发布、异仓同名 PR、同名头和基线与各驱动的 HTTP 请求。它使用协议 fixture；真实托管请求仍待统一验收。

## 发布仓库

`POST /projects/{project}/forge/publish` 在已登记的工作树发布当前分支。请求包含 UUID `id`、`provider`、`repository` 和 `visibility`（public 或 private），可指定 `host`、`remote_name`、`protocol`（auto、https 或 ssh）。公开与私有仓库均由主机所有者通过工作台凭据创建。GitHub 接受个人仓库名称或组织/名称。GitLab 支持嵌套命名空间；Azure 使用明确的组织、项目和仓库路径，其项目必须具有请求的可见性。

创建返回的仓库身份和地址会记入 Kar 状态。空仓返回 `remote_added`，已有提交成功推送后返回 `pushed`。名称冲突时保留原远端，选择可用名称。auto 首选 SSH，发生认证或主机密钥拒绝时改用仓库提供的 HTTPS 地址。若推送失败，修复 Git 访问条件后可用同 UUID 重试；已确认创建的远端不会被重复创建。创建请求回执不确定时，Kar 返回 `needs_reconciliation`，仓库可用 `POST /forge/repositories/lookup` 核实。

GitCafe 的创建需等待 admission；Cursor Origin 仅允许私有仓库，先创建、再修改可见性，若修改失败会报告仓库可能仍对团队可见。macOS 的 `cargo test --locked --test workbench_forge` 覆盖八种驱动的 HTTP 请求、空仓、独立本地 Git 裸仓、失去创建回执，以及远端创建完成后重启续推。上述场景已编译，统一验收和实际托管发布另行记录。

## GitHub App 登录

Kar 使用主机配置的 `KAR_GITHUB_APP_CLIENT_ID` 启动自己的 GitHub App 设备码流程。四个 POST 入口为 `/forge/sign-in/start`、`/forge/sign-in/wait`、`/forge/sign-in/cancel` 和 `/forge/app-access`。输入包含 `provider: github` 和 `host: github.com`，安装范围查询另加 `repository`。启动返回用户码、验证链接和毫秒过期时间。等待返回 GitHub 登录账号，或明确的 not-started、cancelled、denied、expired、failed、unsupported 错误。

Kar 将已授权的应用会话保存在工作台私有状态目录下的用户专用文件，绑定应用客户端与 API 地址。未完成的设备码轮询随进程结束；已授权的会话可重启读取。即将到期或被 GitHub 拒绝的令牌由主机串行刷新。环境凭据优先；独立的安装范围查询始终使用应用令牌。上游在安装查询暂时失败时对 UI 返回 covered，Kar 保留这一显示行为，实际调用仍由 GitHub 核验。

`cargo test --locked --test workbench_forge` 的本地设备协议场景覆盖取消、拒绝、过期、pending、slow_down、授权账号、安装范围、重启使用、401 后刷新。已做 macOS 编译检查，尚待统一实跑。实际授权需要启用设备流程的 Kar GitHub App 和交互式用户批准。

克隆的普通入口和项目克隆任务共用同一传输。SSH 认证或主机密钥被拒时可自动尝试 HTTPS，显式 ssh 请求保留原错误。每次尝试写临时目录，完整后才安装到目标。直接 HTTPS 地址中的用户名、密码与查询只留在当前主机进程；HTTP 快照、项目目录记录及克隆配置中的 origin 使用脱敏 URL。若携带临时凭据的任务在进程重启后仍需认证，可用 Git 凭据助手重试或对同目标重新提供认证地址。


## Editing an issue

Send `update_issue` to the ordinary scoped operation endpoint with a stable operation ID, chat ID, issue number and at least one of `title`, `body`, `labels` or `assignees`. An omitted property keeps its current remote value. Empty body and empty metadata arrays clear those fields. Kar verifies issue ownership before sending writes. GitHub metadata requires the returned label and assignee sets to match. Forgejo resolves every label before its first write. Azure uses a revision test and the item's native description or repro field. Reconciliation reads the remote issue and certifies only a matching field set. Partial multi-request edits and missing native receipts remain visible.

`cargo test --locked --test workbench_forge` includes updating and reusing an operation ID and a GitHub peer that silently drops labels. The fixture and installed-provider scenarios await unified execution.

### Issue and comment reactions

Send `set_issue_reaction` to the project's forge operations endpoint with a stable UUID, `chat`, `number`, optional `subject_id`, `content` (e.g. `thumbs-up`, `heart`) and `reacted`. An absent subject selects the issue. The operation reads the authenticated viewer's state before acting. GitHub and Linear prove global comment membership against the selected issue. GitLab and Azure Boards scope comment lookup by issue. Forgejo verifies comment IDs in the selected issue's comment pages before accessing the repository-wide comment reaction endpoint. Linear uses the bound account/team. Azure Boards supports six reactions on comments. TanStack Source, Bitbucket, GitCafe and Cursor Origin expose no issue reaction operation at the fixed revision.

A missing write receipt can be reconciled by reading the same viewer, issue, subject and reaction. If the current state differs, the operation stays uncertain. The local GitHub protocol scenario in `cargo test --locked --test workbench_forge` covers foreign comments, duplicate request UUIDs and repeated desired state under a fresh UUID. The five native drivers and this scenario await the unified run.

### 跨项目的关联聊天

`GET /forge/linked-chats?kind=issue&number=7&repository=github%3A%2F%2Fgithub.com%2Fteam%2Frepo` 使用绑定返回的仓库标识；PR 传 `kind=pull`。读取已登记且属于工作台授权目录的项目，按聊天去重，最近活动在先、同一毫秒按聊天 ID 排序；数据都来自本地持久化目录。聊天标题复用列表的重命名和首条用户消息规则。返回 `archivedAt`、`archived` 和 `settled`。旧版 Kar 仅记录归档布尔值，因此旧归档保留 `archived=true` 和 `archivedAt=null`。

`cargo test --locked --test workbench_forge` 新增两项目关联同一 Issue、重复链接、相邻类型和另一 host 隔离、归档时间以及重启后顺序稳定的 HTTP 场景。待集中运行。

### 多项目、多账号 Issue 搜索

`POST /forge/issues/list` 接受固定契约的 `state`、`involvement`、`projectId`、`excludeProjectIds`、`host`、`limit`、`query`、`labels`、`assignee`、`sort`、`refresh` 和 `cursors`。每次读取原生页面。首次请求发现受授权项目的 Git 主远端、上游远端和已绑定的 Linear 团队。同一账号、host 和 scope 只读一次。续页把返回的 `nextCursors` 作为 `cursors`，每个失败的续页保留原游标。最终 entries 按更新时间跨范围排序。指定参与人筛选时按账号查询 viewer。

| 驱动 | 原生范围与分页 |
| --- | --- |
| GitHub | GraphQL Issue search 以仓库及状态限定，文本逐词引用，支持游标、全部标签和三种参与人。 |
| GitLab | 项目 REST Issue 页支持 assignee、author、搜索和标签。固定驱动对 mentioned 返回宽集合。 |
| Forgejo | 专用 Issue 过滤、标签取交集、按 Link 决定下一页。 |
| Azure Boards | 以项目为界，按流程类别解析关闭状态。WIQL 使用日期和编号作游标，再用 workitemsbatch 读取并恢复原顺序。mentioned 沿用固定宽集合。 |
| TanStack Source | 合并原生开放/关闭流，按创建日期排序并去重，续页保留尚未交付的原生页。过滤隐藏行和多标签，创建日期兼作更新日期。 |
| Linear | 原生 IssueFilter 和命名账号的 team 绑定，支持参与人、每个标签、文字和原生 ENG-7 式编号。续页限定账号及团队。 |

集中复验使用 `cargo test --locked --test workbench_forge --test workbench_issue_accounts`，覆盖六种协议、同范围多个项目和同账号多个团队。当前为 macOS 编译证据，上述场景及真实托管账号尚待执行。

### GitHub Issue 详情与评论

`GET /projects/{project}/forge/issues/{n}/detail` 与 `GET /projects/{project}/forge/issues/{n}/comments?cursor=...` 返回富文本数据所需的 Markdown 正文、结构化作者、反应及权限。项目内选定已登记的 tracker、host 和 scope 才可标为可写，同一 host 的其他 repository 显示为只读。GitHub 驱动读完标签、受理人、关闭引用的所有后续页，再合并时间线里的引用。同一个 PR 同时引用并关闭 Issue 时保留关闭关系。评论游标向较早的记录翻页，反应计数包括自己，展示的用户列表剔除自己。复验入口为 `cargo test --locked --test workbench_forge`，当前仅完成 macOS 编译。

GitHub 状态操作接受 Open、Completed、Not planned 和 Duplicate，并在写回及对账时核对远端编号、state 和 state_reason。对已关闭 Issue 改变关闭原因会返回明确错误。GitLab、Forgejo 和 TanStack Source 接受详情展示的 Open 与 Closed。

Linear 的相同详情和评论入口验证 workspace、team 和原生编号，按状态类型及 position 排出工作流并识别 duplicate 关闭项，显示状态迁移、标签、标题和责任人事件，解析 GitHub/GitLab 附件里的 PR 标识。TanStack Source 解析数字时间戳，排除隐藏评论，返回按登录及锁定状态计算的权限、fixedBy 改动和较早时间线。两个 HTTP 场景分别位于 workbench_issue_accounts 和 workbench_forge，尚待集中运行。

Forgejo 的扩展工单入口通过原生 timeline 读取事件和跨仓库 PR 引用，按引用仓库和编号去重。评论从原生整表每次取 50 项，再并行读取本页各条评论的反应。写权限同时取决于仓库归档、push 权限、作者和工单锁定状态。macOS 协议场景位于 `tests/workbench_forge/providers.rs`，覆盖锁定工单、51 条评论、两位反应者和重复的跨仓引用。统一执行命令为 `cargo test --locked --test workbench_forge`，本批仅编译。

Azure DevOps 从工单类型读取可选状态和自定义完成态，按所选项目校验工单编号，读取最多五页原生 revisions，去重解析最多十个关联 PR。评论从最新一页向前翻，每页按时间正序展示；保留六种原生计数及当前账号是否已反应，Azure 未返回反应者名单。正常 HTTP 场景覆盖复现步骤字段、自定义完成态、修订生成的五类事件、评论续页及跨项目拒绝，命令为 `cargo test --locked --test workbench_forge`。待统一执行与托管账号验证。

GitLab 工单详情用 `iids[]` 读取带颜色的标签，用独立的 activity 分页还原系统事件，解析自托管子路径下的引用，并优先保留有关闭关系的关联 MR。评论的 REST 页号与 GraphQL awardEmoji 游标随同传递，短页按 X-Next-Page 续读。旧版本明确拒绝 awardEmoji 字段时保留评论，权限拒绝保留为错误。正常 HTTP 场景覆盖上述边界，统一执行 `cargo test --locked --test workbench_forge`；本批证据为 macOS 全目标编译，托管实机待验。

PR 文件全文可由 `POST /projects/{project}/forge/pulls/{n}/diff/contents` 读取。请求含 changeType、oldPath、newPath 以及可选 commit。GitHub 优先读比较的 merge base，GitLab 使用 MR 的 diff_refs，Forgejo 能从 fork 解码 base64。GitCafe 同时核对快照版本、提交及文件名；TanStack Source 采用文件列表的头与合并基线；Azure 采用最新合法 iteration 的 commonRef 和 sourceRef。新建和删除只读存在的一侧。GitHub/GitLab 对原始文件设 1 MiB 上限，并校验 UTF-8 和 NUL。Azure 对指定的较旧提交返回拒绝，固定实现只能提供最新 iteration 的比较；GitCafe 与 TanStack Source 的固定实现拒绝单提交。Bitbucket 和 Cursor Origin 的固定提供方没有该读取方法。

同一组协议场景覆盖六种托管服务的重命名、带空格及特殊字符的文件名、GitHub 初始提交与二进制拒绝、fork 源仓库、两种未实现驱动的原生能力边界。统一执行 `cargo test --locked --test workbench_forge`，本轮仅做 macOS 全目标编译。

托管和关联聊天的刷新修订号随工作台目录持久化。两个客户端分别订阅 `/forge/refreshes/pulls`、`/forge/refreshes/linked-chats`，通过 Last-Event-ID 读取更新后的最新修订，多个刷新允许合并。仅已阅标记失效不会广播全局 PR 刷新。`POST /forge/pulls/report-state` 保存客户端观察，按项目、主机、仓库与 PR 在五分钟内去重，最多保存 2048 个对象；已绑定 PR 另外向托管方回读。客户端报告不会覆盖已验证的 PR 数据。协议测试在 `tests/workbench_forge/signals.rs`，命令为 `cargo test --locked --test workbench_forge`，双客户端、断线、服务重启及重复报告等待集中执行。

文件已阅入口为 `GET/PUT /projects/{project}/forge/pulls/{n}/files-viewed`，单批最多 500 个文件。GitHub 按当前账号读原生 viewed、unviewed、dismissed，每 100 个 toggle 一组有序提交。GitLab 从 MR 头修订查询文件 blob，Azure 读取最新迭代的变更对象，Forgejo 和 Bitbucket 从 Git patch 的 index 头解析修改后 blob。后四者按托管账号、主机、仓库及 PR 保存标记，优先记住用户实际读到的版本；取不到当前修订时保留已阅。缺少原生 viewedFiles 能力的三个驱动返回 unsupported。旧的 reviewed 接口保留原有按 PR 头确认的行为，旧数据没有托管账号可核实。

正常 HTTP 场景 `tests/workbench_forge/viewed.rs` 包括 GitHub 分页和同路径开关顺序、四种文件版本来源、跨账号隔离、文件删除，以及 Bitbucket 跟随仓库内 diff 重定向。统一运行 `cargo test --locked --test workbench_forge`；开发期仅在 macOS 编译。
