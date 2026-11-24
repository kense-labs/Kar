use super::{HELD, LAST, NEXT, SEED};
use axum::{
    Router,
    extract::{Json, State},
    response::IntoResponse,
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone)]
struct Model {
    calls: Arc<Mutex<Vec<&'static str>>>,
    released: watch::Sender<bool>,
}
pub(super) struct Fixture {
    pub origin: String,
    model: Model,
    server: JoinHandle<()>,
}
impl Fixture {
    pub async fn start() -> Self {
        let model = Model {
            calls: Default::default(),
            released: watch::channel(false).0,
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(reply))
            .route("/v1/responses", post(reply))
            .with_state(model.clone());
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", socket.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        Self {
            origin,
            model,
            server,
        }
    }
    pub fn calls(&self) -> Vec<&'static str> {
        self.model.calls.lock().unwrap().clone()
    }
    pub fn release(&self) {
        self.model.released.send_replace(true);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn reply(State(model): State<Model>, Json(input): Json<Value>) -> impl IntoResponse {
    let kimi = input["messages"].is_array();
    let key = if kimi { "messages" } else { "input" };
    let last = input[key]
        .as_array()
        .and_then(|a| a.iter().rev().find(|r| r["role"] == "user"))
        .map(|r| r["content"].to_string())
        .unwrap_or_default();
    let marker = [SEED, HELD, NEXT, LAST]
        .into_iter()
        .filter_map(|m| last.rfind(m).map(|at| (at, m)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, m)| m);
    if let Some(m) = marker {
        model.calls.lock().unwrap().push(m);
        if m == HELD {
            model.released.subscribe().wait_for(|v| *v).await.unwrap();
        }
    }
    let message = "done";
    let id = uuid::Uuid::new_v4().simple().to_string();
    let body = if kimi {
        let mut sse = String::new();
        for (delta, reason) in [
            (json!({"role":"assistant","content":message}), Value::Null),
            (json!({}), json!("stop")),
        ] {
            let chunk = json!({"id":format!("chatcmpl_{id}"),"object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
            sse.push_str(&format!("data: {chunk}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");
        sse
    } else {
        let resp = format!("resp_{id}");
        let item_id = format!("msg_{id}");
        let item = json!({"type":"message","id":item_id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":message,"annotations":[]}]});
        let result = json!({"id":resp,"object":"response","status":"completed","model":"gpt-5.1","output":[item],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}});
        [
            json!({"type":"response.created","response":{"id":resp,"status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":item_id,"role":"assistant","status":"in_progress","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":item_id,"delta":message}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":result}),
        ].iter().map(|e|format!("event: {}\ndata: {e}\n\n",e["type"].as_str().unwrap())).collect()
    };
    ([("content-type", "text/event-stream")], body)
}
