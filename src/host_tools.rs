//! Serve application-owned Rust functions to local agent sessions over MCP.
mod http;

use crate::{Error, McpServer, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap, hash_map::Entry},
    future::Future,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
};
pub use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HostToolContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Debug, Serialize)]
pub struct HostToolResult {
    pub content: Vec<HostToolContent>,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}
impl From<String> for HostToolResult {
    fn from(text: String) -> Self {
        Self {
            content: vec![HostToolContent::Text { text }],
            is_error: false,
        }
    }
}
impl From<&str> for HostToolResult {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

type Reply = std::result::Result<HostToolResult, String>;
type Handler =
    dyn Fn(Map<String, Value>, CancellationToken) -> BoxFuture<'static, Reply> + Send + Sync;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostTool {
    pub name: String,
    pub input_schema: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Map<String, Value>>,
    #[serde(skip)]
    handler: Box<Handler>,
}
impl HostTool {
    /// The schema and annotations are advertised as supplied. The handler owns input validation.
    pub fn new<F, Fut>(
        name: impl Into<String>,
        input_schema: Map<String, Value>,
        handler: F,
    ) -> Self
    where
        F: Fn(Map<String, Value>, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        Self {
            name: name.into(),
            input_schema,
            description: None,
            annotations: None,
            handler: Box::new(move |args, signal| Box::pin(handler(args, signal))),
        }
    }
}

type Tools = IndexMap<String, Arc<HostTool>>;
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn check_tools(tools: Vec<HostTool>) -> Result<Tools> {
    let mut checked = IndexMap::new();
    for tool in tools {
        if !valid_name(&tool.name) || tool.name.len() > 64 || checked.contains_key(&tool.name) {
            return Err(Error::Protocol(format!(
                "invalid or duplicate MCP tool name {:?}",
                tool.name
            )));
        }
        checked.insert(tool.name.clone(), Arc::new(tool));
    }
    Ok(checked)
}

pub struct ServedMcpTools {
    pub server: McpServer,
    state: Arc<State>,
    listener: Option<JoinHandle<()>>,
}
impl ServedMcpTools {
    /// Insert this entry into each participating session's `mcp_servers`.
    pub fn entry(&self) -> (String, McpServer) {
        (self.state.name.clone(), self.server.clone())
    }

    pub fn set_tools(&self, tools: Vec<HostTool>) -> Result<()> {
        *self.state.tools.write().unwrap() = check_tools(tools)?;
        self.state.changes.send_modify(|revision| *revision += 1);
        Ok(())
    }
    pub async fn close(mut self) {
        self.state.shutdown.cancel();
        if let Some(listener) = self.listener.take() {
            let _ = listener.await;
        }
    }
}
impl Drop for ServedMcpTools {
    fn drop(&mut self) {
        self.state.shutdown.cancel();
    }
}

pub async fn serve_mcp_tools(
    name: impl Into<String>,
    tools: Vec<HostTool>,
    instructions: Option<String>,
) -> Result<ServedMcpTools> {
    let name = name.into();
    if !valid_name(&name) || crate::options::instructions::reserved_server(&name) {
        return Err(Error::Protocol(format!(
            "invalid or reserved MCP server name {name:?}"
        )));
    }
    let tools = check_tools(tools)?;
    let mut secret = [0; 32];
    getrandom::fill(&mut secret).map_err(std::io::Error::other)?;
    let authorization = format!("Bearer {}", URL_SAFE_NO_PAD.encode(secret));
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let server = McpServer::HostedHttp {
        url: format!("http://{}/mcp", listener.local_addr()?),
        headers: BTreeMap::from([("Authorization".into(), authorization.clone())]),
    };
    let state = Arc::new(State {
        name,
        instructions,
        authorization,
        tools: RwLock::new(tools),
        changes: watch::channel(0).0,
        calls: Mutex::new(HashMap::new()),
        shutdown: CancellationToken::new(),
    });
    let task = tokio::spawn(http::listen(listener, Arc::clone(&state)));
    Ok(ServedMcpTools {
        server,
        state,
        listener: Some(task),
    })
}

type CallKey = (String, String);
struct State {
    name: String,
    instructions: Option<String>,
    authorization: String,
    tools: RwLock<Tools>,
    changes: watch::Sender<u64>,
    calls: Mutex<HashMap<CallKey, CancellationToken>>,
    shutdown: CancellationToken,
}

struct Call<'a> {
    state: &'a State,
    key: CallKey,
}
impl Drop for Call<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.state.calls.lock().unwrap().remove(&self.key) {
            token.cancel();
        }
    }
}
fn key(session: &str, id: &Value) -> Option<CallKey> {
    (id.is_string() || id.is_number()).then(|| (session.to_owned(), id.to_string()))
}
fn rpc_error(code: i32, message: impl Into<String>) -> Value {
    json!({"code":code,"message":message.into()})
}
impl State {
    async fn answer(&self, session: &str, msg: &Value) -> Option<Value> {
        let method = msg["method"].as_str()?;
        let params = &msg["params"];
        let id = &msg["id"];
        let Some(call_key) = key(session, id) else {
            if method == "notifications/cancelled"
                && let Some(key) = key(session, &params["requestId"])
                && let Some(signal) = self.calls.lock().unwrap().get(&key)
            {
                signal.cancel();
            }
            return None;
        };
        let result = match method {
            "initialize" => {
                let version = params["protocolVersion"]
                    .as_str()
                    .filter(|v| matches!(*v, "2025-06-18" | "2025-03-26" | "2024-11-05"))
                    .unwrap_or("2025-06-18");
                let mut hello = json!({"protocolVersion":version,"serverInfo":{"name":self.name,"version":"1.0.0"},"capabilities":{"tools":{"listChanged":true}}});
                if let Some(s) = &self.instructions {
                    hello["instructions"] = s.clone().into();
                }
                Ok(hello)
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(
                json!({"tools": self.tools.read().unwrap().values().map(AsRef::as_ref).collect::<Vec<_>>()}),
            ),
            "tools/call" => self.call(call_key, params).await,
            _ => Err(rpc_error(-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        })
    }

    async fn call(&self, key: CallKey, params: &Value) -> std::result::Result<Value, Value> {
        let name = params["name"].as_str().unwrap_or_default();
        let tool = self
            .tools
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| rpc_error(-32602, format!("unknown tool: {name}")))?;
        let signal = self.shutdown.child_token();
        match self.calls.lock().unwrap().entry(key.clone()) {
            Entry::Occupied(_) => return Err(rpc_error(-32600, "request ID already in use")),
            Entry::Vacant(e) => e.insert(signal.clone()),
        };
        let _call = Call { state: self, key };
        let args = params["arguments"].as_object().cloned().unwrap_or_default();
        let (mut tx, rx) = oneshot::channel();
        // Keep polling the handler briefly after a disconnect so it can observe cancellation
        // and clean up. A handler ignoring its signal cannot keep a detached task alive forever.
        tokio::spawn(async move {
            let mut work = (tool.handler)(args, signal.clone());
            tokio::select! {
                result = &mut work => { let _ = tx.send(result); }
                _ = tx.closed() => {
                    signal.cancel();
                    let _ = tokio::time::timeout(Duration::from_secs(1), work).await;
                }
            }
        });
        let result = rx
            .await
            .unwrap_or_else(|_| Err("host tool handler stopped unexpectedly".into()))
            .unwrap_or_else(|s| HostToolResult {
                is_error: true,
                ..s.into()
            });
        Ok(json!(result))
    }
}
