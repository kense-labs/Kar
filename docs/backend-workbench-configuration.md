# 工作台服务配置和内置供应商安装

GET /server/config 返回当前设置、工作目录、供应商、解析后的快捷键和修订号。GET /server/config/events 为每个客户端先发送当前快照，后续设置或快捷键变更再次发布。PATCH /server/settings 支持收藏、模型顺序与隐藏列表、界面流式显示和环境图标，以及与原工作台共用存储的默认工作区、工作树起点、供应商更新检查、提示建议、后台活动和 Git 文案偏好。

快捷键默认表含 88 条规则。PUT /server/keybindings 使用 key、command、可选 when 和 replace；DELETE 使用相同的 key、command 和 when。条件支持 !、&&、|| 以及括号。config 中的 keybindingsConfigPath 指向可手工编辑的 keybindings.json。正在订阅的客户端会收到外部编辑和解析问题。文件损坏时连接仍能读取上次有效设置，修复后自动加载。文件中的未知规则会在界面保存有效规则时保留。

## 内置安装

固定工作台 apps/server/src/provider/providerInstallation.ts 第 83 至 125 行只为 Codex 和 Antigravity 选择内置安装器。GET /providers/{id}/install 读取当前操作、已安装版本、可执行路径、来源和是否允许卸载，GET /providers/{id}/install/events 建立初始快照及变更订阅。宿主 PUT /providers/{id}/setup 可用 setupMode 选择 managed 或 existing。Codex 首次安装前须选择 managed，Antigravity 默认使用托管目录中已验收的版本。宿主 POST /providers/{id}/install/start、cancel、remove 控制任务，取消需要传入 operationId。

Kar 在 macOS 下载供应商的发布包，Codex 使用官方 GitHub 发布校验和或固定版 0.166.0 的 SHA-256，Antigravity 通过官方 updater 获取目标版本和 SHA-512。解包后检查 Codex 清单、辅助程序和版本，Antigravity 使用只初始化的 ACP 连接验证身份、版本、恢复、登出和登录方法。在独立暂存目录通过后切换生效的安装。并发请求加入同一个任务，正在使用的版本受文件锁保护，普通聊天创建、恢复、模型菜单使用同一可执行路径。移除只处理 Kar 有匹配安装收据的版本，并保护外部可执行文件和主动引用这些路径的实例。

## 复验与范围

macOS 开发编译使用 cargo check --locked --all-targets。统一验收入口为 cargo test --locked --test workbench_configuration 及 cargo test --locked --test workbench_managed_providers。前者覆盖双客户端订阅、快捷键冲突条件、手工损坏与恢复、设置和重启。后者用本机 HTTP 发放两种真实格式的归档，供应商程序由 Rust 协议夹具扮演。正式供应商验证需在独立 Kar 状态和测试账户下通过正常安装入口访问官方发布源，再从工作台发起聊天。开发阶段未下载或执行官方安装，也未运行这两个场景。

固定工作台版本为 2808e878de91aacc7fb82887f18414976be7ae34。服务端配置对应 apps/server/src/serverSettings.ts、keybindings.ts 及 packages/shared/src/keybindings.ts。内置安装对应 apps/server/src/provider/CodexInstallation.ts、AntigravityInstallation.ts 和 providerInstallation.ts。Kar 使用已有的安装探测和本机聊天运行时解析；第三方 ACP 安装仍由其独立目录和收据管理。

### 桌面应用自身更新

固定源码 apps/server/src/desktopUpdate/DesktopAppUpdate.ts:63 只有在 desktop 模式且桌面宿主提供控制文件描述符时才启用。服务端在第 177 行发起准备请求，按返回 requestId 跟踪进度，并在第 216 行将提交命令交给同一宿主。真正开始安装及重启的是 apps/desktop/src/updates/DesktopRemoteUpdates.ts 里的桌面更新器。Kar 当前是独立 Rust 工作台后端。这批配置提交没有添加桌面发布包、桌面宿主更新器或等价的宿主控制协议，也没有完成桌面更新准备或提交的实机验收。内置供应商发布包安装有独立入口与证据。
