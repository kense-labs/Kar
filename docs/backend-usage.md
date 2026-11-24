# Kar 工作台使用量

固定工作台提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。两个入口沿用 Kar 工作台认证；请求和响应使用 snake_case。

| 固定操作 | 正常入口 | 证据和复验预期 |
| --- | --- | --- |
| server.getUsageSummary | `POST /usage/summary` | JSON 必填 since_day、until_day 与 time_zone。resolution=hour 另传含偏移的 since_time 和 until_time，采用左闭右开区间，最长 24 小时。响应含逐来源状态与物理指纹、逐源去重会话数、模型与时段、未定价记录与 token 数。支持 model_filter.include/exclude、provisional、await_refresh。 |
| server.refreshUsageRates | `POST /usage/rates/refresh` | 使用 LiteLLM 模型费率表，24 小时缓存与一分钟刷新下限。下载失败保留已存表，无表时显示 unavailable。主机通过 KAR_USAGE_RATES_URL 指定镜像。推理已含在输出计数，缓存写入按 5 分钟与 1 小时分列计算。供应商申报金额优先，金额分类按比例折算。 |

本地来源包括 Kar 聊天逐流增量（含子会话、供应商交接）、Claude projects、Codex sessions 与 archived_sessions、Pi sessions、Grok updates.jsonl 和显式 MUSE_HOME 下的 session.jsonl。相同原生会话有 transcript 计量时，采用原生记录。Claude 重复内容块按请求和消息 ID 取各计数最大值，Codex 以 response_id 或可对账的上下文快照计量，忽略分叉复制段。Grok 按模型分配剩余申报费用。新增 Kar 用量更新包含原始事件毫秒；老日志使用最近带时间的更新，未包含模型的旧会话展示 unknown。

工作台将规范化计量结果和解析进度写入 `workbench/usage-index-v1.json`。已解析的正文、工具内容和提示词不会进入索引。重复读取可命中缓存；追加时校验已消费前缀的 SHA-256，暂缺换行的尾段留待下次。当前来源下删除的文件仍可从索引恢复已观察的历史。费率源只接收一个 GET 请求。下载表的确定性离线覆盖使用测试自己的本地 HTTP peer。

macOS 编译命令为 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --locked --tests`。集中复验命令为 `cargo test --locked --test chat_restart -- workbench-usage` 与 `cargo test --locked --lib kar_usage_is_scoped_by_open_child_and_handoff`。HTTP 用例使用专用 HOME、原生格式样例与本地费率表，覆盖秋季夏令时回拨的两个独立小时、折算和申报、重复内容块与请求回执、尾段补全、筛选、未定价、删除后重启、刷新下限。该组尚未执行；供应商真实账单与公开费率源连通性需另行采证。

OpenCode 的两个数据库表（message、session_message）与 storage/message 旧记录通过 OPENCODE_DATA_DIR 或 XDG_DATA_HOME 发现，数据库与旧文件按原生消息 ID 去重，v2 分叉另按父会话、序号、创建时间和 through/before 边界消除复制。导入 OpenCode 费用时 reasoning 计入输出，只有网关的免费模型将零费用视为申报。Antigravity 使用 ANTIGRAVITY_DATA_DIR 或本机默认安装目录，读取 gen_metadata、steps 和 trajectory_metadata_blob 的原生 protobuf，合并重试和跨数据库的多键关联，保留可信度更高的模型及时间。Kar 多实例配置也可提供 Codex、Claude、Pi 用量目录。

这两类数据库均使用现有纯 Rust sqlite-core 读取活跃表和已提交 WAL。数据库无 WAL 时按页读，遍历消息表一次只解码一张叶页。存在 WAL 时使用内存快照，主库上限 512 MiB、WAL 上限 128 MiB，超过上限或扫描期间发生写入时来源显示 partial，保留该来源上次可读的计量。集中 HTTP 用例新增 Rust 写入的真实 SQLite 文件，覆盖层级分叉/旧 JSON 去重、每模型申报额、Antigravity 两个表同一轮合并、回退到带时间的原生记录。这是原生格式样例；用户账号账单留待独立验证。

Cursor 从 CURSOR_API_KEY 交换账号令牌，随后从固定的 Cursor Dashboard API 读取个人与团队历史。团队请求始终同时指定当前用户。接口按账单总事件数识别翻页重叠，保留确实发生多次的相同计费事件。以账号标识的 SHA-256 联合主机指纹，来源缓存只持久化散列及规范化计量。await_refresh=true 等待账号读取，读取失败保留该来源旧记录并显示 partial。复验命令为 `cargo test --locked --lib cursor_account_usage_counts_paid_occurrences_across_overlapping_pages`；用本地 HTTP 服务模拟分页、凭证交换和团队限制。真实账号需要主机上的 Cursor API key，与离线协议证据分别登记。

Claude、Codex 和 Pi 的压缩记录同样支持 .jsonl.zst，复用现有纯 Rust zstd 流式解码器。HTTP 用例把同一批已归档的 Codex 请求同时放入压缩与普通文件，核对不重复计量。

## 用量配置版本

`GET /usage/settings` 返回最新 revision 与 settings；加 `?revision=1` 可取回历史版本。默认模型别名与费率覆盖为空，历史保留 92 天。`PUT /usage/settings` 接收 request_id（UUID）、expected_revision 和完整 settings。修改已知版本时递增版本号，文件置换前同步内容，置换后同步目录。重用同一请求与内容时返回第一次写入的版本，跨重启有效；用过的 ID 提交其他内容会被拒绝。过期的 expected_revision 返回 409。Kar 的 `Workbench::usage_settings` 供其他后端调用者读取同一版本。

设置的 model_aliases 可串接，循环路径上的模型保留各自名称。model_filter 始终匹配原始供应商及模型。price_overrides 的输入、输出、读缓存及写缓存价格单位均为每百万 token 美元，模型覆盖对申报金额优先。history_retention_days 可为正整数或 unlimited，清理过期删除文件前再核对当前配置。汇总响应包含 settings_revision。集中 workbench-usage 用例包含两次写入、旧版本回放、重启后重试、冲突、筛选与定价覆盖。
