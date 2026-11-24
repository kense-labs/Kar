# macOS 终端和预览

固定工作台后端为 `2808e878de91aacc7fb82887f18414976be7ae34`。HTTP 路径前缀为 `/projects/{project}`。API 请求沿用工作台认证和项目权限。

|操作|Kar 请求与可观察结果|复验|
|---|---|---|
|清空指定 PTY|向 `/terminals/{id}/clear` 发 POST，返回 `{"type":"cleared","cursor":N}`。原有 `/output?after=...` 的 events 包含清屏边界，两端从同一游标看到按序的边界和后续输出。服务重启后，清屏前的字节已无法从该终端读取。连续清屏保留最近 1024 个边界。| `cargo test --locked --test workbench_terminal_preview clear_wakes_two_observers_and_survives_restart_without_replacing_the_pty -- --exact` |
|停止已发现的本地服务|从 `/terminals/{id}/ports` 取得实际 pid、端口和出生时间，向 `/terminals/{id}/ports/stop` 发 POST，提交 `pid`、`port`、可选的 `born`。检查当前进程归属及整个作业组后发送 TERM；三秒后检查原成员仍在同组才升级到 KILL。错误端口、失效身份、其他终端和共享 shell 作业组会被拒绝。| `cargo test --locked --test workbench_terminal_preview stop_checks_live_owner_and_job_and_only_terminates_the_selected_listener -- --exact` |
|浏览器视口、缩放、外观|创建预览时可提供 `profile_id`、`viewport`、`zoom_factor`、`color_scheme`。向标签的 `/previews/{id}/viewport` 发 PUT，提交 `{"_tag":"freeform","width":800,"height":600}`、`preset` 或 `fill`。向 `/previews/{id}/browser` 发 PATCH，提交 `{"zoom_factor":2,"color_scheme":"dark"}`。该地址的 GET 返回原生标题和布局宽高。实际浏览器画面通过同一授权下的 GET `/previews/{id}/screen` 读 PNG。无视口约束的服务端页面初始画布为 1280 × 800。|以下真实浏览器场景|
|指定标签硬刷新、清除数据|对同一 browser 地址 PATCH `{"hard_reload":true}`、`{"clear":"cookies"}` 或 `{"clear":"cache"}`。Chromium 执行对应 CDP 操作。POST `/preview-profiles/{profile}/clear` 关闭该配置的浏览器并清除其目录；下次访问使用新配置启动。同一项目和设备下按配置隔离；incognito 按标签使用临时目录。|以下真实浏览器场景|

本轮 macOS 编译命令为 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --test workbench_terminal_preview`。终端实测需要 macOS PTY、工作台 HTTP 服务和系统 lsof。浏览器实测还需要可启动的 Chromium，在 macOS 安装标准 Chrome/Chromium 或 Brave，也可将 `KAR_PREVIEW_BROWSER` 设为可执行文件绝对路径。浏览器由 Kar 的 Rust 后端调用，使用独立用户数据目录、只监听回环的 DevTools 和仅允许该配置内有效预览访问的 HTTP 代理。前端可继续使用原始预览 URL，也可显示服务端渲染的 PNG。

联合验收命令为 `cargo test --locked --test workbench_terminal_preview managed_chromium_applies_controls_and_keeps_two_profiles_independent -- --ignored --exact --nocapture`。场景使用两个独立浏览器配置，从真实网页读视口、配色、cookie、缓存资源与 localStorage。源码移植期间仅执行编译，待运行时登记浏览器发行版和操作系统版本。

`GET /preview-profiles` 返回当前配置目录，工作台按项目与设备分别维护目录。`WS /preview-profiles/report` 接受包含 profiles、default_profile_id 及可选 recording_frame_rate 的 JSON。在线客户端按最近报告顺序合并；断开连接时重新合并，在后端重启时加载最后落盘的完整目录。创建标签省略 profile_id 会选该目录的默认配置。

外部预览可 `PUT /previews/{id}/status` 报告 nav_status、can_go_back、can_go_forward。服务端 Chromium 接管后，状态由原生导航读取，外部报告得到 403。`GET /previews/events` 以 SSE 发布打开、导航、失败、调整和关闭事件。支持 after 查询或 Last-Event-ID 从缓冲内续收，超出缓冲或旧游标超出当前范围时返回 resync。复验命令为 `cargo test --locked --test workbench_terminal_preview profile_reports_merge_persist_and_preview_navigation_reaches_observers -- --exact`。
