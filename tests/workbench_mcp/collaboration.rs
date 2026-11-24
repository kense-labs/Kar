use super::*;
use std::path::Path;
pub(super) async fn run(
    url: &str,
    host: &Client,
    p: &str,
    chat: Uuid,
    db: &Path,
    call: impl Fn(&str, Value) -> RequestBuilder,
) {
    let invoke =
        |name: &str, args: Value| call("tools/call", json!({"name":name,"arguments":args}));
    let policy = format!("{url}/projects/{p}/chats/{chat}/model-tools/permissions");
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
    let spec = json!({"name":"notes.txt","mime":"text/plain","kind":"file","bytes":3});
    assert!(
        server::json(invoke("kar_attachment_prepare_upload", spec.clone())).await["error"]
            .is_object()
    );
    server::json(host.put(&policy).json(&json!({"mode":"execute"}))).await;
    let foreign = server::json(
        host.post(format!("{url}/projects/{p}/attachments/uploads"))
            .json(&spec),
    )
    .await;
    let denied = server::json(invoke(
        "kar_attachment_discard",
        json!({"attachment_id":foreign["id"]}),
    ))
    .await;
    assert_eq!(denied["result"]["isError"], true);
    let made = server::json(invoke("kar_attachment_prepare_upload", spec.clone())).await;
    let ticket = &made["result"]["structuredContent"];
    assert!(ticket["upload_url"].as_str().unwrap().starts_with(url));
    Client::new()
        .put(ticket["upload_url"].as_str().unwrap())
        .body("abc")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let sent = Uuid::new_v4();
    for _ in 0..2 {
        let receipt = server::json(invoke(
            "kar_thread_send_attachments",
            json!({"attachment_ids":[ticket["id"]],"input_id":sent,"text":"notes"}),
        ))
        .await;
        assert_eq!(
            receipt["result"]["structuredContent"]["id"],
            sent.to_string(),
            "{receipt}"
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let snap =
                server::json(host.get(format!("{url}/projects/{p}/chats/{chat}/snapshot"))).await;
            if snap["history"]["turns"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|t| t["id"] == sent.to_string() && t["status"] == "completed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let request = Uuid::new_v4();
    fs::write(db.join("hold-collaboration"), "").unwrap();
    let message=host.post(format!("{url}/projects/{p}/chats/{chat}/messages")).json(&json!({"id":request,"text":"mcp collaboration run","requested_output_schema":{"type":"object","properties":{"done":{"const":true}},"required":["done"],"additionalProperties":false}}));
    let called = tokio::spawn(async move { server::json(message).await });
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while !db.join("collaboration-ready").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    let bad = server::json(invoke(
        "kar_submit_output",
        json!({"json":"{\"done\":false}"}),
    ))
    .await;
    assert_eq!(bad["result"]["isError"], true);
    let show = server::json(invoke(
        "kar_send_to_user",
        json!({"message":"### Exact draft\n\nSecond line."}),
    ))
    .await;
    assert_eq!(
        show["result"]["structuredContent"]["turn"],
        request.to_string(),
        "{show}"
    );
    let done = server::json(invoke(
        "kar_submit_output",
        json!({"json":"{\"done\":true}"}),
    ))
    .await;
    assert_eq!(
        done["result"]["structuredContent"]["turn"],
        request.to_string()
    );
    fs::remove_file(db.join("hold-collaboration")).unwrap();
    let turn = called.await.unwrap();
    assert_eq!(turn["structured_output"], json!({"done":true}), "{turn}");
    assert!(turn.to_string().contains("Exact draft"));
}
