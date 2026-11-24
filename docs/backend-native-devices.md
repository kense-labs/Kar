# Kar 工作台设备服务

工作台对照提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。契约来自 `packages/contracts/src/device.ts` 和 `rpc.ts`，服务实现对照 `apps/server/src/device/DeviceService.ts`、`DeviceActions.ts`、`DeviceHubOperations.ts`、`ExpoApi.ts`、`SimlockDeviceHost.ts`。Kar 的本地设备控制和 HTTP 服务均为 Rust。本地发现直接调用 Xcode 和 Android SDK。

| 上游入口 | Kar HTTP 与操作 |
| --- | --- |
| device.configure | `POST /devices/configure` 保存设备开关、onboarding 和 agent access 设置。开启 agent access 时签发每聊天、每设备独立的配置文件，关闭授权即时撤销。关闭设备支持时撤销聊天视图，本地设备保持开机，云租约进入释放流程。 |
| device.list | `GET /devices`。用 `inspect_only=true` 只探测工具安装情况，不启动 adb 或模拟器。读取 iOS simulator、已连接 Android 及已安装的 AVD。在一个平台读失败时保留该平台上次的结果和失败原因，另一侧继续更新。Kar 使用主机安装的 Xcode 和 Android SDK。 |
| device.testHost | `POST /devices/test-host`。通过 SSH BatchMode 向指定主机读取系统与 Xcode、adb 的可用性，支持端口和指定的 SSH 身份文件。将同样的主机对象加入 `POST /devices/configure` 的 `ssh_hosts` 数组，可选择 `host_id` 实际启动、控制、截图、订阅该主机的模拟器。不同主机上的相同设备 ID 分开计入资源与授权。替换或移除主机会释放其视图和代理配置。 |
| device.easAccount | `POST /devices/eas-account`。向 Expo 官方 GraphQL 获取账户名称和应用。支持传入 token、EXPO_TOKEN 与 EAS 本机登录。 |
| device.simlockIdentity | `POST /devices/simlock-identity`。向指定网关请求 /v1/identity，校验 agent 角色。仅匹配本机配置的网关能使用对应环境凭证。 |
| device.open | `POST /devices/open` 接收 project、chat、device_id、platform、boot 并校验聊天归属。可以启动指定 iOS 模拟器或已安装的 Android AVD。启动中对订阅方公布 booting_devices，完成后重新扫描并用真实设备 ID 创建视图。 |
| device.close、device.shutdown | `POST /devices/close` 默认只释放指定聊天的设备视图，可指定同时关机。`POST /devices/shutdown` 关闭模拟器并清除所有指向它的视图。 |
| device.detail、device.action | `GET /devices/detail?device_id=…` 读取真实设置。向 `POST /devices/action` 发送设备 ID 和原动作类型。支持平台相应的外观、文字、位置、App、推送、权限、方向和辅助功能。ADB shell 参数逐项转义。iOS 扩展辅助设置需要已安装的 KAR_DEVICE_IOS_AX_HELPER；通知权限需要 KAR_DEVICE_IOS_PERMISSION_HELPER。 |
| subscribeDeviceState、设备截图 | `GET /devices/events` 返回初始状态和后续变更，有订阅者时定期刷新本地设备；`GET /devices/screenshot?device_id=…` 返回原始 PNG。iOS 用 simctl io，Android 用 adb exec-out。已打开的设备视图可通过 `GET /devices/stream?project=…&chat=…&device_id=…` 接收实时 multipart PNG。多个视图共享一次本地抓图，关闭或撤销立即停止该视图。 |

## 复验与界限

- macOS 编译命令为 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests`；已通过。
- 协议替身验收入口为 `cargo test --locked --test chat_restart -- workbench-devices`。它运行普通 Kar 服务，以独立 Rust 进程记录 xcrun、adb、emulator 与 ssh 的实际参数，核对两聊天共用设备、关闭所有权、实际读回、图像与重启后设置。此项待集中执行。
- 真实 iOS 入口为 `KAR_DEVICE_LIVE_ID=<专用已开机模拟器的UDID> cargo test --locked --test chat_restart -- workbench-devices-live`。需要安装 Xcode、对应 iOS Runtime 并启动模拟器。用相同的外观设置验证控制链，关闭视图后保留开机状态。真实 Xcode 动作和截图尚待统一执行。聊天对端在该测试中仍为 Codex 协议替身。
- Android 真实操作需要可用的 adb、Android SDK Emulator 和已安装 AVD 或已授权真机。外部账号和 SSH 需要有效 Expo 登录、可达 Simlock 网关与 agent token，或已配置登录的 SSH 主机。集中验收必须记录实际工具版本、目标设备与读回。SSH 探测和账号读取单独验收。
- 启用代理授权后，打开设备的响应含 `agent_config`。从同一主机运行 `kar device-agent --config <file> detail`、`screenshot` 或 `action --json <动作对象>`。独立回环入口只接受该聊天及设备的令牌。关闭视图、撤销 agent access 或重启均使旧令牌失效。配置文件权限为 0600。正常 HTTP 与 CLI 的代理授予及撤销检查归入上述两个复验入口。
- SSH 主机真实复验需要已配置的批处理登录，远端需要 Xcode 或 Android SDK。

## Simlock 与 Expo 云设备

`POST /devices/configure` 的 `cloud_hosts` 接受数组。Simlock 配置示例为 `{"id":"lab","kind":"simlock","label":"Lab","gateway_url":"https://gateway.example","max_sessions":2,"max_minutes":60}`。Expo 示例为 `{"id":"expo","kind":"eas","label":"Expo","app_id":"<Expo 项目 UUID>","max_sessions":2,"max_minutes":30,"idle_minutes":10}`。可提交 `token`，后续状态只显示 `has_credential`。主机凭据、创建记录和释放所需的原始凭据持久保存于状态目录内的 0600 文件。

| 供应商及来源 | 操作与期望 |
| --- | --- |
| Simlock 网关及 agent-device 0.21.41 | 从 `/v1/catalog` 选择工作节点在线的手机或平板。向 `/v1/agent-device/rpc` 分配并按返回的租约 ID 打开。核对租户、run 和后端。每两分钟续约，到期、显式关闭或重启时回收。令牌可随主机保存或来自匹配网关的 `AGENT_DEVICE_DAEMON_BASE_URL` 和 `AGENT_DEVICE_DAEMON_AUTH_TOKEN`。截图通过该租约的认证 artifact 下载。 |
| Expo Simulators GraphQL | 可选 `eas-ios` 或 `eas-android`。从聊天物理工作区的静态 Expo 项目配置确定付费项目，歧义和动态配置使用显式 `app_id`。使用主机 token、`EXPO_TOKEN` 或 EAS 登录。创建前持久化唯一名称，记录时长与空闲上限，待 `IN_PROGRESS` 后读取预览服务的设备标识。预览和原生 agent 使用各自凭据。停止时核对远端终态。未收到创建回执时通过项目、原始唯一名称及分页的活跃列表找回。明确拒绝放回并发名额，未知结果在列表确认和宽限期后释放名额，并继续清理迟到资源。 |
| 工作台与聊天代理 | 已打开的视图使用 `/devices/stream` 取共享 PNG。`GET /devices/video` 向同一视图提供 Simlock 或 Expo iOS 的 AVCC 字节流，`/devices/input` 提供输入 WebSocket，Expo Android 在该 socket 上同时返回视频。三者均使用 `project`、`chat`、`host_id`、`device_id`。`GET /devices/leases` 返回租约状态、名额和最近原生确认时间。查看器令牌关闭后失效。聊天代理可通过 `kar device-agent --config <file> action --json '{"type":"agentCommand","command":"snapshot","arguments":[]}'` 或相同结构的 tap、swipe、press、scroll、type、back、home 操作绑定设备。 |
| 固定源码定位 | 工作台提交 `2808e878de91aacc7fb82887f18414976be7ae34` 的 `apps/server/src/device/{EasDeviceHost,SimlockDeviceHost,DeviceHubProxy,ExpoApi}.ts`、`ExpoApi.graphql`、`apps/server/schema/expo.graphql`、`deviceToolVersions.json`。Simlock 原生选路、请求和 artifact 来自 agent-device v0.21.41 的 `packages/contracts/src/lease-scope.ts`、`packages/client/src/lease-client.ts`、`packages/remote/src/daemon-artifacts.ts` 及 `src/__tests__/remote-connection-proxy-lease-id.test.ts`。该标签提交为 `a620da022cf0eac3554dc9ff5af34ee4f0a1b3c8`。 |

### 分层复验

| 范围 | macOS 入口与实际证据 |
| --- | --- |
| 编译 | `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests` 已通过。`git diff --check` 已通过。 |
| Simlock、Expo 协议替身 | `cargo test --locked --lib workbench::devices::cloud::tests`。使用本地 Rust HTTP peer；覆盖目录、回执返回的真实 ID、重试去重、并发上限、原生续约、隔离凭据、重启账本、明确拒绝与失联后按名称释放。已编译，待集中运行。 |
| Simlock 真实网关 | 设定 `KAR_SIMLOCK_LIVE_GATEWAY`、`KAR_SIMLOCK_LIVE_TOKEN`、`KAR_SIMLOCK_LIVE_CATALOG` 后运行 `cargo test --locked --test chat_restart -- workbench-simlock-live`。需要自己的 agent 角色凭据、在线 worker 及专用目录条目。读取真实截图和代理 snapshot，等待网关续约成功并确认释放。本轮未启动网关租约。 |
| Expo 真实云资源 | 设定 `KAR_EXPO_LIVE_ALLOW_BILLING=1`、`KAR_EXPO_LIVE_APP`、`EXPO_TOKEN` 后运行 `cargo test --locked --test chat_restart -- workbench-expo-live`。需要具备 Expo Simulators 计费权限的项目。检查原生截图、代理 snapshot 和服务端停止回执。本轮未创建计费资源。 |
| 真实云验收工作区 | 以上两个真实云入口通过普通 Kar 工作台 HTTP 和聊天作用域 CLI 工作；聊天模型对端为 Codex 协议替身。验收会先打印私有状态路径。意外中断或云主机拒绝停止时，按打印命令重启 Kar 继续清理。 |

固定上游的两种云主机均不提供通用 simctl 或 ADB shell。工作台对这些原生设置动作返回不支持，交互和图像使用供应商提供的代理、预览或输入通道。旧 Simlock 网关可能丢弃调用方的预分配 ID；若分配响应丢失、随后以原 ID 和 run 回收均无法确认，Kar 保留 `release_pending` 供复查。
