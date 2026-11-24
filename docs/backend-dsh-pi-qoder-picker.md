# DSH、Pi、Qoder 工作区模型菜单

固定上游运行时提交为 `2b15859a13dcc50e8b66cbaee5a007832aa00e08`。对照各运行时的 `list-models.ts`、Pi 的 `model-projection.ts`、DSH 的 `bridge.ts`、`tests/dsh/list-models.test.ts`、`tests/pi/pi-list-models.test.ts` 及 `tests/qoder/{models,provider-models}.test.ts`。Kar 的 CLI 入口为 `kar models <runtime> --picker --cwd <project>`，工作台使用同一份模型结果。供应商边界由 Rust 控制进程或 Rust Node-API 模块调用。

| 读取 | 选择器、默认值和推理强度 | 空、认证和错误 |
| --- | --- | --- |
| DSH 自 0.2.1-rc.5 起，在短生命周期的 ACP 配置中读取 Cordis 的 llm 服务，查询 providers、models、resolveModelInfo 及原生默认路由。 | ID 为 `供应商/模型`，以同名供应商和模型确认默认。推理模型使用服务提供的 effort ID 和 defaultEffort。 | 其他供应商有结果时仍返回 live。空目录且默认供应商成功返回零条时为 unauthenticated；供应商都没列出且带失败记录时为 unsupported；未配置默认且无故障的空表为 empty。 |
| Pi 通过匹配的已安装 SDK 构造工作区 SettingsManager、createAgentSessionServices，调用实际 getAvailable。加载项目扩展，跳过技能、提示、主题和上下文文件；配置的包先通过原生解析器检查。 | ID 为 `供应商/模型`。只标记工作区配置中明确保存且可用的默认项。推理档位来自 pi-ai 的 getSupportedThinkingLevels，不另设 defaultEffort。 | 原始模型表非空但可用项为零时为 unauthenticated；两者均为空是 empty。缺匹配 SDK、所需方法或必须安装的包为 unsupported。 |
| Qoder 自 1.1.78 起，先用同一环境读原生认证状态，随后仅发 initialize 和实时 get_models 控制。1.1.81 起遵守 daemon v2 初始化与会话路由。 | 始终使用 value 作 ID。旧版只认 serverModel.display_name；1.1.82 起仅在 modelId 匹配且名字和选择器均唯一时提供 resolvedId。保留禁用状态、isDefault、原生 efforts 及 defaultEffort。 | 明确 logged_out 直接返回 unauthenticated。未知认证状态仍查询原生目录，空表为 empty，控制失败为 failed，版本不支持为 unsupported。 |

公开 API 的调用方提供了 bundled 项时会保留精确原因并返回 fallback。查询逾时保留 timeout，不启动模型回合。Pi 的可用性由本地配置、提供的扩展及认证决定，真实 OAuth 配置可能进行原生令牌刷新。

## macOS 统一复验

| 路径 | 命令和预期 |
| --- | --- |
| DSH 真实 CLI，独立配置和回环模型 | 设置 `KAR_DSH_BIN`，执行 `cargo test --locked --test dsh_live model_picker_reads_the_native_route_without_a_turn -- --exact --ignored`；核对模型路由、工作区文件和模型请求计数。 |
| Pi 真实 SDK，本地配置的合成模型 | 设置 `KAR_PI_BIN`，必要时提供同版本 `KAR_PI_SDK_DIR`；执行 `cargo test --locked --test pi_provider_live model_picker_reads_project_default_and_actual_available_models -- --exact --ignored`。核对无凭据分类、项目覆盖、模型可用性和 SDK 推理菜单。 |
| Qoder 真实 CLI，未登录配置 | 设置 `KAR_QODER_BIN`，执行 `cargo test --locked --test qoder_live model_picker_stops_after_native_logged_out_status -- --exact --ignored`。工作区保持为空。 |
| Qoder 真实 CLI 与已登录账号 | 设置 `KAR_QODER_BIN`、`KAR_QODER_MODEL_LIVE=1`，执行 `cargo test --locked --test qoder_live authenticated_model_picker_lists_selectors_without_a_turn -- --exact --ignored`。需要有效账号及服务连接，核对实时选择器和安全字段。 |
| 静态投影断言 | `cargo test --locked --lib picker`。覆盖同名选择器、禁用原因、默认项、reasoning 菜单及空与认证分类。 |

上述四个具名实机场景已登记到统一 macOS runner。当前 macOS Rust 全目标编译通过，场景等待集中执行。
