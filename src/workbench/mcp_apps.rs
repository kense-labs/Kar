//! MCP applications use only the MCP server and native thread that produced their original tool item.
mod native;
pub(in crate::workbench) mod record;
use super::{Workbench, http::ApiError};
use crate::{Error, Protocol, Result, Session, chat::Item, observe::Tool};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::PathBuf, sync::Arc};
use uuid::Uuid;
type Key = (Uuid, Uuid, Uuid, String);
type Reply = std::result::Result<Json<Value>, ApiError>;
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct App {
    server: String,
    tool: String,
    resource_uri: String,
    native: String,
    provider: String,
}
struct Selected {
    key: Key,
    cache: PathBuf,
    native: String,
    tool: Tool,
}
fn bad(msg: &str) -> Error {
    Error::Refused(msg.into())
}
async fn select(w: &Workbench, key: Key) -> Result<Selected> {
    let w = w.clone();
    tokio::task::spawn_blocking(move || select_item(&w, key))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}
fn select_item(w: &Workbench, key: Key) -> Result<Selected> {
    let (p, chat, turn, ref item) = key;
    w.binding(p, chat)?;
    let history = w.0.chats.history(chat)?;
    let row = history
        .turns
        .iter()
        .find(|t| t.id == turn)
        .ok_or_else(|| bad("no active turn with this item"))?;
    let mut tools = row.items.iter().filter_map(|i| {
        if !i.source.path.is_empty() {
            return None;
        }
        if let Item::Tool { id, activity } = &i.item
            && id == item
            && activity.finished
        {
            Some((i.source.session_id.as_ref()?, activity))
        } else {
            None
        }
    });
    let Some((source, tool)) = tools.next() else {
        return Err(bad("item is not a completed native application tool"));
    };
    if tools.next().is_some() {
        return Err(bad("ambiguous native tool occurrence"));
    }
    let cache =
        w.0.chats
            .directory()
            .join("workbench/mcp-apps")
            .join(chat.to_string())
            .join(record::hash(&json!([turn, item, source])));
    Ok(Selected {
        key,
        cache,
        native: source.clone(),
        tool: tool.clone(),
    })
}
impl Selected {
    fn session(&self, w: &Workbench) -> Result<Arc<Session>> {
        w.active_control(self.key.0, self.key.1)?
            .filter(|s| s.id() == self.native)
            .ok_or_else(|| {
                Error::Closed("the MCP app's original provider session has stopped".into())
            })
    }
    fn cached(&self) -> Result<Option<App>> {
        record::read(&self.cache.join("reference.json"))
    }
    async fn open(&self, s: &Session) -> Result<App> {
        let (server,tool,uri,provider)=match s.protocol(){
            Protocol::Codex=>{
                if self.tool.native_type.as_deref()!=Some("mcpToolCall"){return Err(bad("item has no native Codex MCP identity"));}
                (self.tool.server.clone().ok_or_else(||bad("missing native MCP server"))?,self.tool.tool_name.clone().ok_or_else(||bad("missing native tool"))?,
                 self.tool.mcp_app_uri.clone().ok_or_else(||bad("item carries no Codex MCP app reference"))?,"codex")
            }
            Protocol::Claude=>{
                if !s.claude_mcp_apps.load(std::sync::atomic::Ordering::Acquire){return Err(Error::Unsupported("Claude did not advertise the native MCP app metadata, resource and result-isolation capabilities".into()));}
                let name=self.tool.name.as_deref().filter(|n|n.starts_with("mcp__")).ok_or_else(||bad("item is not a Claude MCP tool"))?;
                let servers=native::claude_servers(s).await?;let(server,tool)=native::capability(&servers,name)?;
                (server.into(),tool["name"].as_str().ok_or_else(||bad("missing MCP tool name"))?.into(),native::uri(tool).ok_or_else(||bad("native tool declares no app resource"))?.into(),"claude")
            }
            _=>return Err(Error::Unsupported("MCP applications require a provider which can call tools and read resources on the original session".into()))
        };
        let app = App {
            server,
            tool,
            resource_uri: uri,
            native: self.native.clone(),
            provider: provider.into(),
        };
        if let Some(prior) = self.cached()? {
            if prior != app {
                return Err(bad("native MCP application binding changed"));
            }
            return Ok(app);
        }
        // Only native, declared MCP app documents establish the application. Tool output is untrusted.
        let resource = native::resource(s, &app.server, &app.resource_uri).await?;
        let content = resource["contents"]
            .as_array()
            .and_then(|v| {
                v.iter().find(|item| {
                    item["uri"] == app.resource_uri
                        && item["mimeType"].as_str().is_some_and(|m| {
                            m.split_whitespace()
                                .collect::<String>()
                                .eq_ignore_ascii_case("text/html;profile=mcp-app")
                        })
                })
            })
            .ok_or_else(|| bad("MCP app resource lacks its declared HTML media type"))?;
        document(content)?;
        record::capture(&self.cache.join("reference.json"), &app)?;
        if self.cached()?.as_ref() != Some(&app) {
            return Err(bad("conflicting native MCP application identity"));
        }
        Ok(app)
    }
}
fn document(v: &Value) -> Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    if v["text"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.len() <= 5 * 1024 * 1024)
    {
        return Ok(());
    }
    if let Some(blob) = v["blob"].as_str().filter(|s| s.len() <= 7 * 1024 * 1024)
        && let Ok(bytes) = STANDARD.decode(blob)
        && !bytes.is_empty()
        && bytes.len() <= 5 * 1024 * 1024
        && std::str::from_utf8(&bytes).is_ok()
    {
        return Ok(());
    }
    Err(bad("invalid or oversized MCP app document"))
}
async fn open(State(w): State<Workbench>, Path(key): Path<Key>) -> Reply {
    let selected = select(&w, key).await?;
    let s = selected.session(&w)?;
    let app = selected.open(&s).await?;
    Ok(Json(json!({"app":app,"stream":s.stream_id()})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, tag = "action", rename_all = "snake_case")]
enum Request {
    ToolInfo {
        name: String,
        stream: Uuid,
    },
    CallTool {
        name: String,
        arguments: serde_json::Map<String, Value>,
        command_id: Uuid,
        stream: Uuid,
    },
    ReadResource {
        uri: String,
        stream: Uuid,
    },
}
async fn request(
    State(w): State<Workbench>,
    Path(key): Path<Key>,
    Json(req): Json<Request>,
) -> Reply {
    let selected = select(&w, key).await?;
    let s = selected.session(&w)?;
    let stream = match &req {
        Request::ToolInfo { stream, .. }
        | Request::CallTool { stream, .. }
        | Request::ReadResource { stream, .. } => *stream,
    };
    if stream != s.stream_id() {
        return Err(bad("MCP app controller has expired").into());
    }
    let app = selected.open(&s).await?;
    let result = match req {
        Request::ToolInfo { name, .. } => {
            let t = native::tool(&s, &app.server, &name, false).await?;
            let mut info = json!({"callable":t.as_ref().is_some_and(native::visible),"readOnly":t.as_ref().and_then(|v|v.pointer("/annotations/readOnlyHint")).is_some_and(|v|v==true)});
            if let Some(t) = t {
                if let Some(title) = t["title"]
                    .as_str()
                    .or_else(|| t.pointer("/annotations/title").and_then(Value::as_str))
                {
                    info["title"] = title.into();
                }
                info["tool"] = t;
            }
            info
        }
        Request::ReadResource { uri, .. } => native::resource(&s, &app.server, &uri).await?,
        Request::CallTool {
            name,
            arguments,
            command_id,
            ..
        } => {
            let t = native::tool(&s, &app.server, &name, true).await?;
            if !t.as_ref().is_some_and(native::visible) {
                return Err(bad("MCP tool is not callable by an application").into());
            }
            let path = selected.cache.join("calls").join(command_id.to_string());
            let signature = record::hash(&json!([app.server, name, arguments]));
            if let Some(prior) = record::reserve(&path, &signature)? {
                prior
            } else {
                let value = native::call(&s, &app.server, &name, Value::Object(arguments)).await;
                record::finish(&path, &signature, value)?
            }
        }
    };
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    conversation: Uuid,
    #[serde(default)]
    content: Vec<Value>,
    structured_content: Option<Value>,
}
fn text(input: Context) -> Result<String> {
    let mut blocks = vec![];
    for b in input.content {
        if b["type"] != "text" {
            return Err(bad("MCP model context only accepts text blocks"));
        }
        blocks.push(
            b["text"]
                .as_str()
                .ok_or_else(|| bad("invalid MCP text block"))?
                .to_owned(),
        );
    }
    if let Some(data) = input.structured_content {
        blocks.push(data.to_string());
    }
    let text = blocks.join("\n");
    if text.len() > 16 * 1024 {
        return Err(bad("MCP model context exceeds 16 KiB"));
    }
    Ok(text)
}
async fn context(
    State(w): State<Workbench>,
    Path(key): Path<Key>,
    Json(input): Json<Context>,
) -> Reply {
    let selected = select(&w, key).await?;
    let target = input.conversation;
    let text = text(input)?;
    let (p, source, turn, ref id) = selected.key;
    w.binding(p, target)?;
    if w.archived(target) {
        return Err(bad("chat is archived").into());
    }
    let mut cursor = target;
    let mut visited = HashSet::new();
    while cursor != source {
        if !visited.insert(cursor) {
            return Err(bad("cyclic chat lineage").into());
        }
        cursor =
            w.0.chats
                .ancestry_info(cursor)?
                .forked_from
                .ok_or_else(|| bad("MCP app context belongs to its chat or a fork descendant"))?
                .chat;
    }
    let app = match selected.cached()? {
        Some(a) => a,
        None => {
            let s = selected.session(&w)?;
            selected.open(&s).await?
        }
    };
    w.0.chats
        .set_app_context(target, source, turn, id, &app.server, &app.tool, text)?;
    Ok(Json(json!({"updated":true})))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/turns/{turn}/mcp-apps/{item}/open",
            post(open),
        )
        .route(
            "/projects/{project}/chats/{chat}/turns/{turn}/mcp-apps/{item}/requests",
            post(request),
        )
        .route(
            "/projects/{project}/chats/{chat}/turns/{turn}/mcp-apps/{item}/context",
            post(context),
        )
}
