# 子任务完成投递

Kar 保存委派操作发生时的父回合 UUID。默认 `settled_only` 在父回合仍运行时保留结果供模型读取。切换到 `always` 可提前把结果加入父会话的原生输入队列。来自同一父回合且尚未开始发送的兄弟结果会合并成一条消息。开始原生投递后，消息内容不再变化，后续结果等待下一条通知。

`kar_delegate_task` 可传 `completion_wake`。`kar_task_delivery` 接受 `task_id`、`command_id` 及 `action`（`wake_policy`、`acknowledge` 或 `dispose`）。工作台可通过 `GET /projects/{project}/chats/{parent}/delegations/{task}` 查看状态，或向同一路径 POST 命令。按实际父回合观察了结果才记录 `observed_by_run_id`。尚未开始发送的通知只保留未处理任务，空通知会撤回。

`completion_delivery` 分别记录消息 UUID、当前队列状态、原生确认及显式处理结果。已入队、原生是否收到、父回合是否观察到可以独立读回。远端工作完成后，发起端的父会话处理通知。接收端不唤醒本地配对模板。旧任务若只有历史布尔值，既无相应队列记录也无可验证的父回合，保留原因并抑制自动重发。

工作台固定提交为 `2808e878de91aacc7fb82887f18414976be7ae34`。对照函数包括 `dispatchDelegatedTaskCompletionDeliveryResolution`、`dispatchDelegatedTaskWakePolicy`、`planDelegatedCompletionDelivery` 和 `finalizeDelegatedCompletionDelivery`。

macOS arm64 的开发编译命令为 `cargo check --locked --offline --lib --tests --message-format=short`。正常 HTTP/MCP 路径的回归位于 `tests/workbench_mcp/orchestrator.rs`，复验命令为 `cargo test --locked --test workbench_mcp`。该场景使用 Codex 协议测试进程，验证两个子任务共享通知、读取与处置缩减消息、父回合结束后的原生回执。供应商实机、强杀恢复和成对远端工作台的网络中断列入集中验收。
