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
    pub html: bool,
    pub html_workspace_files: bool,
    pub upload_files: bool,
    pub public_origins: BTreeSet<String>,
}
#[derive(Copy, Clone)]
enum Access {
    Read,
    Control,
    DeviceRead,
    DeviceControl,
    Html,
}
fn allowed(c: &Caller, a: Access) -> bool {
    let Some(p) = c.policy.as_ref() else {
        return false;
    };
    c.grant.as_ref().is_some_and(|g| g.active())
        && match a {
            Access::Html => p.preview_tools.html,
            Access::DeviceRead | Access::DeviceControl => !p.preview_tools.device_hosts.is_empty(),
            _ => {
                !p.preview_tools.tabs.is_empty()
                    || !p.preview_tools.terminals.is_empty()
                    || !p.preview_tools.public_origins.is_empty()
            }
        }
        && (matches!(a, Access::Read | Access::DeviceRead | Access::Html)
            || (p.executes() && !c.plan))
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let mut out = Vec::new();
    if allowed(c, Access::Read) {
        out.push(Tool{name:"kar_preview_list",description:"List this chat's shared preview tabs. Use the returned tab_id for browser operations.",properties:json!({"cursor":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":50}}),required:&[]});
    }
    if allowed(c, Access::Control) {
        out.push(Tool{name:"kar_preview_close",description:"Close a preview tab shared with this chat and release its browser and tunnel.",properties:json!({"tab_id":{"type":"string","format":"uuid"}}),required:&["tab_id"]});
    }
    let tab = json!({"type":"string","format":"uuid"});
    let string = json!({"type":"string"});
    let number = json!({"type":"number"});
    let yes = json!({"type":"boolean"});
    for (name, description, properties, required, a) in [
        (
            "kar_preview_status",
            "Read the live title, address, viewport and dialog of a shared tab.",
            json!({}),
            &[][..],
            Access::Read,
        ),
        (
            "kar_preview_open",
            "Open a real preview of the granted project terminal listener. Supply its terminal_id and port or select an existing tab_id.",
            json!({"terminal_id":tab,"port":{"type":"integer"},"url":string,"path":string,"reuse_existing_tab":yes}),
            &[],
            Access::Control,
        ),
        (
            "kar_preview_navigate",
            "Navigate within the granted application's origin. Absolute URLs must belong to the same preview.",
            json!({"url":string}),
            &["url"][..],
            Access::Control,
        ),
        (
            "kar_preview_resize",
            "Set a fill, freeform or preset viewport. Supply viewport in Kar's workbench format.",
            json!({"viewport":{"type":"object","properties":{"mode":{"enum":["fill","preset","freeform"]},"preset":string,"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1},"device_scale_factor":{"type":"number","minimum":0.25},"mobile":yes}}}),
            &["viewport"],
            Access::Control,
        ),
        (
            "kar_preview_set_appearance",
            "Change the browser's preferred color scheme.",
            json!({"appearance":{"enum":["dark","light","system"]}}),
            &["appearance"],
            Access::Control,
        ),
        (
            "kar_preview_snapshot",
            "Read visible text, element selectors and viewport. Optionally include or save the PNG.",
            json!({"include_image":yes,"save":yes}),
            &[],
            Access::Read,
        ),
        (
            "kar_preview_dialog",
            "Answer the browser's current JavaScript dialog.",
            json!({"accept":yes,"prompt_text":string}),
            &["accept"],
            Access::Control,
        ),
        (
            "kar_preview_evaluate",
            "Evaluate JavaScript in the granted app's isolated page and return its JSON result.",
            json!({"expression":string,"await_promise":yes}),
            &["expression"],
            Access::Control,
        ),
        (
            "kar_preview_wait_for",
            "Wait for text, a CSS locator or a URL fragment in the selected page.",
            json!({"text":string,"selector":string,"locator":string,"url_includes":string,"timeout_ms":{"type":"integer"}}),
            &[],
            Access::Read,
        ),
        (
            "kar_preview_act",
            "Use a browser action on a CSS selector or coordinates in the granted tab.",
            json!({"action":{"enum":["click","type","hover","select","drag","upload","press","scroll"]},"selector":string,"locator":string,"x":number,"y":number,"text":string,"key":string,"source":string,"target":string,"delta_x":number,"delta_y":number,"values":{"type":"array","items":string},"paths":{"type":"array","items":string}}),
            &["action"],
            Access::Control,
        ),
        (
            "kar_preview_recording_start",
            "Start capturing real frames of this tab into a WebM recording.",
            json!({}),
            &[],
            Access::Control,
        ),
        (
            "kar_preview_recording_stop",
            "Stop recording and publish the video as an attachment in this chat.",
            json!({}),
            &[],
            Access::Control,
        ),
    ] {
        if allowed(c, a) {
            let mut properties = properties;
            properties["tab_id"] = tab.clone();
            out.push(Tool {
                name,
                description,
                properties,
                required,
            });
        }
    }
    if allowed(c, Access::Html) {
        let text = json!({"type":"string","minLength":1,"maxLength":512000});
        let path = json!({"type":"string","minLength":1});
        out.push(Tool{name:"kar_html_preview",description:"Preview one inline HTML page or authorized absolute workspace .html path. Returns a Chromium PNG, content height, and console messages.",properties:json!({"html":text,"path":path,"width":{"type":"integer","minimum":240,"maximum":1600},"appearance":{"enum":["dark","light"]}}),required:&[]});
        out.push(Tool{name:"kar_html_render",description:"Publish an HTML attachment in this chat, with a signed sandboxed reader view. Pass exactly one of html or path.",properties:json!({"html":text,"path":path,"height":{"type":"integer","minimum":80,"maximum":2000},"title":{"type":"string","minLength":1,"maxLength":200}}),required:&["title","height"]});
    }
    let device = json!({"type":"string","minLength":1});
    let props = json!({"device_id":device,"host_id":device});
    if allowed(c, Access::DeviceRead) {
        let mut shot_props = props.clone();
        shot_props["save"] = yes.clone();
        shot_props["include_image"] = yes;
        out.push(Tool{name:"kar_device_list",description:"List devices, host readiness and this chat's open device views on the granted hosts.",properties:json!({"host_id":device}),required:&[]});
        out.push(Tool{name:"kar_device_screenshot",description:"Capture the real PNG from a device open in this chat. Defaults to the latest open device.",properties:shot_props,required:&[]});
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
    if name.starts_with("kar_html_") {
        let g = check(app, c, Access::Html)?;
        return super::html::call(
            app,
            g.project,
            g.chat,
            name,
            input,
            &c.policy.as_ref().unwrap().preview_tools.public_origins,
            c.policy
                .as_ref()
                .unwrap()
                .preview_tools
                .html_workspace_files
                && c.files_allowed,
            || check(app, c, Access::Html).map(|_| ()),
        )
        .await;
    }
    if name.starts_with("kar_device_") {
        let mut forward = input;
        let mut save_shot = false;
        let mut include_image: Option<bool> = None;
        if name == "kar_device_screenshot"
            && let Some(obj) = forward.as_object_mut()
        {
            if let Some(v) = obj.remove("save") {
                save_shot = serde_json::from_value(v)?;
            }
            if let Some(v) = obj.remove("include_image") {
                include_image = serde_json::from_value(v)?;
            }
        }
        let mut out =
            super::super::devices::model::call(app, DevicePermission(c.clone()), name, forward)
                .await?;
        if name == "kar_device_screenshot" && save_shot {
            let g = check(app, c, Access::DeviceRead)?;
            if let Some(b64) = out["screenshot"]["data"].as_str() {
                use base64::{Engine as _, engine::general_purpose::STANDARD};
                let bytes = STANDARD
                    .decode(b64)
                    .map_err(|_| Error::Protocol("invalid device PNG base64".into()))?;
                out["attachment"] = json!(app.0.chats.persist_attachment(
                    g.chat,
                    Uuid::new_v4(),
                    "device.png",
                    "image/png",
                    crate::chat::AttachmentKind::Image,
                    &bytes,
                )?);
            }
            if include_image == Some(false) {
                out["_omit_inline_image"] = Value::Bool(true);
            }
        }
        return Ok(out);
    }
    let g = check(
        app,
        c,
        if matches!(
            name,
            "kar_preview_list"
                | "kar_preview_status"
                | "kar_preview_snapshot"
                | "kar_preview_wait_for"
        ) {
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
            let all = state.model_tabs(g.project, g.chat, &resources.tabs, &resources.terminals);
            let next = start.saturating_add(limit);
            let count = all.len();
            let sessions: Vec<_> = all.into_iter().skip(start).take(limit).collect();
            Ok(json!({"sessions":sessions,"next_cursor":(next<count).then_some(next)}))
        }
        "kar_preview_close" => {
            let tab: Tab = serde_json::from_value(input)?;
            state.model_close(
                g.project,
                g.chat,
                &resources.tabs,
                &resources.terminals,
                tab.tab_id,
            )?;
            Ok(json!({"closed":tab.tab_id}))
        }
        _ => {
            let omit_inline = name == "kar_preview_snapshot"
                && input.get("save") == Some(&Value::Bool(true))
                && input.get("include_image") == Some(&Value::Bool(false));
            let weak = std::sync::Arc::downgrade(&app.0);
            let caller = c.clone();
            let access = if matches!(
                name,
                "kar_preview_status" | "kar_preview_snapshot" | "kar_preview_wait_for"
            ) {
                Access::Read
            } else {
                Access::Control
            };
            let mut out = super::super::terminal_preview::automation(
                app,
                g.project,
                g.chat,
                &resources.tabs,
                &resources.terminals,
                &resources.public_origins,
                resources.upload_files && c.files_allowed,
                name,
                input,
                std::sync::Arc::new(move || {
                    let app = Workbench(
                        weak.upgrade()
                            .ok_or_else(|| Error::Closed("workbench stopped".into()))?,
                    );
                    check(&app, &caller, access).map(|_| ())
                }),
            )
            .await?;
            if omit_inline {
                out["_omit_inline_image"] = Value::Bool(true);
            }
            Ok(out)
        }
    }
}

pub(super) const MAX_SNAPSHOT_TEXT_BYTES: usize = 20_000;

fn truncate_str_chars(s: &mut String, max_chars: usize) {
    if let Some((byte_idx, _)) = s.char_indices().nth(max_chars) {
        s.truncate(byte_idx);
    }
}

fn truncate_str_bytes(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

fn shed_console_arrays(obj: &mut serde_json::Map<String, Value>, clear_all: bool) {
    for key in ["consoleMessages", "console", "logs"] {
        if let Some(Value::Array(arr)) = obj.get_mut(key) {
            if clear_all {
                arr.clear();
            } else {
                arr.truncate(8);
                for item in arr.iter_mut() {
                    if let Some(s) = item.as_str().map(str::to_owned) {
                        let mut trimmed = s;
                        truncate_str_chars(&mut trimmed, 120);
                        *item = Value::String(trimmed);
                    } else if let Some(entry_obj) = item.as_object_mut()
                        && let Some(Value::String(text)) = entry_obj.get_mut("text")
                    {
                        truncate_str_chars(text, 120);
                    }
                }
            }
        }
    }
}

/// Progressively shed verbose console logs and long DOM text nodes so serialized
/// `kar_preview_snapshot` metadata stays `<= MAX_SNAPSHOT_TEXT_BYTES` while preserving
/// interactive ARIA element refs/locators (`tag`, `role`, `selector`, `locator`, `ref`).
pub(super) fn bound_snapshot_metadata(meta: &mut Value) {
    if meta.to_string().len() <= MAX_SNAPSHOT_TEXT_BYTES {
        return;
    }
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("truncated".into(), Value::Bool(true));
        shed_console_arrays(obj, false);
        if let Some(page) = obj.get_mut("page").and_then(Value::as_object_mut) {
            shed_console_arrays(page, false);
        }
    }
    if meta.to_string().len() <= MAX_SNAPSHOT_TEXT_BYTES {
        return;
    }

    // Stage 1b: Drop remaining verbose console log arrays completely before touching page content/ARIA refs.
    if let Some(obj) = meta.as_object_mut() {
        shed_console_arrays(obj, true);
        if let Some(page) = obj.get_mut("page").and_then(Value::as_object_mut) {
            shed_console_arrays(page, true);
        }
    }
    if meta.to_string().len() <= MAX_SNAPSHOT_TEXT_BYTES {
        return;
    }

    // Stage 2: Progressively shrink `page.text` (and top-level `text` if present), keeping all interactive `elements` intact.
    for budget_bytes in [8_000usize, 4_000, 1_500, 512, 0] {
        if let Some(obj) = meta.as_object_mut() {
            if let Some(Value::String(t)) = obj.get_mut("text") {
                truncate_str_bytes(t, budget_bytes);
            }
            if let Some(page) = obj.get_mut("page").and_then(Value::as_object_mut)
                && let Some(Value::String(t)) = page.get_mut("text")
            {
                truncate_str_bytes(t, budget_bytes);
            }
        }
        if meta.to_string().len() <= MAX_SNAPSHOT_TEXT_BYTES {
            return;
        }
    }

    // Stage 3: Trim verbose element `text` labels while preserving `tag`, `role`, `selector`, `locator`, `ref`.
    for label_chars in [48usize, 16, 0] {
        if let Some(elements) = meta
            .pointer_mut("/page/elements")
            .and_then(Value::as_array_mut)
        {
            for elem in elements.iter_mut() {
                if let Some(eobj) = elem.as_object_mut()
                    && let Some(Value::String(label)) = eobj.get_mut("text")
                {
                    truncate_str_chars(label, label_chars);
                }
            }
        }
        if meta.to_string().len() <= MAX_SNAPSHOT_TEXT_BYTES {
            return;
        }
    }

    // Stage 4: Hard cap fallback if huge URLs/titles or hundreds of oversized CSS selectors still exceed 20 KB.
    if let Some(page) = meta.get_mut("page").and_then(Value::as_object_mut) {
        for field in ["title", "url"] {
            if let Some(Value::String(s)) = page.get_mut(field) {
                truncate_str_bytes(s, 512);
            }
        }
    }
    while meta.to_string().len() > MAX_SNAPSHOT_TEXT_BYTES {
        let popped = meta
            .pointer_mut("/page/elements")
            .and_then(Value::as_array_mut)
            .and_then(Vec::pop);
        if popped.is_some() {
            if let Some(page) = meta.get_mut("page").and_then(Value::as_object_mut) {
                page.insert("elements_truncated".into(), Value::Bool(true));
            }
        } else {
            break;
        }
    }
}

pub(super) fn result(name: &str, value: Value) -> Value {
    let omit_inline = value.get("_omit_inline_image") == Some(&Value::Bool(true))
        || (value.get("save") == Some(&Value::Bool(true))
            && value.get("include_image") == Some(&Value::Bool(false)))
        || (value.get("attachment").is_some()
            && value.get("include_image") == Some(&Value::Bool(false)));
    let is_screenshot_tool = matches!(
        name,
        "kar_device_screenshot" | "kar_preview_snapshot" | "kar_html_preview"
    );
    let image_data = if is_screenshot_tool && value["screenshot"]["mimeType"] == "image/png" {
        value["screenshot"]["data"].as_str().map(str::to_owned)
    } else {
        None
    };

    let mut metadata = value;
    if let Some(obj) = metadata.as_object_mut() {
        obj.remove("_omit_inline_image");
    }
    if is_screenshot_tool
        && let Some(sc) = metadata
            .get_mut("screenshot")
            .and_then(Value::as_object_mut)
    {
        sc.remove("data");
    }
    if name == "kar_preview_snapshot" {
        bound_snapshot_metadata(&mut metadata);
    }

    let text = metadata.to_string();
    if let Some(image) = image_data.filter(|_| !omit_inline) {
        return json!({
            "structuredContent": metadata,
            "content": [
                {"type": "text", "text": text},
                {"type": "image", "mimeType": "image/png", "data": image}
            ]
        });
    }
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": metadata
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_strips_screenshot_data_from_structured_content_and_respects_omit_inline_image() {
        let raw = json!({
            "tab_id": Uuid::nil(),
            "screenshot": {
                "mimeType": "image/png",
                "width": 800,
                "height": 600,
                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB"
            }
        });
        let out = result("kar_preview_snapshot", raw);
        assert!(out["structuredContent"]["screenshot"].get("data").is_none());
        assert_eq!(out["structuredContent"]["screenshot"]["width"], 800);
        assert_eq!(out["content"].as_array().unwrap().len(), 2);
        assert_eq!(out["content"][1]["type"], "image");
        assert!(
            !out["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("iVBORw0KGgo")
        );

        // When save == true and include_image == Some(false), omit inline base64 image block!
        let saved_no_inline = json!({
            "tab_id": Uuid::nil(),
            "attachment": {"id": Uuid::nil()},
            "_omit_inline_image": true,
            "screenshot": {
                "mimeType": "image/png",
                "width": 800,
                "height": 600,
                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB"
            }
        });
        let omitted = result("kar_preview_snapshot", saved_no_inline);
        assert!(
            omitted["structuredContent"]["screenshot"]
                .get("data")
                .is_none()
        );
        assert!(
            omitted["structuredContent"]
                .get("_omit_inline_image")
                .is_none()
        );
        assert_eq!(omitted["content"].as_array().unwrap().len(), 1);
        assert_eq!(omitted["content"][0]["type"], "text");
    }

    #[test]
    fn snapshot_bounding_sheds_logs_and_body_text_while_preserving_aria_locators() {
        let elements: Vec<Value> = (0..40)
            .map(|i| {
                json!({
                    "tag": "button",
                    "role": "button",
                    "ref": format!("e{i}"),
                    "selector": format!("#action-btn-{i}"),
                    "locator": format!("role=button[name='Action {i}']"),
                    "text": format!("Action button {i} description"),
                })
            })
            .collect();
        let logs: Vec<Value> = (0..80)
            .map(|i| json!({"level": "info", "text": "verbose-log-".repeat(30) + &i.to_string()}))
            .collect();
        let huge = json!({
            "tab_id": Uuid::nil(),
            "page": {
                "url": "http://127.0.0.1:3000/dashboard",
                "title": "Dashboard",
                "text": "Long prose paragraph content. ".repeat(1_500),
                "width": 1280,
                "height": 800,
                "consoleMessages": logs,
                "elements": elements,
            },
            "screenshot": {"width": 1280, "height": 800}
        });

        let out = result("kar_preview_snapshot", huge);
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(
            text.len() <= MAX_SNAPSHOT_TEXT_BYTES,
            "snapshot text was {} bytes (> {})",
            text.len(),
            MAX_SNAPSHOT_TEXT_BYTES
        );
        assert_eq!(out["structuredContent"]["truncated"], true);
        let preserved = out["structuredContent"]["page"]["elements"]
            .as_array()
            .unwrap();
        assert_eq!(preserved.len(), 40);
        assert_eq!(preserved[0]["selector"], "#action-btn-0");
        assert_eq!(preserved[39]["locator"], "role=button[name='Action 39']");
        assert_eq!(preserved[39]["ref"], "e39");
    }
}
