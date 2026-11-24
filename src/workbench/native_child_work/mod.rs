//! Antigravity keeps its per-child work outside ACP. Join it only to a recorded native launch.
mod database;
mod detached;
mod profile;
mod transcript;
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{
    Result,
    chat::{ChatTurn, Item, NativeChildOrigin},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_STEP: usize = 1 << 20;
const CUT: &str = "[Earlier output truncated]\n\n";
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Launch {
    #[serde(rename = "Role")]
    role: String,
    #[serde(rename = "Prompt")]
    prompt: String,
}
struct Child {
    id: String,
    uri: Option<String>,
    launch: Launch,
}
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
fn launches(value: &Value) -> Option<Vec<Launch>> {
    if let Some(list) = value.get("Subagents") {
        let list: Vec<Launch> = serde_json::from_value(list.clone()).ok()?;
        return (!list.is_empty()).then_some(list);
    }
    Some(vec![serde_json::from_value(value.clone()).ok()?])
}
#[derive(Debug, PartialEq, Eq)]
struct Description {
    title: String,
    prompt: String,
}
fn bounded(s: &str) -> String {
    let total = s.encode_utf16().count();
    if total <= 8000 {
        return s.into();
    }
    let tail: Vec<_> = s.encode_utf16().skip(total - 8000).collect();
    format!("{CUT}{}", String::from_utf16_lossy(&tail))
}
fn describe(launches: &[Launch]) -> Description {
    let roles: Vec<_> = launches.iter().map(|l| bounded(&l.role)).collect();
    let prompt = match launches {
        [only] => bounded(&only.prompt),
        batch => batch
            .iter()
            .zip(&roles)
            .map(|(l, r)| format!("{r}\n{}", bounded(&l.prompt)))
            .collect::<Vec<_>>()
            .join("\n\n"),
    };
    Description {
        title: roles.join(", "),
        prompt,
    }
}
struct Batch<'a> {
    id: &'a str,
    description: Option<Description>,
}
fn batches<'a>(
    turn: &'a ChatTurn,
    origin: &'a NativeChildOrigin,
) -> impl Iterator<Item = Batch<'a>> {
    turn.items.iter().filter_map(move |item| {
        let Item::Tool { id, activity } = &item.item else {
            return None;
        };
        if !item.source.path.is_empty()
            || item.source.session_id.as_ref() != Some(&origin.root_session)
            || activity.native_type.as_deref() != Some(crate::wire::antigravity_children::LAUNCH)
        {
            return None;
        }
        let description = activity
            .input
            .as_ref()
            .filter(|_| !activity.input_partial && !activity.elided)
            .and_then(launches)
            .map(|l| describe(&l));
        Some(Batch { id, description })
    })
}
impl Workbench {
    pub fn native_child_work(
        &self,
        project: Uuid,
        chat: Uuid,
        turn: Uuid,
        tool: &str,
    ) -> Result<Value> {
        self.binding(project, chat)?;
        let current = self.0.chats.history(chat)?;
        let selected = current
            .turns
            .iter()
            .find(|t| t.id == turn)
            .ok_or_else(super::missing)?;
        let empty = || json!({"children":[]});
        let Some(origin) = selected.native_child_origin.as_ref() else {
            return Ok(empty());
        };
        if !valid_id(&origin.root_session) || !batches(selected, origin).any(|b| b.id == tool) {
            return Ok(empty());
        }
        // An inherited snapshot may cut off siblings later in the original session.
        let original;
        let history = if origin.chat == chat {
            &current
        } else {
            original = self.0.chats.history(origin.chat)?;
            &original
        };
        let mut found = None;
        let mut siblings = vec![];
        for t in history
            .turns
            .iter()
            .filter(|t| t.native_child_origin.as_ref() == Some(origin))
        {
            for batch in batches(t, origin) {
                if batch.id == tool {
                    // A reused native ID cannot prove a single owning batch.
                    if t.id != turn || found.is_some() {
                        return Ok(empty());
                    }
                    found = Some(batch);
                } else {
                    siblings.push(batch);
                }
            }
        }
        let Some(target) = found else {
            return Ok(empty());
        };
        let Ok(profile) = profile::Profile::open(&origin.profile) else {
            return Ok(empty());
        };
        let root = &origin.root_session;
        let children = detached::read(&profile, root, target.id)
            .or_else(|| database::read(&profile, root, &target, &siblings))
            .unwrap_or_default();
        let budget = (512 * 1024 / children.len().max(1)).min(256 * 1024);
        let children=children.into_iter().map(|child|{
            let transcript=transcript::read(&profile,&child,budget).ok();
            let (steps,truncated)=transcript.map_or((None,false),|(s,t)|(Some(s),t));
            json!({"conversation_id":child.id,"role":child.launch.role,"prompt":child.launch.prompt,"steps":steps,"truncated":truncated})
        }).collect::<Vec<_>>();
        Ok(json!({"children":children}))
    }
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new().route(
        "/projects/{p}/chats/{c}/turns/{turn}/tools/{tool}/children",
        get(query),
    )
}
async fn query(
    State(app): State<Workbench>,
    Path((p, c, t, tool)): Path<(Uuid, Uuid, Uuid, String)>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || app.native_child_work(p, c, t, &tool)).await
}
