# 工作台语音转录

Kar 的工作台使用 Rust 解析音频、校验本地模型、调用推理程序或云端服务，并返回文本。启动命令为 `kar workbench --state <状态目录> --root <项目目录>`。请求带工作台 Bearer 令牌。

## 本地转录

设置 `KAR_VOICE_ENGINE` 为独立安装的 `transcribe-cli` 路径，或将它放入 PATH。对应的推理程序为 transcribe.cpp 0.4.1。Kar 独立启动该程序，每次推理结束就释放模型。CPU 与实际探测到的 GPU 可用。浏览器可向 `/ws/voice` 分块上传，由服务在收到 finish 后统一转录。

1. `GET /api/voice/status` 查询引擎和已选模型。引擎缺失时返回 `supported: false` 及原因。
2. `GET /api/voice/models` 查询固定目录。通过 `POST /api/voice/models/link` 提交 `{"path":"<本地 GGUF 绝对路径>"}`，或向 `/api/voice/models/download` 提交 `{"modelId":"<目录 ID>"}` 下载。模型必须匹配目录中的大小与 SHA-256。
3. 向 `POST /api/voice/models/select` 提交模型 ID，随后可用 `POST /api/voice/models/prepare` 检查模型能否加载。
4. 向 `POST /api/voice/transcribe` 提交二进制音频帧。成功时返回 `{"text":"…"}`。

帧头是四字节小端无符号整数，表示后续 UTF-8 JSON 选项的字节数。选项后紧接单声道 16 kHz 的小端 Float32 PCM，样本取值范围为 -1 至 1。必填选项示例为 `{"speechLanguage":"en","speechTranslateToEnglish":false,"speechCustomWords":[],"speechRemoveFillerWords":false}`。音频上限为 300 秒。

## 云端转录

`POST /api/voice/cloud/<供应商>/key` 接受 `{"key":"<专用 API 密钥>"}`，传入 null 删除密钥。密钥采用 AES-GCM 保存在工作台私有状态目录，`GET /api/voice/cloud/keys` 只返回是否已设置。使用密钥引用时，可配置主机的 `KAR_VOICE_SECRET_PROVIDERS` 指向一个 JSON 映射；每个条目指定提供者名及允许的目录根或 macOS Keychain 服务。

`POST /api/voice/cloud/<供应商>/transcribe` 接收相同音频帧。可选 `openai`、`xai` 或 `openrouter`。使用 OpenRouter 时，先读取 `GET /api/voice/cloud/openrouter/models`，再传入 `?model=<目录 ID>`。服务根据实际供应商的返回生成文本或错误，错误区分密钥缺失、读取失败、被拒绝、限流及模型不可用。工作台流量复验可使用主机指定的 `KAR_VOICE_OPENAI_BASE`、`KAR_VOICE_XAI_BASE` 或 `KAR_VOICE_OPENROUTER_BASE`。

## 第 16 项复验

| 用例 | 条件与命令 |
| --- | --- |
| 云协议及错误回执 | `cargo test --locked --test workbench_voice cloud_protocol_and_errors_through_workbench -- --exact`。使用本机 Kar 与回环对端，检查实际 WAV 字节、三个供应商的请求形状、缺少密钥和供应商拒绝。 |
| 本地实际推理 | 设置 `KAR_VOICE_ENGINE`、`KAR_VOICE_TEST_MODEL`（GGUF 路径）、`KAR_VOICE_TEST_MODEL_ID`、`KAR_VOICE_TEST_WAV`（单声道 16 kHz PCM16 WAV）、`KAR_VOICE_TEST_EXPECT`（预期短语），运行 `cargo test --locked --test workbench_voice installed_local_model_transcribes_audio -- --exact --ignored --nocapture`。用例经正常 HTTP 链接、选择并加载模型，将真实音频送入转录接口，检查返回文本。 |
| 已配置供应商实际推理 | 设置 `KAR_VOICE_TEST_PROVIDER`、`KAR_VOICE_TEST_KEY`、`KAR_VOICE_TEST_WAV`、`KAR_VOICE_TEST_EXPECT`。OpenRouter 另设 `KAR_VOICE_TEST_MODEL_ID`。运行 `cargo test --locked --test workbench_voice configured_cloud_transcribes_audio -- --exact --ignored --nocapture`。此用例向所选供应商提交音频，可能产生账户费用。 |

固定本地样例可用 `handy-computer/whisper-tiny.en-gguf` 的 `whisper-tiny.en-Q8_0.gguf`（SHA-256 `e8c9b73c06344307d8b346e07fbe93dd88d894627854bcff31523f1ce44394fa`）及 transcribe.cpp v0.4.1 的 `samples/jfk.wav`（SHA-256 `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`），预期短语为 `what your country`。统一套件分别保留回环协议、本地模型和供应商账号三个结果。
