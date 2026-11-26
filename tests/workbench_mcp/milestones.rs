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
    let listed_events = list["result"]["events"].as_array().unwrap();
    assert_eq!(listed_events.len(), 12);
    let find_schema = |name: &str| listed_events.iter().find(|e| e["name"] == name).unwrap();
    assert_eq!(
        find_schema("workflow.input_resolved")["payloadSchema"]["properties"]["resolution"]["enum"],
        json!(["answered", "cancelled", "expired"])
    );
    assert_eq!(
        find_schema("workflow.run_ended")["payloadSchema"]["properties"]["outcome"]["enum"],
        json!(["completed", "stopped", "skipped", "cancelled"])
    );
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
    assert_eq!(
        find_schema("thread.run_settled")["delivery"],
        json!(["poll", "push", "webhook"])
    );
    let unsupported = server::json(request(
        c,
        url,
        owner,
        "events/subscribe",
        json!({"name":"thread.run_settled","delivery":{"mode":"email"}}),
    ))
    .await;
    assert_eq!(unsupported["error"]["code"], -32026);
    assert_eq!(unsupported["error"]["data"]["value"], "email");
    let incomplete = server::json(request(
        c,
        url,
        owner,
        "events/subscribe",
        json!({"name":"thread.run_settled","delivery":{"mode":"webhook"}}),
    ))
    .await;
    assert_eq!(incomplete["error"]["code"], -32602);
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
struct SseReader {
    resp: reqwest::Response,
    buf: Vec<u8>,
}
impl SseReader {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            resp,
            buf: Vec::new(),
        }
    }
    async fn next_json(&mut self) -> Value {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let raw = String::from_utf8(self.buf.drain(..pos + 2).collect()).unwrap();
                let data: String = raw
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !data.is_empty() {
                    return serde_json::from_str(&data).unwrap();
                }
            }
            let chunk = self
                .resp
                .chunk()
                .await
                .unwrap()
                .expect("expected SSE frame before stream closed");
            self.buf.extend_from_slice(&chunk);
        }
    }
}

static NESTED_EXPECTATIONS: std::sync::OnceLock<(String, Vec<String>)> = std::sync::OnceLock::new();

async fn wait_run_status(
    c: &Client,
    url: &str,
    owner: &str,
    p: &str,
    run_id: &str,
    want: &str,
) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row = server::json(
                c.get(format!("{url}/projects/{p}/workflow-runs/{run_id}"))
                    .header("Authorization", owner),
            )
            .await;
            if row["status"] == want {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

async fn exercise_nested_and_terminal_milestones(c: &Client, url: &str, owner: &str) {
    let projects = server::json(
        c.get(format!("{url}/projects"))
            .header("Authorization", owner),
    )
    .await;
    let p = projects.as_array().unwrap().first().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let edge = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({"name": "workflow.run_ended", "cursor": null}),
    ))
    .await["result"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // Subscribe TWO external MCP clients:
    // Client A: includeSubagentChildren = true (plus maxAgeMs to verify zero spurious active frames).
    // Client B: includeSubagentChildren = false.
    let mut client_a = SseReader::new(
        request(
            c,
            url,
            owner,
            "events/stream",
            json!({
                "name": "workflow.run_ended",
                "cursor": edge,
                "maxAgeMs": 300,
                "heartbeatMs": 2000,
                "arguments": {"includeSubagentChildren": true}
            }),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap(),
    );
    let mut client_b = SseReader::new(
        request(
            c,
            url,
            owner,
            "events/stream",
            json!({
                "name": "workflow.run_ended",
                "cursor": edge,
                "heartbeatMs": 2000,
                "arguments": {"includeSubagentChildren": false}
            }),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap(),
    );
    let active_a = tokio::time::timeout(Duration::from_secs(5), client_a.next_json())
        .await
        .unwrap();
    assert_eq!(active_a["method"], "notifications/events/active");
    let active_b = tokio::time::timeout(Duration::from_secs(5), client_b.next_json())
        .await
        .unwrap();
    assert_eq!(active_b["method"], "notifications/events/active");

    // Wait >400ms so Client A runs a live tick after maxAgeMs (300ms) has elapsed; must NOT re-emit active.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let save_wf = |id: Uuid, body: Value| {
        c.put(format!("{url}/projects/{p}/workflows/{id}"))
            .header("Authorization", owner)
            .json(&body)
    };
    let child_wf = Uuid::new_v4();
    let parent_wf = Uuid::new_v4();
    let skip_wf = Uuid::new_v4();
    let stop_wf = Uuid::new_v4();
    server::json(save_wf(
        child_wf,
        json!({
            "name": "ms-nested-child",
            "enabled": true,
            "schedule": {"type": "manual"},
            "steps": [{"type": "ask", "question": "Expire child question?", "timeout_ms": 60}]
        }),
    ))
    .await;
    server::json(save_wf(
        parent_wf,
        json!({
            "name": "ms-nested-parent",
            "enabled": true,
            "schedule": {"type": "manual"},
            "steps": [
                {"type": "workflow", "workflow": child_wf, "input": {"from": "parent"}},
                {"type": "ask", "question": "Expire parent question?", "timeout_ms": 60}
            ]
        }),
    ))
    .await;
    server::json(save_wf(
        skip_wf,
        json!({
            "name": "ms-skip-flow",
            "enabled": true,
            "schedule": {"type": "manual"},
            "steps": [{"type": "skip", "reason": "nothing changed in workspace"}]
        }),
    ))
    .await;
    server::json(save_wf(
        stop_wf,
        json!({
            "name": "ms-stop-flow",
            "enabled": true,
            "schedule": {"type": "manual"},
            "steps": [{"type": "ask", "question": "Hold until stopped"}]
        }),
    ))
    .await;

    // 1) Trigger parent workflow -> invokes child workflow -> both questions expire -> both complete.
    let parent_started = server::json(
        c.post(format!("{url}/projects/{p}/workflows/{parent_wf}/runs"))
            .header("Authorization", owner)
            .json(&json!({"operation": Uuid::new_v4(), "input": {"root": true}})),
    )
    .await;
    let parent_run_id = parent_started["id"].as_str().unwrap().to_owned();
    let parent_done = wait_run_status(c, url, owner, &p, &parent_run_id, "completed").await;
    let child_run_id = parent_done["receipts"][0]["result"]["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let parent_ask_attempt = parent_done["receipts"][1]["attempt"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let child_done = wait_run_status(c, url, owner, &p, &child_run_id, "completed").await;
    let child_ask_attempt = child_done["receipts"][0]["attempt"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Client A (includeSubagentChildren: true) receives BOTH child and parent run_ended, then disconnects mid-test!
    let mut a_events = Vec::new();
    let mid_cursor = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let msg = client_a.next_json().await;
            assert_ne!(
                msg["method"], "notifications/events/active",
                "spurious active frame emitted after initial catch-up: {msg}"
            );
            if msg["method"] == "notifications/events/event" {
                let cursor = msg["params"]["cursor"].as_str().unwrap().to_owned();
                let run_id = msg["params"]["data"]["workflowRunId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                a_events.push(msg);
                if run_id == parent_run_id {
                    return cursor;
                }
            }
        }
    })
    .await
    .unwrap();
    drop(client_a);

    let child_ev = a_events
        .iter()
        .find(|m| m["params"]["data"]["workflowRunId"] == child_run_id)
        .expect("Client A must receive child workflow.run_ended");
    assert_eq!(
        child_ev["params"]["eventId"],
        format!("workflow.run_ended:{child_run_id}")
    );
    assert_eq!(
        child_ev["params"]["data"]["parentWorkflowRunId"],
        parent_run_id
    );
    assert_eq!(child_ev["params"]["data"]["isSubagentChild"], true);
    assert_eq!(child_ev["params"]["data"]["outcome"], "completed");

    let parent_ev = a_events
        .iter()
        .find(|m| m["params"]["data"]["workflowRunId"] == parent_run_id)
        .unwrap();
    assert_eq!(
        parent_ev["params"]["eventId"],
        format!("workflow.run_ended:{parent_run_id}")
    );
    assert!(parent_ev["params"]["data"]["parentWorkflowRunId"].is_null());
    assert_eq!(parent_ev["params"]["data"]["isSubagentChild"], false);

    // 2) While Client A is disconnected, trigger:
    //    - HTTP nested child skip run (parent_run_id = Some(parent_run_id))
    //    - Top-level skipped run (Step::Skip with reason)
    //    - Top-level stopped run (Action::Stop with reason)
    let http_child_skip = server::json(
        c.post(format!("{url}/projects/{p}/workflows/{skip_wf}/runs"))
            .header("Authorization", owner)
            .json(&json!({
                "operation": Uuid::new_v4(),
                "input": {},
                "parent_run_id": parent_run_id
            })),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_run_status(c, url, owner, &p, &http_child_skip, "skipped").await;

    let skip_run_id = server::json(
        c.post(format!("{url}/projects/{p}/workflows/{skip_wf}/runs"))
            .header("Authorization", owner)
            .json(&json!({"operation": Uuid::new_v4(), "input": {}})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_run_status(c, url, owner, &p, &skip_run_id, "skipped").await;

    let stop_run_id = server::json(
        c.post(format!("{url}/projects/{p}/workflows/{stop_wf}/runs"))
            .header("Authorization", owner)
            .json(&json!({"operation": Uuid::new_v4(), "input": {}})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_run_status(c, url, owner, &p, &stop_run_id, "asking").await;
    server::json(
        c.post(format!(
            "{url}/projects/{p}/workflow-runs/{stop_run_id}/actions"
        ))
        .header("Authorization", owner)
        .json(&json!({
            "type": "stop",
            "operation": Uuid::new_v4(),
            "reason": "operator halted run"
        })),
    )
    .await;
    wait_run_status(c, url, owner, &p, &stop_run_id, "stopped").await;

    // 3) Reconnect Client A from mid_cursor and verify all 3 missed events arrive without duplicating earlier ones!
    let mut client_a_resumed = SseReader::new(
        request(
            c,
            url,
            owner,
            "events/stream",
            json!({
                "name": "workflow.run_ended",
                "cursor": mid_cursor,
                "heartbeatMs": 2000,
                "arguments": {"includeSubagentChildren": true}
            }),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap(),
    );
    let resumed_active = tokio::time::timeout(Duration::from_secs(5), client_a_resumed.next_json())
        .await
        .unwrap();
    assert_eq!(resumed_active["method"], "notifications/events/active");
    assert_eq!(resumed_active["params"]["truncated"], false);

    let mut resumed_events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let msg = client_a_resumed.next_json().await;
            if msg["method"] == "notifications/events/event" {
                let run_id = msg["params"]["data"]["workflowRunId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                resumed_events.push(msg);
                if run_id == stop_run_id {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    drop(client_a_resumed);

    let resumed_ids: Vec<&str> = resumed_events
        .iter()
        .map(|m| m["params"]["data"]["workflowRunId"].as_str().unwrap())
        .collect();
    assert_eq!(
        resumed_ids,
        vec![
            http_child_skip.as_str(),
            skip_run_id.as_str(),
            stop_run_id.as_str()
        ]
    );
    assert_eq!(
        resumed_events[0]["params"]["data"]["parentWorkflowRunId"],
        parent_run_id
    );
    assert_eq!(resumed_events[0]["params"]["data"]["isSubagentChild"], true);
    assert_eq!(resumed_events[0]["params"]["data"]["outcome"], "skipped");
    assert_eq!(resumed_events[1]["params"]["data"]["outcome"], "skipped");
    assert_eq!(
        resumed_events[1]["params"]["data"]["detail"],
        "nothing changed in workspace"
    );
    assert_eq!(resumed_events[2]["params"]["data"]["outcome"], "stopped");
    assert_eq!(
        resumed_events[2]["params"]["data"]["detail"],
        "operator halted run"
    );

    // 4) Client B (includeSubagentChildren: false) stayed connected the whole time and MUST NOT receive child runs!
    let mut b_events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let msg = client_b.next_json().await;
            if msg["method"] == "notifications/events/event" {
                let run_id = msg["params"]["data"]["workflowRunId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                b_events.push(msg);
                if run_id == stop_run_id {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    drop(client_b);
    let b_run_ids: Vec<&str> = b_events
        .iter()
        .map(|m| m["params"]["data"]["workflowRunId"].as_str().unwrap())
        .collect();
    assert_eq!(
        b_run_ids,
        vec![
            parent_run_id.as_str(),
            skip_run_id.as_str(),
            stop_run_id.as_str()
        ]
    );
    assert!(
        b_events
            .iter()
            .all(|m| m["params"]["data"]["isSubagentChild"] == false)
    );

    // 5) Poll workflow.input_resolved from edge with includeSubagentChildren: true vs false.
    let resolved_all = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({
            "name": "workflow.input_resolved",
            "cursor": edge,
            "arguments": {"includeSubagentChildren": true}
        }),
    ))
    .await;
    let res_events = resolved_all["result"]["events"].as_array().unwrap();
    let find_res = |rid: &str, step: &str| {
        res_events
            .iter()
            .find(|e| e["eventId"] == format!("workflow.input_resolved:{rid}:{step}"))
            .expect("expected deterministic workflow.input_resolved eventId")
    };
    let child_expired = find_res(&child_run_id, &child_ask_attempt);
    assert_eq!(child_expired["data"]["resolution"], "expired");
    assert_eq!(child_expired["data"]["parentWorkflowRunId"], parent_run_id);
    assert_eq!(child_expired["data"]["isSubagentChild"], true);

    let parent_expired = find_res(&parent_run_id, &parent_ask_attempt);
    assert_eq!(parent_expired["data"]["resolution"], "expired");
    assert!(parent_expired["data"]["parentWorkflowRunId"].is_null());
    assert_eq!(parent_expired["data"]["isSubagentChild"], false);

    let stop_cancelled = res_events
        .iter()
        .find(|e| e["data"]["workflowRunId"] == stop_run_id)
        .unwrap();
    assert_eq!(stop_cancelled["data"]["resolution"], "cancelled");

    let resolved_top = server::json(request(
        c,
        url,
        owner,
        "events/poll",
        json!({
            "name": "workflow.input_resolved",
            "cursor": edge,
            "arguments": {"includeSubagentChildren": false}
        }),
    ))
    .await;
    assert!(
        resolved_top["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["data"]["workflowRunId"] != child_run_id)
    );

    let expected_ended_ids = vec![
        format!("workflow.run_ended:{child_run_id}"),
        format!("workflow.run_ended:{parent_run_id}"),
        format!("workflow.run_ended:{http_child_skip}"),
        format!("workflow.run_ended:{skip_run_id}"),
        format!("workflow.run_ended:{stop_run_id}"),
    ];
    let _ = NESTED_EXPECTATIONS.set((edge, expected_ended_ids));
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
    assert_eq!(
        id,
        format!(
            "workflow.run_started:{}",
            event["data"]["workflowRunId"].as_str().unwrap()
        )
    );
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
    if first.is_none() {
        exercise_nested_and_terminal_milestones(c, url, owner).await;
    } else if let Some((edge, expected_ids)) = NESTED_EXPECTATIONS.get() {
        let replayed = server::json(request(
            c,
            url,
            owner,
            "events/poll",
            json!({
                "name": "workflow.run_ended",
                "cursor": edge,
                "arguments": {"includeSubagentChildren": true}
            }),
        ))
        .await;
        let seen_ids: Vec<&str> = replayed["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["eventId"].as_str().unwrap())
            .collect();
        let mut unique = std::collections::BTreeSet::new();
        for eid in &seen_ids {
            assert!(
                unique.insert(*eid),
                "duplicate eventId after restart: {eid}"
            );
        }
        for exp in expected_ids {
            assert!(
                unique.contains(exp.as_str()),
                "missing expected eventId after restart: {exp}"
            );
        }
    }
    id.into()
}
