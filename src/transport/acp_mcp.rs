//! MCP-over-ACP reverse requests are available only after negotiating the opaque server ID.
use super::Command;
use crate::{McpServer, WorkbenchRegistration};
use reqwest::{Client, StatusCode, header::HeaderValue};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) struct Proxy {
    accepted: Arc<AtomicU8>,
    server: String,
    http: Arc<Endpoint>,
    connections: HashMap<String, Arc<Connection>>,
    replies: mpsc::WeakSender<Command>,
    stop: CancellationToken,
    permits: Arc<Semaphore>,
}
#[derive(Default)]
struct Connection {
    session: Mutex<Option<HeaderValue>>,
    sequence: AtomicU64,
    closed: CancellationToken,
}
struct Endpoint {
    client: Client,
    url: String,
    authorization: HeaderValue,
}
type Reply = Result<Value, &'static str>;
fn error(id: &Value, text: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":text}})
}
impl Proxy {
    pub(super) fn new(
        r: &WorkbenchRegistration,
        replies: mpsc::WeakSender<Command>,
    ) -> Option<Self> {
        let McpServer::Http { url, headers } = &r.http else {
            return None;
        };
        let http = Endpoint {
            url: url.clone(),
            authorization: headers.get("Authorization")?.parse().ok()?,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?,
        };
        r.acp.store(1, Ordering::Release);
        Some(Self {
            accepted: r.acp.clone(),
            server: r.acp_id.clone(),
            http: Arc::new(http),
            connections: HashMap::new(),
            replies,
            stop: CancellationToken::new(),
            permits: Arc::new(Semaphore::new(32)),
        })
    }
    pub(super) fn stop_calls(&mut self) {
        self.stop.cancel();
        self.stop = CancellationToken::new();
    }
    // Some(None) consumes a notification or an asynchronously answered request.
    pub(super) fn dispatch(&mut self, packet: &Value) -> Option<Option<Value>> {
        let method = packet["method"].as_str()?;
        if !matches!(method, "mcp/connect" | "mcp/disconnect" | "mcp/message") {
            return None;
        }
        let id = &packet["id"];
        let failure = |s| Some((!id.is_null()).then(|| error(id, s)));
        if self.accepted.load(Ordering::Acquire) != 2 {
            return failure("MCP-over-ACP was not negotiated");
        }
        let p = &packet["params"];
        if method == "mcp/connect" {
            if p["serverId"] != self.server || self.connections.len() >= 16 || id.is_null() {
                return failure("Unknown MCP server or connection limit reached");
            }
            let key = Uuid::new_v4().to_string();
            self.connections.insert(key.clone(), Default::default());
            return Some(Some(
                json!({"jsonrpc":"2.0","id":id,"result":{"connectionId":key}}),
            ));
        }
        let Some(key) = p["connectionId"].as_str() else {
            return failure("Missing MCP connection");
        };
        let Ok(slot) = self.permits.clone().try_acquire_owned() else {
            return failure("Too many MCP requests");
        };
        let disconnect = method == "mcp/disconnect";
        let connection = if disconnect {
            self.connections.remove(key)
        } else {
            self.connections.get(key).cloned()
        };
        let Some(connection) = connection else {
            return failure("Unknown MCP connection");
        };
        if disconnect {
            connection.closed.cancel();
        }
        let http = self.http.clone();
        let replies = self.replies.clone();
        let p = p.clone();
        let id = id.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let result = if disconnect {
                http.close(&connection).await
            } else {
                tokio::select! {
                    _=stop.cancelled()=>Err("MCP call stopped"),
                    _=connection.closed.cancelled()=>Err("MCP connection closed"),
                    result=http.call(&connection,&p,!id.is_null())=>result,
                }
            };
            if !id.is_null()
                && let Some(tx) = replies.upgrade()
            {
                let value = match result {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(text) => error(&id, text),
                };
                let _ = tx.send(Command::WorkbenchMcpReply(value)).await;
            }
        });
        Some(None)
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.cancel();
        for (_, c) in self.connections.drain() {
            c.closed.cancel();
            let http = self.http.clone();
            tokio::spawn(async move {
                let _ = http.close(&c).await;
            });
        }
    }
}
impl Endpoint {
    async fn call(&self, c: &Connection, p: &Value, reply: bool) -> Reply {
        let method = p["method"].as_str().ok_or("Missing MCP method")?;
        if !p["params"].is_null() && !p["params"].is_object() {
            return Err("Invalid MCP params");
        }
        let id = c.sequence.fetch_add(1, Ordering::Relaxed);
        let mut body = json!({"jsonrpc":"2.0","method":method});
        if reply {
            body["id"] = id.into();
        }
        if !p["params"].is_null() {
            body["params"] = p["params"].clone();
        }
        let mut req = self
            .client
            .post(&self.url)
            .header("authorization", &self.authorization)
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        let guard = c.session.lock().await;
        let mut initialization = if method == "initialize" && reply && guard.is_none() {
            Some(guard)
        } else {
            let s = guard.as_ref().ok_or("MCP connection is not initialized")?;
            req = req
                .header("mcp-session-id", s)
                .header("mcp-protocol-version", "2025-06-18");
            drop(guard);
            None
        };
        let response = req
            .send()
            .await
            .map_err(|_| "Workbench did not answer MCP request")?;
        if !response.status().is_success() {
            return Err("Workbench rejected MCP request");
        }
        if let Some(s) = initialization.as_mut() {
            **s = Some(
                response
                    .headers()
                    .get("mcp-session-id")
                    .cloned()
                    .ok_or("Missing MCP session")?,
            );
        }
        if !reply {
            return Ok(Value::Null);
        }
        let body = response
            .json::<Value>()
            .await
            .map_err(|_| "Invalid workbench MCP response")?;
        if body["id"] != id || body.get("error").is_some() {
            return Err("Workbench MCP request failed");
        }
        body.get("result")
            .cloned()
            .ok_or("Missing workbench MCP result")
    }
    async fn close(&self, c: &Connection) -> Reply {
        if let Some(s) = c.session.lock().await.take() {
            let res = self
                .client
                .delete(&self.url)
                .header("authorization", &self.authorization)
                .header("mcp-session-id", s)
                .header("mcp-protocol-version", "2025-06-18")
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|_| "Workbench MCP disconnect failed")?;
            if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
                return Err("Workbench rejected MCP disconnect");
            }
        }
        Ok(json!({}))
    }
}
