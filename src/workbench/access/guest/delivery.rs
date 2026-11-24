//! Use the original proposal UUID and the chat's normal durable attachment queue.
use super::*;
use crate::chat::AttachmentKind;

pub(super) fn kind(mime: &str) -> AttachmentKind {
    if mime.starts_with("image/") {
        AttachmentKind::Image
    } else {
        AttachmentKind::File
    }
}

pub(super) fn reconcile(app: &Workbench, id: Uuid, msg: Uuid) -> AccessResult<()> {
    let (project, chat, mut text, files, legacy) = {
        let db = app.0.access.registry.lock().unwrap();
        let s = db.shares.get(&id).ok_or(AccessError::Missing)?;
        let Some(p) = s
            .proposals
            .get(&msg)
            .filter(|p| p.status == Decision::Sending)
        else {
            return Ok(());
        };
        (
            s.project,
            s.chat,
            p.text.clone(),
            p.files.clone(),
            !p.signature.starts_with("2:"),
        )
    };
    let mut ids = Vec::new();
    for file in &files {
        if legacy {
            // Earlier versions used this literal path in the queue and native-side receipt.
            text.push_str(&format!(
                "\n\nAttachment: {}",
                attachment_path(msg, file).display()
            ));
        } else {
            ids.push(file.id);
        }
    }

    let info = app.binding(project, chat)?;
    let store = &app.0.chats;
    let in_queue = store.queued_inputs(chat)?.iter().any(|q| q.id == msg);
    if !in_queue && !store.history(chat)?.turns.iter().any(|t| t.id == msg) {
        if app.archived(chat) {
            return Ok(());
        }
        match crate::input::check_text(&text, crate::input::text_limit(&info.runtime)) {
            Err(Error::InputTooLarge { .. }) => {
                return record_delivery(app, id, msg, InputStatus::TooLarge);
            }
            other => other?,
        }
        let workspace = app.workspace_at(&info.workspace)?;
        for file in files {
            let bytes = STANDARD
                .decode(&file.base64)
                .map_err(|_| Error::Protocol("invalid saved guest attachment".into()))?;
            workspace.publish_attachment(&attachment_path(msg, &file), &bytes)?;
            if !legacy {
                let mime = file.mime.as_deref().unwrap_or("application/octet-stream");
                store.persist_attachment(chat, file.id, &file.name, mime, kind(mime), &bytes)?;
            }
        }
    }
    let receipt = store.enqueue_attached(chat, msg, text, &ids)?;
    if !in_queue {
        app.wake_queue();
    }
    record_delivery(app, id, msg, receipt.status)
}
