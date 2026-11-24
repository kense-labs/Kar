//! User-operated diagnostics and MCP OAuth on an already attached provider controller.
mod oauth;
use super::{Workbench, access::DeviceLease, http::ApiError, mcp_apps::record};
use crate::{Error, Protocol, Result, Session};
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
pub(super) use oauth::Flows;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
type Reply = std::result::Result<Json<Value>, ApiError>;
type Chat = (Uuid, Uuid);
fn bad(reason: &str) -> Error {
    Error::Refused(reason.into())
}
fn active(w: &Workbench, key: Chat, stream: Uuid) -> Result<Arc<Session>> {
    w.active_control(key.0, key.1)?
        .filter(|s| s.stream_id() == stream)
        .ok_or_else(|| Error::Closed("the selected provider controller is no longer active".into()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Feedback {
    stream: Uuid,
    command_id: Uuid,
    reason: Option<String>,
}
async fn feedback(
    State(w): State<Workbench>,
    Path(key): Path<Chat>,
    Json(input): Json<Feedback>,
) -> Reply {
    let s = active(&w, key, input.stream)?;
    if s.protocol() != Protocol::Codex {
        return Err(Error::Unsupported(
            "this provider has no native diagnostic feedback adapter".into(),
        )
        .into());
    }
    let reason = input
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut params = json!({"classification":"bug","includeLogs":true,"threadId":s.id()});
    if let Some(reason) = reason {
        params["reason"] = reason.into();
    }
    let path =
        w.0.chats
            .directory()
            .join("workbench/provider-feedback")
            .join(key.1.to_string())
            .join(input.command_id.to_string());
    let signature = record::hash(&params);
    if let Some(receipt) = record::reserve(&path, &signature)? {
        return Ok(Json(receipt));
    }
    let result = s.rpc("feedback/upload", params).await.and_then(|r| {
        r["threadId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|id| json!({"feedback_id":id}))
            .ok_or_else(|| Error::Protocol("Codex omitted its native feedback receipt".into()))
    });
    Ok(Json(record::finish(&path, &signature, result)?))
}
async fn capabilities(State(w): State<Workbench>, Path(key): Path<Chat>) -> Reply {
    let s = w
        .active_control(key.0, key.1)?
        .ok_or_else(|| Error::Closed("no active provider controller".into()))?;
    Ok(Json(
        json!({"stream":s.stream_id(),"native_session":s.id(),"mcp_sign_in":matches!(s.protocol(),Protocol::Claude|Protocol::Codex),"upload_diagnostic_feedback":s.protocol()==Protocol::Codex}),
    ))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/provider-controls",
            get(capabilities),
        )
        .route(
            "/projects/{project}/chats/{chat}/provider-feedback",
            post(feedback),
        )
        .route(
            "/projects/{project}/chats/{chat}/mcp-sign-in",
            post(oauth::start),
        )
        .route(
            "/projects/{project}/chats/{chat}/mcp-sign-in/{flow}",
            post(oauth::complete).delete(oauth::cancel),
        )
}
