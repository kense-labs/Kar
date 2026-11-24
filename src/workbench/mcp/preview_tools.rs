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
    pub device_hosts: BTreeSet<String>,
}
#[derive(Copy, Clone)]
enum Access {
    Read,
    Control,
    DeviceRead,
    DeviceControl,
}
fn allowed(c: &Caller, a: Access) -> bool {
    let Some(p) = c.policy.as_ref() else {
        return false;
    };
    c.grant.as_ref().is_some_and(|g| g.active())
        && match a {
            Access::DeviceRead | Access::DeviceControl => !p.preview_tools.device_hosts.is_empty(),
            _ => !p.preview_tools.tabs.is_empty() || !p.preview_tools.terminals.is_empty(),
        }
        && (matches!(a, Access::Read | Access::DeviceRead) || (p.executes() && !c.plan))
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let mut out = Vec::new();
    if allowed(c, Access::Read) {
        out.push(Tool{name:"kar_preview_list",description:"List this chat's shared preview tabs. Use the returned tab_id for browser operations.",properties:json!({"cursor":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":50}}),required:&[]});
    }
    if allowed(c, Access::Control) {
        out.push(Tool{name:"kar_preview_close",description:"Close a preview tab shared with this chat and release its browser and tunnel.",properties:json!({"tab_id":{"type":"string","format":"uuid"}}),required:&["tab_id"]});
    }
    let device = json!({"type":"string","minLength":1});
    let props = json!({"device_id":device,"host_id":device});
    if allowed(c, Access::DeviceRead) {
        out.push(Tool{name:"kar_device_list",description:"List devices, host readiness and this chat's open device views on the granted hosts.",properties:json!({"host_id":device}),required:&[]});
        out.push(Tool{name:"kar_device_screenshot",description:"Capture the real PNG from a device open in this chat. Defaults to the latest open device.",properties:props.clone(),required:&[]});
    }
    if allowed(c, Access::DeviceControl) {
        out.push(Tool{name:"kar_device_open",description:"Open a device in this chat, boot if needed, and return its bound Kar device-agent command.",properties:json!({"device_id":device,"host_id":device,"platform":{"enum":["ios","android"]}}),required:&[]});
        out.push(Tool{name:"kar_device_close",description:"Close this chat's device view. Optionally shut down the device when no other chat uses it.",properties:json!({"device_id":device,"host_id":device,"shutdown":{"type":"boolean"}}),required:&[]});
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
#[derive(Clone)]
pub(in crate::workbench) struct DevicePermission(Caller);
impl DevicePermission {
    pub(in crate::workbench) fn verify(
        &self,
        app: &Workbench,
        write: bool,
    ) -> Result<(Uuid, Uuid)> {
        let g = check(
            app,
            &self.0,
            if write {
                Access::DeviceControl
            } else {
                Access::DeviceRead
            },
        )?;
        Ok((g.project, g.chat))
    }
    pub(in crate::workbench) fn hosts(&self) -> &BTreeSet<String> {
        &self
            .0
            .policy
            .as_ref()
            .expect("authorized policy")
            .preview_tools
            .device_hosts
    }
    pub(in crate::workbench) fn same_lease(&self, other: &Self) -> bool {
        self.0
            .grant
            .as_ref()
            .zip(other.0.grant.as_ref())
            .is_some_and(|(a, b)| a.id == b.id)
            && self.0.policy == other.0.policy
            && self.0.plan == other.0.plan
    }
}
pub(super) async fn call(app: &Workbench, c: &Caller, name: &str, input: Value) -> Result<Value> {
    if name.starts_with("kar_device_") {
        return super::super::devices::model::call(app, DevicePermission(c.clone()), name, input)
            .await;
    }
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

pub(super) fn result(name: &str, value: Value) -> Value {
    if matches!(
        name,
        "kar_device_screenshot" | "kar_preview_snapshot" | "kar_html_preview"
    ) && let Some(image) = value["screenshot"]["data"].as_str()
        && value["screenshot"]["mimeType"] == "image/png"
    {
        let mut metadata = value.clone();
        metadata["screenshot"]
            .as_object_mut()
            .unwrap()
            .remove("data");
        return json!({"structuredContent":value,"content":[{"type":"text","text":metadata.to_string()},{"type":"image","mimeType":"image/png","data":image}]});
    }
    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value})
}
