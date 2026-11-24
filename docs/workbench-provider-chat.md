# Kiro、Fx 和 Muse 聊天复验

工作台对照提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。Kiro V3 和 Fx 使用各自的 ACP 设置，Muse 使用 MSP v1。运行时注册表驱动 `kar chat new` 和工作台创建聊天接口。

| 供应商 | 原生程序 | 原生设置及恢复 |
|---|---|---|
| Kiro | `kiro-cli acp --agent-engine=v3 --auth-method=cli` | 关闭 autopilot。再次启动时使用已保存的原生会话 ID 和 cwd。模型、代理和推理强度由原生配置方法确认。 |
| Fx | `fx acp` | 使用工作区权限模式和本机 profile MCP。新建和恢复都显式写入 ask 或用户选择的模式。 |
| Muse | `muse serve --trust-workspace` | 协商 MSP v1。保存客户端生成的原生 UUIDv7，恢复时确认会话 ID 和存活的投影。回合回执必须匹配提交的命令 ID。会话级 MCP 需要原生授权，仅可发送 streamable HTTP 配置。 |

## 协议回归

`cargo test --locked --test workbench_provider_chat` 从三个真实 Kar CLI 入口分别新建、跨进程恢复并读取历史。测试子程序扮演原生运行时，校验启动参数、工作区、设置和消息身份。

## 已安装供应商

逐个选择本机已登录并可发送模型请求的程序。在默认搜索路径之外安装时可分别设置 `KAR_KIRO_BIN`、`KAR_FX_BIN` 或 `KAR_MUSE_BIN`。记录程序版本、平台和命令退出状态。

```sh
cargo test --locked --test workbench_provider_chat -- installed kiro
cargo test --locked --test workbench_provider_chat -- installed fx
cargo test --locked --test workbench_provider_chat -- installed muse
```

每条命令创建独立临时工作区和聊天存储，通过真实 Kar CLI 发出两轮请求。第二轮在新进程中恢复原生会话，提示只要求复述第一轮随机值。断言读取两轮各自的助手输出。

## 边界

Muse 当前可恢复已结束回合的会话。若原生回复表示回合仍活跃、投影不可用或会话 ID 不匹配，Kar 会返回具体错误。Muse 原生分叉、子代理自主唤醒及后台工作流的审批不在此适配器批次。Kiro 和 Fx 的恢复取决于本机发行版协商到的 ACP 会话能力；运行时拒绝时 Kar 通过普通入口返回原因。
