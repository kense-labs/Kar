use super::*;

pub(super) async fn controls(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let tab = Uuid::new_v4();
    let set = |mode: &str| {
        host.put(&policy)
            .json(&json!({"mode":mode,"preview_tools":{"tabs":[tab]}}))
    };
    assert!(server::json(call("kar_preview_list", json!({}))).await["error"].is_object());
    server::json(set("plan")).await;
    let list = server::json(call("kar_preview_list", json!({}))).await;
    assert_eq!(list["result"]["structuredContent"]["sessions"], json!([]));
    assert!(
        server::json(call("kar_preview_close", json!({"tab_id":tab}))).await["error"].is_object()
    );
    server::json(set("execute")).await;
    let close = call("kar_preview_close", json!({"tab_id":tab}));
    assert_eq!(server::json(close).await["result"]["isError"], true);
    let restricted = call("kar_preview_close", json!({"tab_id":tab}))
        .header("kar-mode-limit", "full-access/plan");
    assert!(server::json(restricted).await["error"].is_object());
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}
