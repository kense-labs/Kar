use super::*;
use crate::workbench::{Catalog, http::ApiError};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub(in crate::workbench) mod cards;
pub(super) mod links;
pub(in crate::workbench) mod secrets;
mod theme;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(in crate::workbench) struct Environment {
    id: Option<Uuid>,
    preferences: BTreeMap<String, Value>,
    themes: BTreeMap<String, Value>,
    selected_theme: Option<String>,
    appearance: Option<String>,
    links: BTreeMap<Uuid, links::Peer>,
    cards: BTreeMap<Uuid, cards::Card>,
}
fn bad(msg: &str) -> Error {
    Error::Refused(msg.into())
}
type Reply = std::result::Result<Json<Value>, ApiError>;
pub(in crate::workbench) fn id(app: &Workbench) -> Result<Uuid> {
    if let Some(id) = app.0.catalog.lock().unwrap().environment.id {
        return Ok(id);
    }
    app.update(|db| Ok(*db.environment.id.get_or_insert_with(Uuid::new_v4)))
}
fn check(db: &Catalog, c: &Caller, g: &Grant) -> Result<()> {
    if c.plan
        || g.caller_turn().is_none()
        || !g.active()
        || !Policy::current(db, g.project, g.chat).is_some_and(Policy::writes_environment)
    {
        return Err(denied());
    }
    Ok(())
}
pub(in crate::workbench) fn worktrees_from_origin(db: &Catalog) -> bool {
    db.environment
        .preferences
        .get("new_worktrees_start_from_origin")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}
fn prefs(db: &Catalog) -> Value {
    let mut result = json!({"default_thread_env_mode":"local","new_worktrees_start_from_origin":true,"enable_provider_update_checks":true,"background_activity":"balanced",
        "source_control_writing_style":{"mode":"repo_conventions","follow_change_request_templates":true,"custom_instructions":""}});
    for (name, value) in &db.environment.preferences {
        result[name] = value.clone();
    }
    result["prompt_suggestions"] = db.prompt_suggestions.into();
    result
}
fn descriptor(app: &Workbench) -> Result<Value> {
    let id = id(app)?;
    Ok(
        json!({"environment_id":id,"label":std::env::var("KAR_ENVIRONMENT_LABEL").unwrap_or_else(|_|"Kar".into()),"server_version":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS,"preferences":prefs(&app.0.catalog.lock().unwrap())}),
    )
}
fn validate_patch(patch: &Value) -> Result<&serde_json::Map<String, Value>> {
    let object = patch
        .as_object()
        .ok_or_else(|| bad("expected preferences object"))?;
    for (k, v) in object {
        let valid = match k.as_str() {
            "default_thread_env_mode" => matches!(v.as_str(), Some("local" | "worktree")),
            "new_worktrees_start_from_origin"
            | "enable_provider_update_checks"
            | "prompt_suggestions" => v.is_boolean(),
            "background_activity" => matches!(
                v.as_str(),
                Some("balanced" | "performance" | "battery-saver")
            ),
            "source_control_writing_style" => v.as_object().is_some_and(|s| {
                !s.is_empty()
                    && s.iter().all(|(k, v)| match k.as_str() {
                        "mode" => matches!(
                            v.as_str(),
                            Some("repo_conventions" | "conventional_commits" | "custom")
                        ),
                        "follow_change_request_templates" => v.is_boolean(),
                        "custom_instructions" => {
                            v.as_str().is_some_and(|v| v.chars().count() <= 4000)
                        }
                        _ => false,
                    })
            }),
            _ => false,
        };
        if !valid {
            return Err(bad("unknown or invalid Kar environment preference"));
        }
    }
    Ok(object)
}
fn update_prefs(app: &Workbench, c: Option<(&Caller, &Grant)>, patch: Value) -> Result<Value> {
    let changes = validate_patch(&patch)?;
    app.update(|db| {
        if let Some((caller, grant)) = c {
            check(db, caller, grant)?;
        }
        for (key, v) in changes {
            if key == "prompt_suggestions" {
                db.prompt_suggestions = v.as_bool().unwrap();
            } else if key == "source_control_writing_style" {
                let mut style = prefs(db)[key].clone();
                style
                    .as_object_mut()
                    .unwrap()
                    .extend(v.as_object().unwrap().clone());
                db.environment.preferences.insert(key.clone(), style);
            } else {
                db.environment.preferences.insert(key.clone(), v.clone());
            }
        }
        Ok(prefs(db))
    })
}
async fn read(State(app): State<Workbench>) -> Reply {
    Ok(Json(descriptor(&app)?))
}
async fn save(State(app): State<Workbench>, Json(patch): Json<Value>) -> Reply {
    Ok(Json(update_prefs(&app, None, patch)?))
}
#[derive(Default, Deserialize)]
struct ThemeQuery {
    theme_id: Option<String>,
}
async fn read_theme(State(app): State<Workbench>, Query(q): Query<ThemeQuery>) -> Reply {
    Ok(Json(theme::read(&app, q.theme_id.as_deref())?))
}
async fn save_theme(State(app): State<Workbench>, Json(input): Json<Value>) -> Reply {
    Ok(Json(theme::set(&app, None, input)?))
}
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/environment", get(read))
        .route("/environment/theme", get(read_theme))
        .merge(links::session_routes())
}
pub(in crate::workbench) fn owner_routes() -> Router<Workbench> {
    Router::new()
        .route("/environment/preferences", put(save))
        .route("/environment/theme/settings", put(save_theme))
        .merge(cards::routes())
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let id = json!({"type":"string","format":"uuid"});
    let mut tools = vec![
        tool(
            "kar_environment_read",
            "Read this environment or a linked environment's live identity and preferences.",
            json!({"environment_id":id}),
            &[],
        ),
        tool(
            "kar_theme_read",
            "Read Kar theme preferences, published palettes and CSS.",
            json!({"theme_id":{"type":"string"}}),
            &[],
        ),
        tool(
            "kar_environment_links",
            "List linked Kar environments and test each existing connection.",
            json!({}),
            &[],
        ),
    ];
    if !c.plan && c.policy.as_ref().is_some_and(Policy::writes_environment) {
        tools.extend([
            tool("kar_environment_preferences_update","Update the owner's environment preferences. Omitted preferences stay as configured.",json!({"preferences":{"type":"object"}}),&["preferences"]),
            tool("kar_theme_set","Publish or select a Kar environment theme and its appearance.",json!({"theme_id":{"type":"string"},"theme":{"type":"object"},"appearance":{"enum":["light","dark","system"]}}),&[]),
            tool("kar_environment_link","Ask the owner to pair another Kar host. Pairing credentials belong in the private approval card.",json!({"request_id":id,"environment_id":id,"hint":{"type":"string"},"url":{"type":"string"}}),&["request_id"]),
            tool("kar_environment_unlink","Remove a linked host and revoke this host's remote device session.",json!({"environment_id":id}),&["environment_id"]),
        ]);
    }
    tools.extend(cards::tools(c));
    tools
}
pub(super) fn handles(name: &str) -> bool {
    name.starts_with("kar_environment_")
        || matches!(
            name,
            "kar_theme_read" | "kar_theme_set" | "kar_feedback" | "kar_request_secret"
        )
}
pub(super) async fn call(
    app: &Workbench,
    c: &Caller,
    g: &Grant,
    name: &str,
    input: Value,
) -> Result<Value> {
    match name {
        "kar_environment_read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Read {
                environment_id: Option<Uuid>,
            }
            let m: Read = serde_json::from_value(input)?;
            if let Some(peer) = m.environment_id
                && peer != id(app)?
            {
                links::read(app, peer).await
            } else {
                descriptor(app)
            }
        }
        "kar_environment_preferences_update" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Patch {
                preferences: Value,
            }
            update_prefs(
                app,
                Some((c, g)),
                serde_json::from_value::<Patch>(input)?.preferences,
            )
        }
        "kar_theme_read" => theme::read(
            app,
            serde_json::from_value::<ThemeQuery>(input)?
                .theme_id
                .as_deref(),
        ),
        "kar_theme_set" => theme::set(app, Some((c, g)), input),
        "kar_environment_links" => links::list(app).await,
        "kar_environment_link" => links::request_link(app, c, g, input),
        "kar_environment_unlink" => links::unlink(app, c, g, input).await,
        _ => cards::call(app, c, g, name, input).await,
    }
}
