# Kar

Kar 是运行编程智能体的 Rust 命令行工具和开发库。每个项目的提问、工具调用和回复都保存在本机，关掉终端也能接着聊。Kwrok 桌面端用的就是这套会话接口。

[English](README.md) · [快速开始](#快速开始) · [管理对话](#管理对话) · [接入应用](#接入应用) · [参考](#参考文档)

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/kar-hero-zh-dark.svg">
    <img src="assets/kar-hero-zh-light.svg" alt="命令行、Rust SDK 和 Kwrok 都可以连接 Kar。Kar 与选中的智能体通信，并把对话记录保存在本地。" width="760">
  </picture>
</p>

## 快速开始

用 Cargo 安装 Kar。下面以 Codex 为例，需要先在本机安装 Codex CLI 并登录。

```sh
cargo install --locked --git https://github.com/kense-labs/Kar.git --bin kar
```

进入一个带 README 的项目，检查 Kar 是否找到了 Codex，然后开始第一轮对话。

```sh
kar list codex --cwd .
kar chat new codex --cwd . "阅读 README.md，用一句话说明这个项目做什么。"
```

命令退出后，继续刚才的话题，再查看这两轮记录。

```sh
kar chat resume "README 里给出的测试命令是什么？"
kar chat history
```

运行 `kar list` 可以查看 Kar 支持的运行时。想用其他已安装的智能体，把示例中的 `codex` 替换为它的 ID。每条记录列出安装状态、通信协议，以及 Kar 在本机探测到的可用能力。

## 管理对话

`kar chat list` 列出已保存的对话。`kar chat resume` 默认在原工作区中继续最近一次对话，也可以用 `--chat` 指定另一条对话的 ID。通过环境变量 `KAR_CHAT_DIR` 可以指定记录的位置。若要在命令行查看桌面端的会话，将它指向同一个对话目录即可。

| 想做什么 | 怎么做 |
| --- | --- |
| 在另一个终端查看进度 | `kar chat watch` 持续输出事件。重新连接时，把上次的游标传给 `--after`。 |
| 恢复文件前核对改动 | 从 `kar chat history` 复制轮次 ID，用 `kar chat changes <TURN>` 查看文件改动，用 `kar chat rollback <TURN> --dry-run` 预览恢复结果。 |
| 在单独的工作树尝试另一种做法 | 对支持原生分叉的运行时，使用 `kar chat fork <TURN> --worktree <PATH>` 创建关联的 Git 工作树。 |
| 排查中断前的消息是否已送达 | `kar chat queue` 显示投递回执。若无法确认是否送达，Kar 会标为待确认，等待处理。 |

## 接入应用

**Rust SDK。** 在项目目录打开运行时，提交提示词并等待结果。将本仓库的 `kar` 加为 Git 依赖，并为 Tokio 启用 `macros` 和 `rt-multi-thread`。

```rust,no_run
use kar::{Outcome, SessionOptions, runtime};

#[tokio::main]
async fn main() -> kar::Result<()> {
    let session = runtime("codex")?.open(SessionOptions::new(".")).await?;
    let report = session.prompt("概括当前的 Git 差异。").await?.report().await?;
    if report.outcome == Outcome::Completed {
        println!("{}", report.text);
    }
    session.close().await
}
```

**工作台服务。** 以下命令为桌面端或编辑器启动本地 HTTP 服务，并打印回环地址和访问令牌。客户端可以通过 SSE 订阅对话事件，在同一个项目里共享终端、审查 Git 改动或编辑 Office 文档。

```sh
kar workbench --root "$PWD" --port 4777
```

**MCP 与 WebAssembly。** `kar mcp --runtimes codex,claude --max-running 4` 通过标准输入和输出提供智能体池，同时最多运行四个会话。Rust 库也可以编译为 WebAssembly，在浏览器里运行 Pi Durable。[宿主工具](src/host_tools.rs) · [浏览器实现](src/browser) · [Kwrok 桌面端](https://github.com/kense-labs/kwork)

## 参考文档

| 主题 | 文档 |
| --- | --- |
| 安装运行时、选择协议、确认版本差异 | [运行时参考手册](docs/runtime-reference.zh-CN.md) · [CLI 命令表](docs/backend-cli-map.tsv) |
| 原生会话、智能体交接与输入队列 | [会话管理](docs/backend-workbench-agent-sessions.md) · [运行时交接](docs/backend-workbench-handoff.md) |
| 开发工作台客户端 | [HTTP 接口](docs/backend-workbench-http-map.tsv) · [MCP 工具](docs/backend-workbench-mcp-map.tsv) · [文档编辑](crates/kar-documents/README.md) |
| 在本机验证源码 | [macOS 综合验收](docs/backend-workbench-final-acceptance.md) · [运行时覆盖情况](docs/backend-source-parity.toml) |

在源码目录运行 `cargo test --locked --test workbench_acceptance`，可以验证工作台的跨模块协作流程。

## 开源许可

Kar 自有代码采用 MIT 许可。Kantor Office 引擎采用 AGPL-3.0-or-later，智能体图标和语音目录各有单独声明。[MIT 许可证](LICENSE) · [Kantor 许可证](https://github.com/kense-labs/kantor/blob/9b780379e32d3fa4d293499f33fbc2483655567d/LICENSE) · [图标声明](src/brands/NOTICE.md) · [语音目录声明](src/workbench/voice/CATALOG-LICENSE)
