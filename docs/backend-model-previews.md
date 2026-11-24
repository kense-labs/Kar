# 模型使用工作台预览和设备

工作台客户端通过 `PUT /projects/{project}/chats/{chat}/model-tools/permissions` 给指定聊天分配资源。模型以该聊天的活动 MCP 租约访问。权限变更会触发 `tools/list_changed`。执行时再次校验项目、聊天、策略和资源。可以只配置需要的字段。

```json
{
  "mode": "execute",
  "preview_tools": {
    "tabs": ["00000000-0000-4000-8000-000000000001"],
    "terminals": ["00000000-0000-4000-8000-000000000002"],
    "device_hosts": ["local"],
    "html": true,
    "html_workspace_files": false,
    "upload_files": false
  }
}
```

| 用户操作 | 模型入口与作用范围 |
| --- | --- |
| 分享工作台已打开的预览 | 将同一项目的实际预览 ID 加入 `tabs`。本机或已配对设备的预览均可授权，设备租约撤销会关闭其预览。外部 MCP 客户端没有聊天内这些工具。 |
| 查看和关闭 | `kar_preview_list` 返回授权的标签页并支持分页。`kar_preview_close` 关闭真实预览，同时释放浏览器目标和隧道。 |
| 打开应用并检查交互 | `kar_preview_open` 接收已授权的 `terminal_id` 和其进程树监听的 `port`，或已共享的 `tab_id`。打开时使用独立无痕配置，`reuse_existing_tab: false` 可另开标签。`kar_preview_status` 提供标题、URL、窗口及待处理对话框。导航局限于已授予的应用来源，可输入相对路径。 |
| 操作浏览器 | `kar_preview_resize` 使用现有工作台 viewport 格式，`kar_preview_set_appearance` 修改配色。`kar_preview_act` 支持点击、输入、悬停、选择、拖动、上传、按键和滚动，`selector` 和 `locator` 接受 CSS。`kar_preview_evaluate` 返回页面脚本结果，`kar_preview_wait_for` 等候选择器、文本和 URL，`kar_preview_dialog` 确认或取消真实对话框。上传需单独设置 `upload_files`，来源限该聊天工作区，文件通过固定目录句柄读取并复制到浏览器标签的临时目录。 |
| 截图与录屏 | `kar_preview_snapshot` 可读取页面文字、控件和 CSS 视口，`include_image: true` 返回实际 PNG，`save: true` 保存聊天附件。`kar_preview_recording_start` 采集 Chromium 的真实 VP8 帧，Kar 的 Rust 代码按采集时间编码为 WebM。`kar_preview_recording_stop` 保存带有 MIME、大小和创建时间的聊天附件。 |
| HTML 预览与发布 | `kar_html_preview` 返回 Chromium PNG、实测内容高度和控制台输出。`kar_html_render` 保存当前聊天的 HTML 附件，在可用时附上九个宽度的测量值。客户端 POST 返回的 `view_url_endpoint` 得到短期阅读链接。HTML 在没有同源权限的 sandbox 下执行。内联 HTML 可在计划模式使用；工作区 HTML 文件和本地图片需要另授 `html_workspace_files`。 |
| 设备列表与实际截图 | 设置工作台设备服务的 `enabled` 和 `agent_access_enabled`，再在聊天策略内列出允许的 `device_hosts`。支持本机、已配置 SSH、Expo、Simlock。`kar_device_list` 列出这些宿主及本聊天已打开的设备，`kar_device_open` 调用同一设备后端并返回受租约约束的 `kar device-agent` 参数，`kar_device_screenshot` 返回真实 PNG，`kar_device_close` 可关闭视图。关闭时指定 `shutdown` 会检查其他聊天是否仍使用该设备。 |

计划模式允许查看、截屏、等待和生成本聊天的 HTML 附件。修改浏览器、打开或关闭设备、录制、执行页面 JavaScript 需要执行模式。签名读取和设备代理配置在每次使用时仍校验其作用范围。

## 复验

```sh
cargo check --locked --tests
cargo test --locked --test workbench_mcp
KAR_MCP_BROWSER_LIVE=1 cargo test --locked --test workbench_mcp
```

普通用例使用隔离的工作台和供应商协议夹具，设备命令通过 Rust 可执行夹具返回可检查的 PNG。浏览器开关要求 macOS 上有可启动的 Chromium，可用 `KAR_PREVIEW_BROWSER` 指定绝对路径。它在真实工作台 PTY 启动隔离的 Rust HTTP 应用，借模型租约打开页面、修改窗口、输入文字、点击、读取 DOM、保存 PNG 和 WebM、验证界外导航被拒、关闭预览。HTML 用例还校验文件权限、阅读签名、沙箱和真实截图中的日志。单独实机设备命令见 [设备验收](backend-native-devices.md)。

开发阶段执行了 macOS arm64 的编译检查。上述功能用例按安排留待统一验收。项目预览、公网页面和桌面标签的授权与传输条件分列如下。HTML 的本地图片扩展支持 PNG/JPEG/GIF/WebP，自包含页面适用于现有隔离代理。

## 公网来源和配对桌面

在当前聊天的 `preview_tools.public_origins` 明确列出允许的来源，如 `["https://docs.example.org","https://assets.example.org"]`，即可用 `kar_preview_open {"url":"https://docs.example.org/page"}` 打开公开页面。Kar 的浏览器读取隔离镜像，宿主只发 GET、验证 DNS 返回的每个 IP、固定连接到验证后的地址。每次重定向重新验证作用域，回环、私网、链路本地、主机自身地址均拒绝。模型无法借镜像提交表单或转发用户凭据。HTML 页面可把已授权来源的静态脚本、样式、图片和字体内联为渲染附件，打开附件时无需网络。依赖登录状态、服务器写操作、外部 WebSocket 或未授权动态来源的网页需要原应用运行环境。

已配对客户端使用其设备令牌 PUT `/projects/{p}/chats/{c}/desktop-previews/{id}`，报告其自己创建的预览标签和导航状态。初次报告示例为 `{"navigation":{"nav_status":{"_tag":"Success","url":"https://docs.example.org","title":"Example"},"can_go_back":false,"can_go_forward":false}}`。在聊天的 `preview_tools.tabs` 授权该 ID 后，模型列表返回 `runtime: "desktop"`。关闭动作发布现有项目预览事件流的 `closed` 事件，宿主据此关闭本地标签；设备撤销同样撤销其视图。现有桌面宿主需要连接 `/projects/{p}/previews/events` 并执行关闭，服务器不能代替桌面程序控制其私有窗口。

工作台固定提交 `2808e878de91aacc7fb82887f18414976be7ae34` 的 `apps/server/src/mcp/toolkits/previewControls/tools.ts:29` 明确标注供模型自动化的是 server 标签。`handlers.ts:28` 对 server 走自动化代理，对其他标签调用管理器关闭。`PreviewAutomationBroker.ts:533` 有多宿主分配机制。生产源码中 `apps/server/src/preview/ServerBrowser.ts:827,2978,3517` 分别更新焦点、回复结果、注册执行者；`apps/server/src/ws.ts:3409` 起暴露预览管理和报告。未发现固定版本的桌面公开 DOM 执行注册入口。

`workbench_mcp` 的普通协议用例覆盖正常设备配对、桌面上报/授权/关闭以及具有白名单的回环来源仍被拒绝。外网复验需要 Chromium、可达的公开网页和该来源下的静态图片，将 `KAR_MCP_PUBLIC_URL` 和 `KAR_MCP_PUBLIC_IMAGE` 设为相应地址，再运行 `cargo test --locked --test workbench_mcp`。物理桌面窗口验证还需要运行上述报告和事件协议的配对客户端；协议夹具不能证明物理窗口已经关闭。
