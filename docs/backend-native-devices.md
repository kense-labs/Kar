# Kar 工作台设备服务

工作台对照提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。契约来自 `packages/contracts/src/device.ts` 和 `rpc.ts`，服务实现对照 `apps/server/src/device/DeviceService.ts`、`DeviceActions.ts`、`DeviceHubOperations.ts`、`ExpoApi.ts`、`SimlockDeviceHost.ts`。Kar 的本地设备控制和 HTTP 服务均为 Rust。本地发现直接调用 Xcode 和 Android SDK。

| 上游入口 | Kar HTTP 与操作 |
| --- | --- |
| device.configure | `POST /devices/configure` 保存设备开关、onboarding 和 agent access 设置。开启 agent access 时签发每聊天、每设备独立的配置文件，关闭授权即时撤销。关闭设备支持时释放聊天的设备会话，保留设备本身的开机状态。 |
| device.list | `GET /devices`。用 `inspect_only=true` 只探测工具安装情况，不启动 adb 或模拟器。读取 iOS simulator、已连接 Android 及已安装的 AVD。在一个平台读失败时保留该平台上次的结果和失败原因，另一侧继续更新。Kar 使用主机安装的 Xcode 和 Android SDK。 |
| device.testHost | `POST /devices/test-host`。通过 SSH BatchMode 向指定主机读取系统与 Xcode、adb 的可用性，支持端口和指定的 SSH 身份文件。 |
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
- 跨主机 SSH 设备操作、Expo 按时长计费的远端模拟器、Simlock 租约续租、流媒体转发和远端代理接入仍是 Kar 实现缺口。账号查询成功不足以覆盖这些场景。
