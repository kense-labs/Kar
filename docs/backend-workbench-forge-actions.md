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
