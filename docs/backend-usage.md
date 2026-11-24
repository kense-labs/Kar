# Kar 工作台使用量

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。两个入口沿用 Kar 工作台认证；请求和响应使用 snake_case。

| 固定操作 | 正常入口 | 证据和复验预期 |
| --- | --- | --- |
| server.getUsageSummary | `POST /usage/summary` | JSON 必填 since_day、until_day 与 time_zone。resolution=hour 另传含偏移的 since_time 和 until_time，采用左闭右开区间，最长 24 小时。响应含逐来源状态与物理指纹、逐源去重会话数、模型与时段、未定价记录与 token 数。支持 model_filter.include/exclude、provisional、await_refresh。 |
| server.refreshUsageRates | `POST /usage/rates/refresh` | 使用 LiteLLM 模型费率表，24 小时缓存与一分钟刷新下限。下载失败保留已存表，无表时显示 unavailable。主机通过 KAR_USAGE_RATES_URL 指定镜像。推理已含在输出计数，缓存写入按 5 分钟与 1 小时分列计算。供应商申报金额优先，金额分类按比例折算。 |

本地来源包括 Kar 聊天逐流增量（含子会话、供应商交接）、Claude projects、Codex sessions 与 archived_sessions、Pi sessions、Grok updates.jsonl 和显式 MUSE_HOME 下的 session.jsonl。相同原生会话有 transcript 计量时，采用原生记录。Claude 重复内容块按请求和消息 ID 取各计数最大值，Codex 以 response_id 或可对账的上下文快照计量，忽略分叉复制段。Grok 按模型分配剩余申报费用。新增 Kar 用量更新包含原始事件毫秒；老日志使用最近带时间的更新，未包含模型的旧会话展示 unknown。

工作台将规范化计量结果和解析进度写入 `workbench/usage-index-v1.json`。已解析的正文、工具内容和提示词不会进入索引。重复读取可命中缓存；追加时校验已消费前缀的 SHA-256，暂缺换行的尾段留待下次。当前来源下删除的文件仍可从索引恢复已观察的历史。费率源只接收一个 GET 请求。下载表的确定性离线覆盖使用测试自己的本地 HTTP peer。

macOS 编译命令为 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests`。集中复验命令为 `cargo test --locked --test chat_restart -- workbench-usage` 与 `cargo test --locked --lib kar_usage_is_scoped_by_open_child_and_handoff`。HTTP 用例使用专用 HOME、原生格式样例与本地费率表，覆盖秋季夏令时回拨的两个独立小时、折算和申报、重复内容块与请求回执、尾段补全、筛选、未定价、删除后重启、刷新下限。该组尚未执行；供应商真实账单与公开费率源连通性需另行采证。

OpenCode 的数据库和旧消息文件、Antigravity 每会话数据库及 Cursor 账号历史有独立读取契约。仅有 Kar 聊天的计数不能替代这些来源的验收。
