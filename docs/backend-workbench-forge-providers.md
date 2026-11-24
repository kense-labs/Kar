# 工作台代码托管和问题跟踪驱动

本文件对应工作台验收项 `forge`。参考版本为 `2808e878de91aacc7fb82887f18414976be7ae34`，注册点在 `apps/server/src/sourceControl/sourceControlDefinitions.ts` 及各 `packages/source-control-*/src/server/driver.ts`。Linear 单独注册于 `packages/issue-tracker-linear`。Kar 以 Rust 实现 HTTP、GraphQL、仓库映射和回执。查询、写入、关联、重试和审阅采用同一工作台服务。

## 操作和固定源码定位

下表的 PR 操作为分页列出、按编号读取与创建，Issue 操作为分页列出、按编号读取、读取后绑定聊天及修改状态。远端账户、仓库、提交和对象号均在对应驱动返回的字段上核对。

| 驱动 | Kar 接入及账户条件 | 对照源码 | 可核对的原生字段或拒绝 |
| --- | --- | --- | --- |
| GitLab | GitLab REST v4，可用 `GITLAB_TOKEN`。gitlab.com 也可读取已经登录的 `glab` 令牌。支持 PR 和 Issue。 | `gitLabMergeRequestJson.ts`、`GitLabIssueTracker.ts` | 仓库 `path_with_namespace`，对象 `iid`，版本 `diff_refs`，Issue 状态 `state_event` |
| Forgejo | Forgejo REST v1，Codeberg 使用 `KAR_FORGEJO_TOKEN`，自托管在 host 配置指定 API 和凭据。支持 PR 和 Issue。 | `forgejoPullRequestJson.ts`、`ForgejoIssueTracker.ts` | `full_name`、`head.sha`、`merge_base` 或 `base.sha`，拒绝把 PR 编号当作普通 Issue |
| Bitbucket Cloud | REST 2.0，`KAR_BITBUCKET_TOKEN` 可作 Bearer，设置 `KAR_BITBUCKET_EMAIL` 后使用 Basic。支持 PR。 | `BitbucketApi.ts`、`bitbucketPullRequestJson.ts`、`driver.ts` | 仓库 `full_name`，PR 两端 `commit.hash`，跟随同源同路径的 `next` |
| GitCafe | git.cafe 和 staging.git.cafe 使用各自的 `/api`。生产可用 `CAFE_TOKEN`，也可读取 `cafe auth http credential` 对应 host 的登录。支持 PR。 | `GitCafeCredentials.ts`、`GitCafeSourceControlProvider.ts`、`gitCafePullRequestJson.ts` | `/auth/principal`，创建前查询源仓库 ID，`sourceBranch`、`headOid`、`observedBaseOid` |
| Cursor Origin | Git 远端为 origin.cursor.com，API 为 api.cursor.com/v1/origin。可用 `KAR_CURSOR_ORIGIN_TOKEN` 或 `origin auth token`。支持 PR。 | `OriginCli.ts`、`OriginSourceControlProvider.ts`、`originPullRequests.ts` | 以个人 namespace 核对账户，`nextPageToken` 翻页，接受原生字符串编号 |
| TanStack Source | 显式设置 `KAR_TANSTACK_SOURCE=1`，可用 `SRC_TOKEN` 或对应 host 的 `src auth token`。支持 PR 和 Issue。 | `TanStackSourceApi.ts`、`TanStackSourceSourceControlProvider.ts`、`TanStackSourceIssueTracker.ts` | 接受 `@owner/repo`，标准化 `owner:branch`，校验 `baseSha`、`headSha` 及 Issue 状态 |
| Azure DevOps | dev.azure.com、ssh.dev.azure.com 与旧 visualstudio.com 远端，REST 7.1 和项目作用域。可通过 host 配置指定 PAT，两个现代公共 host 默认读 `AZURE_DEVOPS_EXT_PAT`。支持 PR 和 Boards 工作项。 | `AzureDevOpsPullRequestProvider.ts`、`azureDevOpsPullRequestJson.ts`、`AzureDevOpsIssueTracker.ts` | 读取最新编号 iteration 的 `sourceRefCommit` 及 `commonRefCommit`。工作项须匹配 `System.TeamProject`，写入时校验修订号、工作项类型及其状态目录 |
| Linear | 在 Git 项目上单独按 UUID 绑定工作区与团队，使用 Linear API key。支持 Issue。 | `LinearApi.graphql`、`LinearIssueTracker.ts` | `organization.urlKey`、`team.key`、`issue.number`、团队工作流状态 ID，GraphQL 鉴权和限流错误 |

固定上游注册中，Bitbucket、GitCafe 与 Cursor Origin 只注册 source control 和 pull requests，Kar 对 Issue 操作返回 HTTP 422 和 `unsupported`。Linear 独立注册为 issue tracker。通过 Linear 绑定的项目仍能使用原 Git 远端的 PR 驱动。部分平台的 PR 列表响应省略基准提交等字段，打开单个 PR 时重新读取；供应商未返回可用的修订时，审阅路径会返回缺少提交身份的原因。GitCafe 的 `observedBaseOid`、TanStack Source 的 `headSha/baseSha` 和 Azure 的 iteration 都有此可能。

公共驱动使用系统配置的 token 环境变量，也支持 `KAR_FORGE_CONFIG` 中按主机配置的 `[[hosts]]`、`kind`、`api_base`、`token_env`。非公共 API 路径和自托管主机使用明确配置的凭据。Azure PAT、Forgejo、Linear 的当前实现使用配置令牌。上游供应商自己的合并、评论、审阅人管理等扩展操作可以从 `GET /forge/drivers` 内的声明查到；本页覆盖上述 Kar 查询和写入操作。

Linear 配置示例中，`project` 填注册后的项目 UUID，`workspace` 可钉住 Linear workspace URL key。配置中只保存环境变量名。

```toml
[[issue_trackers]]
project = "1221319a-acd0-416a-81fd-0145f05ff177"
workspace = "example"
team = "ENG"
token_env = "LINEAR_API_KEY"
```

`GET /projects/{project}/forge/issue-tracker` 返回团队、账户及状态。常规 Issue 查询、修改、重试、绑定与打开聊天依照同一配置选择团队。`GET /projects/{project}/forge/operations/{id}` 可以核对远端拒绝、限流或写入后的不确定结果。未确认的创建结果以标记在远端对账；同一操作 UUID 保留历史结果。

## 验收状态

| 平台、场景 | 操作和期望 | 命令与证据 |
| --- | --- | --- |
| macOS，纯 Rust 编译 | 工作台后端和八驱动协议验收程序通过类型及链接前检查 | `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo check --locked --all-targets`，开发分支已通过 |
| macOS，七个非 GitHub Git 驱动加 Linear 协议验收 | 启动真实 Kar 服务，创建临时 Git 仓库和聊天，访问本地 REST/GraphQL 对端。分别检查账户、PR 提交、同 UUID 的写入次数、Issue 状态，以及 403、429 或 422 | `cargo test --locked --test workbench_forge`。场景在 `tests/workbench_forge/providers.rs`，按当前开发节奏留待统一执行 |
| macOS，第三方托管平台实机 | 各平台需有可访问仓库或团队及其授权。在隔离测试项目注册远端并配置上表凭据，使用普通工作台入口运行上述场景；记录平台 API 版本、原生 ID、远端返回和本地操作 ID | 本批没有执行远端创建，没有第三方实机通过结论。已编译可复跑的协议对端和纯 Rust 驱动。GitHub 的托管账户用例另见 `tests/workbench_forge/hosted.rs` |

Linux、Windows 以及账户和权限实机组合本批均未运行。


## Standalone issue tracker accounts

`GET /forge/issue-trackers` probes the saved first Linear account and additional named accounts. Each status includes its scope directory and workspace. `POST /forge/issue-trackers/connect` accepts `tracker`, `token`, optional `account_id` and `add_account`. A credential is written after a successful native probe. Removing it with an explicit JSON null also clears the corresponding project bindings. A user-only, atomically replaced file under the Kar state directory holds the credentials and bindings together across restarts. The API endpoint is recorded with each credential. A host can set `KAR_LINEAR_API_BASE` to a dedicated GraphQL service before connecting.

`PUT /projects/{project}/forge/issue-tracker/binding` accepts a binding with a primary team, optional account ID and up to 250 additional teams. Sending a null binding clears the matching account's binding. An older client without an account ID cannot detach a named account. The optional `issue_scope` on issue reads, writes, reconciliation through the recorded operation, candidates, templates, links and new issue chats selects a bound Linear team. Static `KAR_FORGE_CONFIG.issue_trackers` entries remain a fallback for projects without a dynamic binding.

The macOS fixture command is `cargo test --locked --test workbench_issue_accounts`. It exercises two local test credentials, a rejected token, a duplicate credential, team pagination, scoped account routing, guarded unbinding, restart and cleanup at normal HTTP entry points. It is registered for centralized execution and has not been run against Linear.
