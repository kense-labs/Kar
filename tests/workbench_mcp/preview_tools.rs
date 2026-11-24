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

pub(super) async fn devices(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    server::json(
        host.post(format!("{url}/devices/configure"))
            .json(&json!({"enabled":true,"agent_access_enabled":true})),
    )
    .await;
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"device_hosts":["local"]}})),
    )
    .await;
    let catalog = server::json(call("kar_device_list", json!({}))).await;
    let udid = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    assert!(
        catalog["result"]["structuredContent"]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == udid)
    );
    let target = json!({"host_id":"local","device_id":udid});
    let open = server::json(call("kar_device_open", target.clone())).await;
    let view = &open["result"]["structuredContent"];
    assert!(view["view_id"].is_string());
    let again = server::json(call("kar_device_open", target.clone())).await;
    assert_eq!(
        again["result"]["structuredContent"]["view_id"],
        view["view_id"]
    );
    let screenshot = server::json(call("kar_device_screenshot", json!({}))).await;
    assert_eq!(screenshot["result"]["content"][1]["type"], "image");
    assert_eq!(
        screenshot["result"]["structuredContent"]["screenshot"]["width"],
        1
    );
    assert_eq!(
        server::json(call("kar_device_screenshot", json!({"host_id":"another"}))).await["result"]["isError"],
        true
    );
    let cfg = view["agent_device"]["args"][2].as_str().unwrap();
    let checked = |path: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_kar"))
            .args(["device-agent", "--config", path, "detail"])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(checked(cfg));
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"plan","preview_tools":{"device_hosts":["local"]}})),
    )
    .await;
    assert!(!checked(cfg), "old launcher retained broader authorization");
    assert!(server::json(call("kar_device_close", target.clone())).await["error"].is_object());
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"device_hosts":["local"]}})),
    )
    .await;
    let closed = server::json(call("kar_device_close", target)).await;
    assert_eq!(closed["result"]["structuredContent"]["closed"], 1);
    assert!(!std::path::Path::new(cfg).exists());
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}
