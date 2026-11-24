use super::*;
use serde::Deserialize;
use std::{collections::BTreeSet, time::Duration};
mod store;
pub(in crate::workbench) use store::{Hub, Log};
const TYPES: &[&str] = &[
    "thread.run_settled",
    "thread.needs_input",
    "thread.input_resolved",
    "thread.guest_request",
    "thread.guest_request_resolved",
    "pr.watch_wake",
    "workflow.run_started",
    "workflow.needs_input",
    "workflow.input_resolved",
    "workflow.run_paused",
    "workflow.run_resumed",
    "workflow.run_ended",
];
pub(super) fn capabilities(c: &Caller, mut base: Value) -> Value {
    if c.grant.is_none() {
        base["extensions"]["io.modelcontextprotocol/events"] = json!({});
    }
    base
}
#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct Args {
    project_ids: Option<BTreeSet<Uuid>>,
    thread_ids: Option<BTreeSet<Uuid>>,
    workflow_run_ids: Option<BTreeSet<Uuid>>,
    workflows: Option<BTreeSet<String>>,
    include_subagent_children: bool,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    name: String,
    cursor: Option<String>,
    max_age_ms: Option<u64>,
    max_events: Option<usize>,
    heartbeat_ms: Option<u64>,
    #[serde(default)]
    arguments: Args,
}
impl Params {
    fn limit(&self) -> usize {
        self.max_events.unwrap_or(100).min(1000)
    }
    fn matches(&self, v: &Value) -> bool {
        let wanted = |set: &Option<BTreeSet<Uuid>>, key: &str| {
            set.as_ref().is_none_or(|s| {
                v[key]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .is_some_and(|id| s.contains(&id))
            })
        };
        v["type"] == self.name
            && (self.arguments.include_subagent_children || v["isSubagentChild"] != true)
            && wanted(&self.arguments.project_ids, "projectId")
            && wanted(&self.arguments.thread_ids, "threadId")
            && wanted(&self.arguments.workflow_run_ids, "workflowRunId")
            && self
                .arguments
                .workflows
                .as_ref()
                .is_none_or(|ws| v["workflow"].as_str().is_some_and(|s| ws.contains(s)))
    }
}
struct Page {
    cursor: String,
    events: Vec<(String, Value)>,
    truncated: bool,
    has_more: bool,
}
impl Page {
    fn replay_start(&self) -> String {
        self.events
            .first()
            .and_then(|(c, _)| c.rsplit_once('.'))
            .and_then(|(epoch, n)| {
                n.parse::<u64>()
                    .ok()
                    .map(|n| format!("{epoch}.{}", n.saturating_sub(1)))
            })
            .unwrap_or_else(|| self.cursor.clone())
    }
}
fn typed(http: StatusCode, id: &Value, code: i32, message: &str, data: Value) -> Response {
    (
        http,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})),
    )
        .into_response()
}
async fn read(app: Workbench, p: Params) -> crate::Result<Page> {
    tokio::task::spawn_blocking(move || app.0.milestone_hub.read(&app, &p))
        .await
        .map_err(|e| crate::Error::Closed(e.to_string()))?
}
fn description(name: &str) -> Value {
    let s = json!({"type":"string"});
    let list = json!({"type":"array","items":{"type":"string"}});
    let mut args = json!({"workflowRunIds":list,"includeSubagentChildren":{"type":"boolean"}});
    let mut fields = json!({"id":s,"type":{"const":name},"occurredAt":s,"projectId":{"type":["string","null"]},"threadId":{"type":["string","null"]},"parentThreadId":{"type":["string","null"]},"actor":{"type":"object"},"isSubagentChild":{"type":"boolean"},"workflowRunId":s});
    if name.starts_with("workflow.") {
        args["workflows"] = list;
        for key in ["workflow", "runTitle", "parentWorkflowRunId"] {
            fields[key] = s.clone();
        }
    } else {
        args["projectIds"] = list.clone();
        args["threadIds"] = list;
    }
    let (desc, keys) = match name {
        "thread.run_settled" => ("A chat turn ended.", vec!["runId", "outcome"]),
        "thread.needs_input" => (
            "A chat needs an answer or approval.",
            vec!["requestId", "kind"],
        ),
        "thread.input_resolved" => (
            "A pending chat request was answered or expired.",
            vec!["requestId", "kind", "resolution"],
        ),
        "thread.guest_request" => (
            "A guest submitted a request.",
            vec!["shareId", "guestMessageId"],
        ),
        "thread.guest_request_resolved" => (
            "A guest request was delivered, dismissed, or closed.",
            vec!["shareId", "guestMessageId", "resolution"],
        ),
        "pr.watch_wake" => (
            "A pull request observation woke its chat.",
            vec!["outcome", "summary"],
        ),
        "workflow.needs_input" => ("A workflow asked a question.", vec!["step", "question"]),
        "workflow.input_resolved" => (
            "A workflow question was answered or cancelled.",
            vec!["step", "resolution"],
        ),
        "workflow.run_paused" => ("A workflow needs retry or reconciliation.", vec!["reason"]),
        "workflow.run_ended" => ("A workflow ended.", vec!["outcome", "detail"]),
        "workflow.run_started" => ("A workflow started.", vec![]),
        _ => ("A workflow resumed.", vec![]),
    };
    for key in keys {
        fields[key] = s.clone();
    }
    if name.starts_with("workflow.") {
        fields["parentWorkflowRunId"] = json!({"type":["string","null"]});
    }
    if name == "workflow.run_ended" {
        fields["detail"] = json!({"type":["string","null"]});
    }
    if name == "pr.watch_wake" {
        fields["watchEnded"] = json!({"type":"boolean"});
        fields["pullRequests"] = json!({"type":"array","items":{"type":"object"}});
    }
    json!({"name":name,"description":desc,"delivery":["poll","push"],"inputSchema":{"type":"object","properties":args,"additionalProperties":false},"payloadSchema":{"type":"object","properties":fields}})
}
pub(super) async fn rpc(
    app: Workbench,
    c: &Caller,
    h: &HeaderMap,
    method: &str,
    p: &Value,
    id: &Value,
    stateless: bool,
    session: Option<Arc<registry::HttpSession>>,
) -> Response {
    let external = c.grant.is_none();
    if method == "events/list" {
        let events = if external {
            TYPES.iter().map(|s| description(s)).collect()
        } else {
            Vec::new()
        };
        return response(id, json!({"events":events}), stateless);
    }
    let name = p["name"].as_str().unwrap_or("");
    if stateless && (name.is_empty() || hdr(h, "mcp-name") != Some(name)) {
        return typed(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "Mcp-Name must match the event name",
            Value::Null,
        );
    }
    if !external || !TYPES.contains(&name) {
        return typed(
            StatusCode::OK,
            id,
            -32023,
            "Unknown event",
            json!({"kind":"event","name":name}),
        );
    }
    if method == "events/subscribe" {
        return typed(
            StatusCode::OK,
            id,
            -32026,
            "Unsupported delivery mode",
            json!({"feature":"deliveryMode","value":p.pointer("/delivery/mode")}),
        );
    }
    if !matches!(method, "events/poll" | "events/stream") {
        return error(StatusCode::OK, id, -32601, "Unknown events method");
    }
    let mut input = p.clone();
    if let Some(obj) = input.as_object_mut() {
        obj.remove("_meta");
    }
    let Ok(args) = serde_json::from_value::<Params>(input) else {
        return error(StatusCode::OK, id, -32602, "Invalid event arguments");
    };
    if args.max_events == Some(0)
        || args.cursor.as_ref().is_some_and(|s| s.len() > 512)
        || args.max_age_ms.is_some_and(|v| v > i64::MAX as u64)
        || (name.starts_with("workflow.")
            && (args.arguments.thread_ids.is_some() || args.arguments.project_ids.is_some()))
        || (!name.starts_with("workflow.") && args.arguments.workflows.is_some())
    {
        return error(StatusCode::OK, id, -32602, "Invalid event filter or bounds");
    }
    let first = match read(app.clone(), args.clone()).await {
        Ok(v) => v,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                id,
                -32603,
                &e.to_string(),
            );
        }
    };
    if method == "events/poll" {
        return response(
            id,
            json!({"cursor":first.cursor,"truncated":first.truncated,"hasMore":first.has_more,"events":first.events.into_iter().map(|(_,v)|v).collect::<Vec<_>>()}),
            stateless,
        );
    }
    let hb = args.heartbeat_ms.unwrap_or(30_000).clamp(1000, 300_000);
    let id = id.clone();
    let notify = move |method: &str, mut params: Value| {
        params["_meta"] = json!({"io.modelcontextprotocol/subscriptionId":id});
        Event::default()
            .event("message")
            .json_data(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .unwrap()
    };
    let lifetime = session.as_ref().map(|s| s.closed.clone());
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let mut args = args;
        let mut page = first;
        let mut announce = true;
        let mut last = tokio::time::Instant::now();
        loop {
            if tx.is_closed() || session.as_ref().is_some_and(|s| s.closed.is_cancelled()) {
                break;
            }
            if announce || page.truncated {
                if tx
                    .send(notify(
                        "notifications/events/active",
                        json!({
                            "cursor":page.replay_start(),"truncated":page.truncated,"heartbeatMs":hb
                        }),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                announce = false;
                last = tokio::time::Instant::now();
            }
            for (cursor, mut item) in page.events {
                item["cursor"] = json!(cursor);
                item["name"] = json!(args.name);
                if tx
                    .send(notify("notifications/events/event", item))
                    .await
                    .is_err()
                {
                    return;
                }
                last = tokio::time::Instant::now();
            }
            args.cursor = Some(page.cursor);
            if last.elapsed() >= Duration::from_millis(hb) {
                if tx
                    .send(notify(
                        "notifications/events/heartbeat",
                        json!({"cursor":args.cursor}),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                last = tokio::time::Instant::now();
            }
            if !page.has_more {
                tokio::select! {_=tokio::time::sleep(Duration::from_millis(400))=>{},_=tx.closed()=>break}
            }
            page = match read(app.clone(), args.clone()).await {
                Ok(v) => v,
                Err(_) => {
                    let _ = tx.send(notify("notifications/events/terminated",
                        json!({"error":{"code":-32603,"message":"Kar could not read milestone history"}}))).await;
                    break;
                }
            };
        }
    });
    let output = futures_util::stream::unfold(rx, |mut rx| async move {
        Some((Ok::<_, Infallible>(rx.recv().await?), rx))
    });
    use futures_util::StreamExt;
    let revoked = async move {
        if let Some(token) = lifetime {
            token.cancelled().await
        } else {
            std::future::pending::<()>().await
        }
    };
    Sse::new(output.take_until(revoked)).into_response()
}
