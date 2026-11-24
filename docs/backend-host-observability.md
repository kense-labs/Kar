# Kar 工作台主机可观测性

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。主机服务通过 Kar 工作台认证入口提供数据，时间响应使用 UTC，进程启动身份以 Unix 毫秒表示。

| 固定服务与操作 | Kar 入口 | 预期 |
| --- | --- | --- |
| HostResources、server.getHostResources | `GET /diagnostics/host-resources` | macOS 自主机 CPU 读取两次采样之间的使用率、CPU 数和总内存。可用内存根据 vm_stat 的 free、inactive、speculative 页数与页长计算。 |
| ResourceTelemetry、subscribeResourceTelemetry | `GET /telemetry` 和 `GET /telemetry/events` | 获取当前进程树和健康状态。SSE 首帧带完整快照，两客户端共享采样，活动时每秒刷新，最后一个订阅退出后停止定时采样。当前工作台能标识用户聊天运行时和 PTY 的根进程。 |
| ResourceTelemetryHistory、ProcessResourceMonitor | `GET /telemetry/history?window_ms=60000&bucket_ms=5000` 和 `GET /diagnostics/process-history?window_ms=60000&bucket_ms=5000` | 对采样期间的 CPU、内存、存储 I/O 和进程数分桶，以 PID 和启动时间合并进程概要。窗口限制为一秒至一小时，桶宽最低一秒。时间窗口切过两次采样时按落入窗口的时长计量。大于三十秒的采样间隔不归集 CPU 和 I/O 增量。 |
| ResourceTelemetry.retry | `POST /telemetry/retry` | 已健康时返回 `accepted=false`。macOS 采集失败后可请求重采并读到对应状态。 |
| ProcessDiagnostics.read 和 signal | `GET /diagnostics/processes` 和 `POST /diagnostics/processes/signal` | 列出可操作的 Kar 子进程。发信号前再次核对后代关系和系统启动身份。支持 SIGINT 和 SIGKILL，目标需提交上次看到的 PID、start_time_ms。对服务器自身、外部进程和已过期身份返回 signaled=false。 |

macOS 实现复用 Kar 的 libproc 启动身份和终端后代查找，进程与 CPU 计量使用 sysinfo 0.38.4。固定上游的监视器使用 0.39.3，该版本要求 Rust 1.95；Kar 仍支持项目声明的 Rust 1.89。没有桌面宿主上报时，桌面进程与电源状态显示为不可用或未知。

复验命令为 `cargo test --locked --test chat_restart -- workbench-observability`。它启动普通 Kar 服务器与专用 PTY，核对真实 macOS 内存、CPU、进程身份、进程分类、两条 SSE 首帧与后续帧、两个历史入口、健康状态重试、三个拒绝目标和对该专用 PTY 的合法信号。该入口和全部测试目标已在 macOS 通过 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests` 编译。按统一验收顺序，运行证据待集中采集。

## 请求追踪、OTLP 与排查上下文

| 操作 | Kar 入口与证据 |
| --- | --- |
| server.getTraceDiagnostics | `GET /diagnostics/traces` 聚合状态目录内的 `traces/workbench.jsonl` 和三份轮转文件。正常工作台请求写入规范化路由名、时间、状态及随机 trace/span ID。诊断结果含解析失败、失败与中断数量、一秒阈值的慢 span、按次数排序的操作、近期失败和警告日志。缺少每份文件和某份文件读取失败分别报告。数据不含请求正文、URL 查询参数或认证头。 |
| server.checkOtlpEndpoint | `POST /diagnostics/otlp` 接收 signal（traces、metrics 或 logs）与 HTTP(S) URL。五秒内只读取状态，返回 accepted 和延时、rejected 和状态码或 unreachable 和超时标记。请求发送所选信号的空 OTLP JSON 或 protobuf 导出。只有与配置的完整导出地址相同才附相应 OTEL_EXPORTER_OTLP_HEADERS 或单信号 headers，不跟随重定向。 |
| server.prepareTriage | `POST /diagnostics/triage` 接收 client 的 label、app_version、os、browser、web_deployment 并返回引用本地 `triage-playbook.md` 的 composer 文本和文件位置。每次使用独立 context.md。服务从 Kar 公开仓库解析当前版本标签（含附注标签），缺少标签时定位 main。Rust 后台任务将指定提交的源码检出到提交专属缓存，成功后原子发布。网络失败时上下文保留失败状态。主机可通过 KAR_TRIAGE_REPOSITORY 指定 Kar 镜像。 |

本组对照固定源码 `TraceDiagnostics.ts`、`OtlpEndpointCheck.ts`、`TriageService.ts`、`triageSession.ts` 和 `triagePrompt.ts`。前述 `workbench-observability` 场景增加了真实 HTTP span 与失败计数、错误轮转行、OTLP 协议替身的认证/重定向/超时、专用 Git 仓库的附注标签、提交精确匹配及两次排查的独立目录。macOS 编译已通过，场景尚待集中运行。OTLP 测试使用本地 HTTP peer，部署中的外部收集器需要自己的接收地址和导出凭据。
