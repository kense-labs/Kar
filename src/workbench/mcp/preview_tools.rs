//! A model lease can see only preview resources shared with its project and chat.
use super::{Caller, Policy, Workbench, toolkit::Tool};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(in crate::workbench) struct Resources {
    pub tabs: BTreeSet<Uuid>,
    pub terminals: BTreeSet<Uuid>,
}
#[derive(Copy, Clone)]
enum Access {
    Read,
    Control,
}
fn allowed(c: &Caller, a: Access) -> bool {
    let Some(p) = c.policy.as_ref() else {
        return false;
    };
    c.grant.as_ref().is_some_and(|g| g.active())
        && (!p.preview_tools.tabs.is_empty() || !p.preview_tools.terminals.is_empty())
        && (matches!(a, Access::Read) || (p.executes() && !c.plan))
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let mut out = Vec::new();
    if allowed(c, Access::Read) {
        out.push(Tool{name:"kar_preview_list",description:"List this chat's shared preview tabs. Use the returned tab_id for browser operations.",properties:json!({"cursor":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":50}}),required:&[]});
    }
    if allowed(c, Access::Control) {
        out.push(Tool{name:"kar_preview_close",description:"Close a preview tab shared with this chat and release its browser and tunnel.",properties:json!({"tab_id":{"type":"string","format":"uuid"}}),required:&["tab_id"]});
    }
    out
}
fn check<'a>(app: &Workbench, c: &'a Caller, a: Access) -> Result<&'a super::Grant> {
    let err = || Error::Refused("preview access was not granted to this chat".into());
    let g = c.grant.as_deref().ok_or_else(err)?;
    if !allowed(c, a)
        || Policy::current(&app.0.catalog.lock().unwrap(), g.project, g.chat) != c.policy.as_ref()
    {
        return Err(err());
    }
    app.binding(g.project, g.chat)?;
    Ok(g)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    cursor: Option<usize>,
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tab {
    tab_id: Uuid,
}
pub(super) async fn call(app: &Workbench, c: &Caller, name: &str, input: Value) -> Result<Value> {
    let g = check(
        app,
        c,
        if name == "kar_preview_list" {
            Access::Read
        } else {
            Access::Control
        },
    )?;
    let resources = &c.policy.as_ref().unwrap().preview_tools;
    let state = &app.0.terminal_preview;
    match name {
        "kar_preview_list" => {
            let page: Page = serde_json::from_value(input)?;
            let start = page.cursor.unwrap_or(0);
            let limit = page.limit.unwrap_or(20).clamp(1, 50);
            let all = state.model_tabs(g.project, g.chat, &resources.tabs);
            let next = start.saturating_add(limit);
            let count = all.len();
            let sessions: Vec<_> = all.into_iter().skip(start).take(limit).collect();
            Ok(json!({"sessions":sessions,"next_cursor":(next<count).then_some(next)}))
        }
        "kar_preview_close" => {
            let tab: Tab = serde_json::from_value(input)?;
            state.model_close(g.project, g.chat, &resources.tabs, tab.tab_id)?;
            Ok(json!({"closed":tab.tab_id}))
        }
        _ => Err(Error::Unsupported("unknown preview operation".into())),
    }
}
