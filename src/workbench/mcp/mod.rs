//! Workbench tools use a caller-scoped transport independent of the runtime's stdio MCP server.
mod forge_tools;
mod issues;
pub(super) use issues::{Policy, permission_routes};
mod registry;
mod toolkit;
mod workflow;
pub(in crate::workbench) mod worktree;
use super::{Workbench, access};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::post,
};
use registry::{Grant, Identity};
pub(super) use registry::{Lease, Registry};
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc};
use uuid::Uuid;

const SESSION: &str = "mcp-session-id";
const VERSION: &str = "mcp-protocol-version";
const STATEFUL: &str = "2025-06-18";
const STATELESS: &str = "2026-07-28";

#[derive(Clone)]
struct Caller {
    identity: Identity,
    grant: Option<Arc<Grant>>,
    plan: bool,
    policy: Option<Policy>,
}
fn hdr<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k)?.to_str().ok()
}
fn error(http: StatusCode, id: &Value, code: i32, message: &str) -> Response {
    (
        http,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
        .into_response()
}
fn invalid(msg: &str) -> Response {
    error(StatusCode::BAD_REQUEST, &Value::Null, -32600, msg)
}
fn gone(id: &Value) -> Response {
    error(
        StatusCode::NOT_FOUND,
        id,
        -32001,
        "Unknown or expired MCP session",
    )
}
fn response(id: &Value, mut result: Value, stateless: bool) -> Response {
    if stateless {
        result["resultType"] = "complete".into();
        result["_meta"] = json!({"io.modelcontextprotocol/serverInfo":server_info()});
    }
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
fn server_info() -> Value {
    json!({"name":"Kar","version":env!("CARGO_PKG_VERSION")})
}
fn capabilities() -> Value {
    json!({"tools":{"listChanged":true}})
}

async fn admit(
    State((app, auth)): State<(Workbench, access::Auth)>,
    mut req: Request,
    next: Next,
) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(grant) = app.0.mcp.resolve(access::bearer(req.headers())) {
        if app.archived(grant.chat) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        req.extensions_mut().insert(grant.clone());
        return access::restrict(req, next, grant.expired()).await;
    }
    access::authorize(State(auth), req, next).await
}
async fn scope(State(app): State<Workbench>, mut req: Request, next: Next) -> Response {
    let grant = req.extensions().get::<Arc<Grant>>().cloned();
    let identity = match (&grant, req.extensions().get::<access::DeviceLease>()) {
        (Some(g), _) => Identity::Provider(g.id),
        (_, Some(d)) => Identity::Device(d.id),
        _ => Identity::Owner,
    };
    let plan = if let Some(raw) = req.headers().get("kar-mode-limit") {
        match raw.to_str().ok().and_then(|s| s.trim().split_once('/')) {
            Some((
                "approval-required" | "auto-accept-edits" | "auto" | "full-access",
                interaction @ ("plan" | "default"),
            )) => interaction == "plan",
            _ => return invalid("Invalid Kar-Mode-Limit"),
        }
    } else {
        false
    };
    let policy = grant.as_ref().and_then(|g| issues::authorized(&app, g));
    req.extensions_mut().insert(Caller {
        policy,
        identity,
        grant,
        plan,
    });
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    res
}
pub(super) fn routes(app: Workbench, auth: access::Auth) -> Router {
    Router::new()
        .route("/mcp", post(rpc).get(events).delete(close))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(app.clone(), scope))
        .layer(middleware::from_fn_with_state((app.clone(), auth), admit))
        .with_state(app)
}
fn session_id(h: &HeaderMap) -> Option<Uuid> {
    hdr(h, SESSION)?.parse().ok()
}
fn accepts(h: &HeaderMap, mime: &str) -> bool {
    hdr(h, "accept").is_some_and(|value| {
        value.split(',').any(|v| {
            v.trim()
                .split(';')
                .next()
                .is_some_and(|v| v == mime || v == "*/*")
        })
    })
}
async fn close(
    State(app): State<Workbench>,
    Extension(caller): Extension<Caller>,
    h: HeaderMap,
) -> Response {
    if hdr(&h, VERSION) == Some(STATELESS) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(id) = session_id(&h) else {
        return invalid("Missing MCP session");
    };
    if app.0.mcp.close(caller.identity, id) {
        StatusCode::OK.into_response()
    } else {
        gone(&Value::Null)
    }
}
async fn events(
    State(app): State<Workbench>,
    Extension(caller): Extension<Caller>,
    h: HeaderMap,
) -> Response {
    if hdr(&h, VERSION) == Some(STATELESS) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if !accepts(&h, "text/event-stream") {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let Some(session) = session_id(&h).and_then(|id| app.0.mcp.session(caller.identity, id)) else {
        return gone(&Value::Null);
    };
    let rx = caller.grant.as_ref().map(|g| g.changes.subscribe());
    let stream = futures_util::stream::unfold((rx, session), |(mut rx, session)| async move {
        let changed = async {
            match &mut rx {
                Some(r) => r.changed().await.ok(),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _=session.closed.cancelled()=>None,
            r=changed=>{r?;
                let e=Event::default().event("message").json_data(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})).ok()?;
                Some((Ok::<_,Infallible>(e),(rx,session)))
            }
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
async fn rpc(
    State(app): State<Workbench>,
    Extension(caller): Extension<Caller>,
    h: HeaderMap,
    Json(msg): Json<Value>,
) -> Response {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return invalid("Missing method");
    };
    let id = &msg["id"];
    if msg["jsonrpc"] != "2.0"
        || msg
            .get("id")
            .is_some_and(|v| !v.is_string() && !v.is_number())
    {
        return invalid("Invalid JSON-RPC request");
    }
    let p = &msg["params"];
    let version = hdr(&h, VERSION);
    if version.is_some_and(|v| !matches!(v, STATEFUL | STATELESS)) {
        return invalid("Unsupported MCP protocol version");
    }
    let stateless = version == Some(STATELESS);
    let mut session = None;
    if stateless {
        if h.contains_key(SESSION) || hdr(&h, "mcp-method") != Some(method) {
            return invalid("MCP method or session mismatch");
        }
        if method == "tools/call" && hdr(&h, "mcp-name") != p["name"].as_str() {
            return invalid("MCP tool name mismatch");
        }
        if method.starts_with("notifications/") && id.is_null() {
            return StatusCode::ACCEPTED.into_response();
        }
        let meta = &p["_meta"];
        if meta["io.modelcontextprotocol/protocolVersion"] != STATELESS
            || !meta["io.modelcontextprotocol/clientCapabilities"].is_object()
        {
            return invalid("Missing stateless protocol metadata");
        }
    } else if method == "initialize" {
        if id.is_null()
            || h.contains_key(SESSION)
            || p["protocolVersion"].as_str().is_none()
            || !p["capabilities"].is_object()
        {
            return invalid("Invalid initialize request");
        }
        let Some(sid) = app.0.mcp.open(caller.identity) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let mut result = response(
            id,
            json!({"protocolVersion":STATEFUL,"serverInfo":server_info(),"capabilities":capabilities()}),
            false,
        );
        result.headers_mut().insert(
            SESSION,
            sid.to_string().parse().expect("UUID is a header value"),
        );
        return result;
    } else {
        let Some(sid) = session_id(&h) else {
            return invalid("Missing MCP session");
        };
        let Some(live) = app.0.mcp.session(caller.identity, sid) else {
            return gone(id);
        };
        session = Some(live);
    }
    if id.is_null() {
        return if method.starts_with("notifications/") {
            StatusCode::ACCEPTED.into_response()
        } else {
            invalid("Missing request id")
        };
    }
    let deferred = hdr(&h, "kar-tool-exposure") == Some("deferred") && caller.grant.is_some();
    let result = match method {
        "ping" => json!({}),
        "server/discover" if stateless => {
            json!({"supportedVersions":[STATEFUL,STATELESS],"capabilities":capabilities()})
        }
        "tools/list" if p.get("cursor").is_none_or(Value::is_null) => {
            json!({"tools":toolkit::list(&caller,deferred)})
        }
        "tools/list" => return error(StatusCode::OK, id, -32602, "Unknown tool list cursor"),
        "tools/call" => {
            let Some(name) = p["name"].as_str() else {
                return error(StatusCode::OK, id, -32602, "Missing tool name");
            };
            if !toolkit::enabled(name, &caller, deferred) {
                return error(StatusCode::OK, id, -32602, "Unknown tool in this scope");
            }
            let args = match p.get("arguments") {
                None => json!({}),
                Some(a) if a.is_object() => a.clone(),
                _ => {
                    return error(
                        StatusCode::OK,
                        id,
                        -32602,
                        "Tool arguments must be an object",
                    );
                }
            };
            let call = toolkit::call(app, caller, name, args);
            let result = if let Some(s) = session {
                tokio::select! {v=call=>v,_=s.closed.cancelled()=>return gone(id)}
            } else {
                call.await
            };
            match result {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value})
                }
                Err(e) => json!({"isError":true,"content":[{"type":"text","text":e.to_string()}]}),
            }
        }
        _ => return error(StatusCode::OK, id, -32601, "Method not found"),
    };
    response(id, result, stateless)
}
