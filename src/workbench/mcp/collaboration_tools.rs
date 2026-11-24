use super::{Caller, Grant, Policy, toolkit::Tool};
use crate::{
    Error, Result,
    workbench::{Workbench, assets},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn denied() -> Error {
    Error::Refused("operation requires an active, authorized model session".into())
}
fn active(c: &Caller) -> Result<&Grant> {
    c.grant.as_deref().filter(|g| g.active()).ok_or_else(denied)
}
fn execute(c: &Caller) -> bool {
    !c.plan && c.policy.as_ref().is_some_and(Policy::executes)
}
fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &'static [&'static str],
) -> Tool {
    Tool {
        name,
        description,
        properties,
        required,
    }
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    if active(c).is_err() {
        return vec![];
    }
    let mut choices = vec![
        tool(
            "kar_send_to_user",
            "Show Markdown verbatim in the current turn.",
            json!({"message":{"type":"string","maxLength":32000}}),
            &["message"],
        ),
        tool(
            "kar_submit_output",
            "Submit JSON matching this turn's requested output schema.",
            json!({"json":{"type":"string","maxLength":200000}}),
            &["json"],
        ),
    ];
    if execute(c) {
        let id = json!({"type":"string","format":"uuid"});
        choices.extend([
            tool("kar_attachment_prepare_upload","Prepare an upload in this chat. PUT the exact bytes to upload_url.",json!({"name":{"type":"string"},"mime":{"type":"string"},"kind":{"enum":["image","file"]},"bytes":{"type":"integer","minimum":1}}),&["name","mime","kind","bytes"]),
            tool("kar_attachment_discard","Discard an unclaimed upload prepared by this chat.",json!({"attachment_id":id}),&["attachment_id"]),
            tool("kar_thread_send_attachments","Queue uploaded or previously sent attachments in this chat. Reuse input_id for retries.",json!({"attachment_ids":{"type":"array","minItems":1,"maxItems":100,"items":id},"text":{"type":"string"},"input_id":id}),&["attachment_ids"]),
        ]);
    }
    choices
}
pub(super) fn handles(name: &str) -> bool {
    matches!(
        name,
        "kar_send_to_user"
            | "kar_submit_output"
            | "kar_attachment_prepare_upload"
            | "kar_attachment_discard"
            | "kar_thread_send_attachments"
    )
}
pub(super) async fn call(app: &Workbench, c: &Caller, name: &str, input: Value) -> Result<Value> {
    let grant = active(c)?;
    app.binding(grant.project, grant.chat)?;
    if app.archived(grant.chat) {
        return Err(denied());
    }
    if matches!(name, "kar_send_to_user" | "kar_submit_output") {
        return output::call(app, grant, name, input);
    }
    let db = app.0.catalog.lock().unwrap();
    if !execute(c) || !Policy::current(&db, grant.project, grant.chat).is_some_and(Policy::executes)
    {
        return Err(denied());
    }
    drop(db);
    let (project, chat) = (grant.project, grant.chat);
    match name {
        "kar_attachment_prepare_upload" => {
            let mut answer =
                assets::prepare(app, project, serde_json::from_value(input)?, Some(chat))?;
            if let (Some(origin), Some(path)) = (app.0.mcp.origin(), answer["upload_path"].as_str())
            {
                answer["upload_url"] = format!("{origin}{path}").into();
            }
            Ok(answer)
        }
        "kar_attachment_discard" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Discard {
                attachment_id: Uuid,
            }
            let arg: Discard = serde_json::from_value(input)?;
            assets::discard_for(app, project, arg.attachment_id, Some(chat))
        }
        "kar_thread_send_attachments" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Send {
                attachment_ids: Vec<Uuid>,
                #[serde(default)]
                text: String,
                input_id: Option<Uuid>,
            }
            let m: Send = serde_json::from_value(input)?;
            if m.attachment_ids.is_empty() || m.attachment_ids.len() > 100 || m.text.len() > 100000
            {
                return Err(Error::Refused("invalid attachment message".into()));
            }
            for id in &m.attachment_ids {
                assets::claim_for(app, project, chat, *id, Some(chat))?;
            }
            let receipt = app.0.chats.enqueue_attached(
                chat,
                m.input_id.unwrap_or_else(Uuid::new_v4),
                m.text,
                &m.attachment_ids,
            )?;
            app.wake_queue();
            Ok(json!(receipt))
        }
        _ => Err(denied()),
    }
}
mod output;
