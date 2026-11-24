//! External event discovery, filtered catch-up, live admission and stable cursors after restart.
use super::*;
fn request(c: &Client, url: &str, auth: &str, method: &str, mut params: Value) -> RequestBuilder {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"extensions":{"io.modelcontextprotocol/events":{}}}});
    let mut r = rpc(c, url, auth, method, params.clone())
        .header("mcp-method", method)
        .header("mcp-protocol-version", "2026-07-28")
        .header("accept", "application/json, text/event-stream");
    if let Some(name) = params["name"].as_str() {
        r = r.header("mcp-name", name);
    }
    r
}
pub(super) async fn begin(
    c: &Client,
    url: &str,
    owner: &str,
    provider: &str,
) -> (String, reqwest::Response) {
    let supported = server::json(request(c, url, owner, "server/discover", json!({}))).await;
    assert!(
        supported
            .pointer("/result/capabilities/extensions/io.modelcontextprotocol~1events")
            .is_some()
    );
    let list = server::json(request(c, url, owner, "events/list", json!({}))).await;
    assert_eq!(list["result"]["events"].as_array().unwrap().len(), 12);
    let invisible = server::json(request(c, url, provider, "events/list", json!({}))).await;
    assert_eq!(invisible["result"]["events"], json!([]));
    let bad = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({"name":"no.such.event","cursor":null}),
    ))
    .await;
    assert_eq!(bad["error"]["code"], -32023);
    let hidden = server::json(request(
        c,
        url,
        provider,
        "events/poll",
        json!({"name":"thread.run_settled","cursor":null}),
    ))
    .await;
    assert_eq!(hidden["error"]["code"], -32023);
    let unsupported = server::json(request(
        c,
        url,
        owner,
        "events/subscribe",
        json!({"name":"thread.run_settled","delivery":{"mode":"webhook"}}),
    ))
    .await;
    assert_eq!(unsupported["error"]["code"], -32026);
    assert_eq!(unsupported["error"]["data"]["value"], "webhook");
    let first = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({"name":"workflow.run_started","cursor":null}),
    ))
    .await;
    assert_eq!(first["result"]["events"], json!([]));
    let mut stream = request(
        c,
        url,
        owner,
        "events/stream",
        json!({"name":"workflow.run_started","cursor":null,"heartbeatMs":5000}),
    )
    .send()
    .await
    .unwrap();
    let mut text = vec![];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !String::from_utf8_lossy(&text).contains("notifications/events/active") {
            text.extend(stream.chunk().await.unwrap().expect("active"));
        }
    })
    .await
    .unwrap();
    (
        first["result"]["cursor"].as_str().unwrap().to_owned(),
        stream,
    )
}
pub(super) async fn observe_live(mut stream: reqwest::Response, expected_id: &str) {
    let mut text = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            text.extend(stream.chunk().await.unwrap().expect("live stream"));
            let read = String::from_utf8_lossy(&text);
            if read.contains("notifications/events/event")
                && read.contains(expected_id)
                && read.contains("notifications/events/heartbeat")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
}
pub(super) async fn recover(
    c: &Client,
    url: &str,
    owner: &str,
    cursor: &str,
    first: Option<&str>,
) -> String {
    let page = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({"name":"workflow.run_started","cursor":cursor,"maxEvents":1}),
    ))
    .await;
    let event = &page["result"]["events"][0];
    let id = event["eventId"].as_str().unwrap();
    assert_eq!(event["data"]["type"], "workflow.run_started");
    if let Some(first) = first {
        assert_eq!(id, first);
    }
    let filtered=server::json(request(c,url,owner,"events/poll",json!({"name":"workflow.run_started","cursor":cursor,"arguments":{"workflowRunIds":[event["data"]["workflowRunId"]]}}))).await;
    assert_eq!(filtered["result"]["events"].as_array().unwrap().len(), 1);
    let rest = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({"name":"workflow.run_started","cursor":page["result"]["cursor"]}),
    ))
    .await;
    assert!(
        rest["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["eventId"] != id)
    );
    let ancient=server::json(request(c,url,owner,"events/poll",json!({"name":"thread.run_settled","cursor":"previous-boot.1","arguments":{"includeSubagentChildren":true}}))).await;
    assert_eq!(ancient["result"]["truncated"], true);
    assert!(
        ancient["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["outcome"] == "completed")
    );
    id.into()
}
