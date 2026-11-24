//! Public configuration snapshots, persisted keyboard rules and multi-client updates.
use super::{Catalog, Workbench, http::ApiError, mcp::collaboration_tools::environment};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, convert::Infallible};
mod keys;
type Reply = std::result::Result<Json<Value>, ApiError>;
fn bad(s: &str) -> Error {
    Error::Refused(s.into())
}
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Config {
    values: BTreeMap<String, Value>,
    bindings: Vec<keys::Rule>,
    pub revision: u64,
}
const SHARED: &[(&str, &str)] = &[
    (
        "enableProviderUpdateChecks",
        "enable_provider_update_checks",
    ),
    (
        "newWorktreesStartFromOrigin",
        "new_worktrees_start_from_origin",
    ),
    ("defaultThreadEnvMode", "default_thread_env_mode"),
    ("promptSuggestions", "prompt_suggestions"),
    ("backgroundActivity", "background_activity"),
    ("sourceControlWritingStyle", "source_control_writing_style"),
];
fn settings(db: &Catalog) -> Value {
    let mut v = json!({"favorites":null,"providerModelPreferences":null,"responseStreamingMode":"paragraph","environmentIcon":null});
    let shared = environment::prefs(db);
    for (k, old) in SHARED {
        v[k] = shared[old].clone()
    }
    for (k, val) in &db.server.values {
        v[k] = val.clone()
    }
    v
}
fn list_of_strings(v: &Value) -> bool {
    v.as_array().is_some_and(|a| {
        a.len() <= 4096
            && a.iter()
                .all(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 512))
    })
}
fn valid(k: &str, v: &Value) -> bool {
    match k {
        "responseStreamingMode" => matches!(v.as_str(), Some("turn" | "paragraph")),
        "environmentIcon" => {
            v.is_null()
                || matches!(
                    v.as_str(),
                    Some(
                        "server"
                            | "cloud"
                            | "linux"
                            | "desktop"
                            | "laptop"
                            | "mac-mini"
                            | "mac-studio"
                    )
                )
        }
        "favorites" => {
            v.is_null()
                || v.as_array().is_some_and(|a| {
                    a.len() <= 4096
                        && a.iter().all(|v| {
                            ["provider", "model"].iter().all(|k| {
                                v[k].as_str()
                                    .is_some_and(|s| !s.trim().is_empty() && s.len() <= 512)
                            })
                        })
                })
        }
        "providerModelPreferences" => {
            v.is_null()
                || v.as_object().is_some_and(|m| {
                    m.len() <= 256
                        && m.iter().all(|(id, p)| {
                            !id.trim().is_empty()
                                && id.len() <= 256
                                && p.as_object().is_some_and(|o| {
                                    o.iter().all(|(k, v)| {
                                        matches!(k.as_str(), "hiddenModels" | "modelOrder")
                                            && list_of_strings(v)
                                    })
                                })
                        })
                })
        }
        _ => false,
    }
}
fn apply(db: &mut Catalog, patch: Value) -> Result<Value> {
    let p = patch
        .as_object()
        .ok_or_else(|| bad("Expected a settings patch object"))?;
    let mut shared = serde_json::Map::new();
    for (k, v) in p {
        if let Some((_, old)) = SHARED.iter().find(|(name, _)| name == k) {
            shared.insert(old.to_string(), v.clone());
        } else if valid(k, v) {
            db.server.values.insert(k.clone(), v.clone());
        } else {
            return Err(bad("Unknown or invalid server setting"));
        }
    }
    environment::apply_prefs(db, &Value::Object(shared))?;
    Ok(settings(db))
}
fn snapshot(app: &Workbench) -> Result<Value> {
    let env = environment::descriptor(app)?;
    let db = app.0.catalog.lock().unwrap();
    let providers = crate::runtimes()
        .into_iter()
        .map(|r| json!({"instanceId":r.id,"driver":r.id,"name":r.brand().name}))
        .collect::<Vec<_>>();
    let root = app.0.roots.first().map(|r| r.path.clone());
    Ok(
        json!({"revision":db.server.revision,"environment":env,"auth":{"sessionCookieName":"kar_session","sessionMethods":["browser-session-cookie","bearer-access-token","dpop-access-token"]},"cwd":root,"newProjectsRoot":root,"providers":providers,"keybindings":keys::resolved(&db.server.bindings)?,"issues":[],"settings":settings(&db),"serverTime":jiff::Timestamp::now().to_string()}),
    )
}
pub(super) fn next_revision(current: &Catalog, next: &mut Catalog) {
    if current.server != next.server || environment::prefs(current) != environment::prefs(next) {
        next.server.revision = current.server.revision.saturating_add(1);
    }
}
async fn read(State(app): State<Workbench>) -> Reply {
    Ok(Json(snapshot(&app)?))
}
async fn get_settings(State(app): State<Workbench>) -> Json<Value> {
    Json(settings(&app.0.catalog.lock().unwrap()))
}
async fn save(State(app): State<Workbench>, Json(value): Json<Value>) -> Reply {
    Ok(Json(app.update(|db| apply(db, value))?))
}
async fn events(
    State(app): State<Workbench>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let rx = app.0.config_changes.subscribe();
    let stream =
        futures_util::stream::unfold((app, rx, None), |(app, mut rx, mut last)| async move {
            loop {
                let revision = *rx.borrow_and_update();
                if last != Some(revision) {
                    let v = snapshot(&app).ok()?;
                    last = v["revision"].as_u64();
                    let e = Event::default()
                        .event("config")
                        .id(last?.to_string())
                        .json_data(v)
                        .ok()?;
                    return Some((Ok(e), (app, rx, last)));
                }
                rx.changed().await.ok()?;
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/server/config", get(read))
        .route("/server/config/events", get(events))
        .route("/server/settings", get(get_settings).patch(save))
        .route(
            "/server/keybindings",
            axum::routing::put(keys::upsert).delete(keys::remove),
        )
}
