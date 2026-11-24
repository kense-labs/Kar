# 公共用例中的图片输入

对照上游运行时 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 的 `sea-trial/cases/session-images.ts` 第 17–46 行。下表列出已运行的供应商、版本、平台与测试。模型请求均发送给隔离的本地 HTTP 服务，供应商 CLI 及会话协议使用真实发行包。

| 供应商 | macOS 27.2.1 arm64 | Ubuntu 24.04.5 aarch64，Lima 虚拟机 | 复验命令 |
| --- | --- | --- | --- |
| Qwen Code，stream-json | 0.27.4 和 0.28.4 通过 | 0.27.4 通过 | 指定 `KAR_QWEN_BIN`，运行 `cargo test --locked --test qwen_live images_travel_with_input -- --ignored --nocapture` |
| Grok CLI，ACP | 1.0.70 和 1.0.92 通过 | 1.0.70 通过 | 指定 `KAR_GROK_BIN`，运行 `cargo test --locked --test grok_live native_image_reaches_the_model -- --ignored --nocapture` |

| 固定源码行 | 操作与期望 | 实际观察 |
| --- | --- | --- |
| 25–29 | 无附件且文本为空，逐一调用 prompt、steer、queue，三个入口分别拒绝 | 所有上述版本均返回 `unsupported` 和 `empty input: give text or images`。每个请求与唯一回执按请求 ID 关联，会话保持空闲 |
| 30–35 | 图片能力关闭时，携带有效 PNG 和非空文本提交，整条拒绝 | Qwen 的 Kar 会话声明图片能力关闭，拒绝码为 `unsupported`。额外独测纯图片也整条拒绝，两次都没有触发模型调用。随后发送普通文字成功完成 |
| 36–40 | 图片能力开启时，提交缺失的 PNG 或不属于图片类型的路径，整条拒绝并维持空闲 | Grok 的适配器分别记入 `error`、`unsupported`。每次把一张有效图片列在错误路径前，整条请求保持未派发。另测文件名为 PNG、内容为纯文本，同样被拒绝 |
| 41–46 | 用固定 32×32 PNG、空文本发起纯图片回合，接收请求回执和完成事件 | Grok 各版本均完成。公开请求保存空文本和含空格、`./` 的原始文件路径，出站记录没有 PNG 的 base64。ACP 仅发送一个图片块，真实供应商交给模型的最新用户消息带有与原文件完全相同的 PNG 数据。出站请求与接受回执按 ID 关联 |

Qwen 不收图片是固定上游运行时适配器的能力约定。Qwen 的 `index.ts`、`session.ts` 均声明 `images: false`，后者的 `userMessage` 创建文本块。该分支在 Kar 的原生出站前即可拒绝。这里的证据适用于此 stream-json 适配器，未据此判定 Qwen 其他入口或模型本身的视觉能力。

Qwen 测试额外通过实际 `kar run qwen 'what is this?' --image <path> --json --record <file>` 核对非零退出、关联的拒绝回执、标准输出与完整录制文件逐字节一致，模型端仍未收到请求。Grok 用例读取移除新增图片元数据的旧式 Kar JSON 记录，保留其既有提示身份。

Windows 和远程托管模型未做本次实测。上述命令需要可执行的对应供应商 CLI，npm 发行包还需要 Node；测试自行创建用户目录、工作区并启动回环模型服务。

## DSH Web 图片输入

原清单的公共用例为 `session.images-travel-with-input`。Kar 在当前 DSH 进程调用 `llm.resolveModelInfo` 并读取 `attachments.imageLimits`。确认模型接受图片、所有 MIME、单图/整批字节和数量限制后，向原生 `/api/session/uploadFileBinary` 逐个上传，再把同一会话的 `receiptId` 交给 `session/prompt`。仅图片输入省略文本块。原生确认写入的附件 ID 可以从 DSH 会话文件读回。模型或存储未报告支持时，当前会话的图片能力为关闭。DSH 发行包中的 Web 模型目录省略了输入模态，Kar 的探针为随 Rust 二进制分发的 Rust Node-API 插件。

macOS 已完成 `cargo check --locked --offline --tests --example backend_suite`。以下两项待统一执行。

| 环境 | 复验命令 | 检查内容 |
| --- | --- | --- |
| macOS，Rust 协议对端 | `cargo test --locked --lib dsh::images` | 当前类型过滤、单图/总量/数量限制、按输入顺序上传、上传途中只返回部分回执、不产生可提交内容。 |
| macOS，官方 DSH Web 0.2.2-rc.5、原生插件及图像编解码器，本地模型响应 | 设置 `KAR_DSH_BIN`，执行 `cargo test --locked --test dsh_live images::uploads_images_to_the_selected_route_and_recovers_native_references -- --ignored --exact` | 文本和 PNG、仅 PNG 排队、原生用户消息与持久附件引用、同一会话改开纯文本模型后的拒绝，以及超过原生数量上限时模型端无请求。 |

对照提交为 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 中的 `src/runtimes/dsh/bridge-input.ts`。官方 DSH 0.2.2-rc.5 的通路见 `dsh-client-file-upload/src/http-route.ts`、`dsh-client-file-upload/src/index.ts` 和 `dsh-api-session-controller/src/commands.ts`。此处开发证据为源码核查与 macOS 编译；DSH 供应商实测结果待填。
