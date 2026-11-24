# OpenCode 无会话模型发现

固定上游运行时提交为 `2b15859a13dcc50e8b66cbaee5a007832aa00e08`。版本分路和投影对照 `packages/oar/src/runtimes/opencode/list-models.ts`，范围环境和模型顺序对照 `tests/opencode/opencode-list-models.test.ts`。Kar 普通 `models` 命令和 `Runtime::form_model_listing` 都通过所选的原生可执行文件查询。版本与目录查询使用调用者的 cwd 和环境，并共享总时限。

| 版本和操作 | 期望 |
| --- | --- |
| 1.x，运行 `opencode models --verbose` | 读取 provider/model 及随后的 JSON 块，保留显示名、供应商 ID 和原生 variants 键顺序；在单独的 ID 行或提示日志上不生成模型。 |
| 2.x，运行 `opencode models --standalone` | 每行合法 provider/model 成为一个选择项。空输出返回 unsupported 原因；2.0.26 的冷目录可能尚未加载。非法行、命令失败和超时分别经过错误处理。 |
| Rust 表单 API | 成功时显示实时结果。无法实时获取时，表单保留传入的内置选择并标明来源与原因；没有内置选择时返回不可用状态。 |

本轮 macOS 已执行 `cargo check --locked --tests` 和 `git diff --check`。以下验证留待统一运行。

| 验证层级 | 精确命令与证据 |
| --- | --- |
| Rust 投影 | `cargo test --locked --lib options::opencode::models::tests::native_release_lines_preserve_catalog_evidence -- --exact`。检查 CRLF、含嵌套字段的 v1 模型块、变体顺序、杂项日志、v2 多行、空目录和非法行。 |
| 普通 CLI、SDK、原生进程替身 | `cargo test --locked --test protocol -- opencode-model-listing`。替身记录每条进程参数和工作目录，核对版本与模型命令收到相同范围的环境。覆盖 v1/v2 非空结果、空目录、畸形输出、非零退出和超时，以及备用模型来源。 |
| macOS 真实发行程序与本地配置 | 为 1.19.5、2.2.7、2.3.2 各自设置 `KAR_OPENCODE_BIN` 后运行 `cargo test --locked --test native_live opencode_fork::native_picker_and_models_cli_list_without_opening_a_chat -- --ignored --exact --nocapture`。在隔离 HOME 配置 fixture/fixture，核对 SDK 与 CLI 的同一个模型、各自发行线提供的字段，以及本地模型端点没有收到推理请求。 |

对应的手动 CLI 入口为 `kar models opencode --bin /absolute/path/to/opencode --cwd /absolute/path/to/project`。上述供应商场景已编译，尚无本轮实机运行结论。
