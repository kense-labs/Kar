# 四个 ACP 模型菜单

对照上游运行时固定提交 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 中各供应商的 `list-models.ts`。公开入口为 Rust 的 `Runtime::form_model_listing`。各查询在调用者的工作区和环境中发送 `initialize`、`session/new`，客户端声明文件、终端能力关闭。每次查询有一个总时限。用于查询的进程由 Kar 管理，查询全程不发送模型回合。

|供应商与来源|操作和期望|
|---|---|
|Devin，`tests/devin/list-models.test.ts`，上游示例版本 3000.12.57|运行 `devin acp`，仅当前模型携带默认档位和可用档位。优先删除临时会话，原生只支持关闭时改用关闭。菜单存在但为空时运行原生账号读取器。可确认的已登录空目录为 empty，未登录为 unauthenticated，有凭据但无法确认时为 unsupported，其余读取错误为 failed。|
|CodeBuddy，`runtimes/codebuddy/list-models.ts`，2.178.1|运行 `codebuddy --acp --permission-mode bypassPermissions`，只读登录状态已有的配置。将 enabled 和 disabled 从推理档位中移除，默认档位必须在当前模型实际报告的菜单内。|
|Kimi，`tests/kimi/kimi-list-models.test.ts`，上游测试 0.41.0 和 Kimi 2.x 配置形状|运行 `kimi acp`。服务端提供 login 时才调用 authenticate，读出菜单后尽力关闭临时会话。按 thought_level 分类寻找推理项，支持字符串和原生对象值。忽略 off。原生缺接口为 unsupported，要求认证为 unauthenticated。聊天仍按 Kar 原有 KAP/ACP 握手打开。|
|Antigravity，`runtimes/antigravity/model-menus.ts` 和 `tests/antigravity/antigravity-list-models.test.ts`，1.5.0|检查稳定版 agy 至少为 1.5.0。读取全量旧目录和当前模型菜单，只对可以还原的临时会话逐个切换模型，核对它亲自返回的档位，并确认恢复初始模型和档位。旧目录有歧义时保留原始选择器并给出原因。首个切换方法缺失则用初始目录，后续个别失败仅标记对应模型。无法确认恢复时整次读目录为 failed。|

macOS 编译使用 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests`。联合验收可分别运行 `cargo test --locked --test protocol -- acp-picker` 和 `cargo test --locked --test protocol -- antigravity-picker`。两条都是原生协议替身，检查启动参数、请求序列、清理和模型结果。真实供应商账号、版本与平台证据由联合实机验收登记。
