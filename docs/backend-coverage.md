# Kar 后端断言与证据对照

在 Kar 仓库根目录运行以下命令。需要 Rust、Git，以及包含固定提交的 Kar 和上游运行时仓库历史。将 `UPSTREAM_CHECKOUT` 指向本机上游运行时检出目录。

```sh
rustc --edition=2024 -D warnings tools/backend-coverage.rs -o /tmp/kar-backend-coverage
/tmp/kar-backend-coverage --upstream "$UPSTREAM_CHECKOUT" > /tmp/kar-backend-coverage.tsv
/tmp/kar-backend-coverage --filter session.cursor-replays-without-loss-or-duplication
```

完整命令按验收清单的截止提交 `2b15859a13dcc50e8b66cbaee5a007832aa00e08` 读取上游 Git 对象，检查索引的源码位置、断言行范围和公共用例 ID。筛选参数匹配用例或文件位置。省略 `--upstream` 时只检查 Kar 侧关联。每次运行均校验已关联的 Kar 测试文件、固定提交内的断言定位文本和对应的历史证据行。

输出的每行保留供应商、版本、传输、平台、操作、预期、测试命令、实际观察及原始验收状态。同一断言可对应多个平台或运行记录。`assertion` 是上游行号和行内序号，空值表示索引未列直接断言。`kar_test` 是指定 Kar 提交内用于复查的测试片段位置。每条映射在 `backend-assertion-map.tsv` 列出实际比对的行为和供应商范围。

- `原生程序与回环模型` 表示历史测试运行已安装供应商程序，模型响应由测试端点提供。
- `Chromium / 原生 Harness 与回环模型` 表示历史测试在真实浏览器中运行指定版本的 Harness，模型端点使用脚本响应。
- `同址待核对` 表示证据表中有人核对过这一源码位置，报告仍保留逐条核对所需的断言、操作、命令及结果。`待核对` 表示证据表尚无以该位置为键的记录。供应商不支持、真实部署缺失等结论以对应记录的 `ledger_status`、`transport` 和 `observed` 为准。

## 首批核对范围

在 Kar `661854c` 的索引和历史证据上复跑时，脚本读取了 233 个上游文件、2,095 条索引和 283 条历史证据，发现 2 条完全重复的历史记录。18 条人工核对映射关联了 10 个公共用例中的 40 个直接断言，以及 5 个 Pi Durable 专有用例中的 21 个直接断言。固定索引中的 20 个公共用例和 381 个供应商测试全部进入报告。复跑报告核对历史证据的出处，当前环境中供应商的结果需运行报告里的测试命令另行记录。

以下公共用例仍缺以其位置为键的历史证据。

| 公共用例 | 要核对的问题 |
| --- | --- |
| `session.capabilities-match-declaration` | 能力声明与会话方法、原生会话文件、上下文用量是否吻合 |
| `session.effort-never-substituted` | 不支持的推理级别是否被静默替换 |
| `session.effort-listed-levels-apply-across-resume` | 列出的推理级别是否在恢复后准确生效 |
| `session.images-travel-with-input` | 图片路径是否随输入记录，无效图片、纯图片输入及回合终态是否符合约定 |
| `session.framing-and-attribution` | 记录边界和事件归属是否正确 |
| `session.resume-or-loud-rejection` | 是否成功恢复或给出可观察的拒绝 |

已关联的位置也包含剩余断言。例如队列撤回后的会话状态、Pi Durable 原生运行 ID 的正整数检查，仍需单独核对。安装、账户用量、回合结束后引导和回合中引导已有同址证据，首批尚未把它们关联到具体断言。报告逐行保留这些项和其余供应商测试，供后续追加有适用范围的证据。
