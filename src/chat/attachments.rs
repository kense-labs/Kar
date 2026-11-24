//! Immutable chat-bound attachments. The inbox and the journal store references, never image bytes.
use super::ChatStore;
use crate::{Error, InputImage, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    File,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub id: Uuid,
    pub name: String,
    pub kind: AttachmentKind,
    pub mime: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Serialize, Deserialize)]
struct Saved {
    chat: Uuid,
    attachment: Attachment,
}
impl Attachment {
    pub(crate) fn validate(name: &str, mime: &str, kind: AttachmentKind, bytes: u64) -> Result<()> {
        if name.trim() != name
            || name.is_empty()
            || name.encode_utf16().count() > 255
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\'))
            || !matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            )
            || mime.is_empty()
            || mime.len() > 100
            || !mime.is_ascii()
            || mime.chars().any(char::is_control)
        {
            return Err(Error::Refused(
                "invalid attachment name or media type".into(),
            ));
        }
        if bytes
            > match kind {
                AttachmentKind::Image => 10 * 1024 * 1024,
                AttachmentKind::File => 50 * 1024 * 1024,
            }
        {
            return Err(Error::Refused("attachment is too large".into()));
        }
        if kind == AttachmentKind::Image
            && !matches!(
                mime,
                "image/gif" | "image/webp" | "image/png" | "image/jpeg"
            )
        {
            return Err(Error::Unsupported("unsupported image type".into()));
        }
        Ok(())
    }
}
impl ChatStore {
    fn attachment_dir(&self, id: Uuid) -> PathBuf {
        self.dir.join("attachments").join(id.to_string())
    }
    fn saved_attachment(&self, id: Uuid) -> Result<Saved> {
        let saved: Saved =
            serde_json::from_reader(File::open(self.attachment_dir(id).join("metadata"))?)?;
        if saved.attachment.id != id {
            return Err(Error::Protocol("unexpected attachment ID".into()));
        }
        let a = &saved.attachment;
        Attachment::validate(&a.name, &a.mime, a.kind, a.bytes)?;
        Ok(saved)
    }
    pub(crate) fn read_attachment(&self, chat: Uuid, id: Uuid) -> Result<Attachment> {
        let saved = self.saved_attachment(id)?;
        if saved.chat == chat {
            return Ok(saved.attachment);
        }
        Err(Error::Refused(
            "attachment belongs to a different chat".into(),
        ))
    }
    pub(crate) fn visible_attachment(&self, chat: Uuid, id: Uuid) -> Result<Attachment> {
        let saved = self.saved_attachment(id)?;
        if saved.chat == chat {
            return Ok(saved.attachment);
        }
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let inherited_end = state
            .info
            .forked_from
            .as_ref()
            .and_then(|origin| state.turns.iter().position(|t| t.id == origin.turn));
        if inherited_end.is_some_and(|end| {
            state.turns[..=end].iter().any(|t| {
                t.attachments
                    .iter()
                    .chain(t.steered_attachments.values().flatten())
                    .any(|a| *a == saved.attachment)
            })
        }) {
            return Ok(saved.attachment);
        }
        Err(Error::Refused(
            "attachment is outside this chat's history".into(),
        ))
    }
    pub fn persist_attachment(
        &self,
        chat: Uuid,
        id: Uuid,
        name: &str,
        mime: &str,
        kind: AttachmentKind,
        data: &[u8],
    ) -> Result<Attachment> {
        self.initial_info(chat)?;
        Attachment::validate(name, mime, kind, data.len() as u64)?;
        if kind == AttachmentKind::Image && InputImage::from_bytes(data)?.mime_type() != mime {
            return Err(Error::Refused(
                "image contents do not match declared type".into(),
            ));
        }
        let attachment = Attachment {
            id,
            name: name.into(),
            mime: mime.into(),
            kind,
            bytes: data.len() as u64,
            sha256: format!("{:x}", Sha256::digest(data)),
        };
        let dir = self.attachment_dir(id);
        let parent = dir.parent().unwrap();
        fs::create_dir_all(parent)?;
        let stage = tempfile::tempdir_in(parent)?;
        fs::create_dir(stage.path().join("content"))?;
        let mut file = File::create(stage.path().join("content").join(name))?;
        file.write_all(data)?;
        file.sync_all()?;
        let record = Saved {
            chat,
            attachment: attachment.clone(),
        };
        let mut metadata = File::create(stage.path().join("metadata"))?;
        serde_json::to_writer(&mut metadata, &record)?;
        metadata.sync_all()?;
        #[cfg(unix)]
        {
            File::open(stage.path().join("content"))?.sync_all()?;
            File::open(stage.path())?.sync_all()?;
        }
        match fs::rename(stage.path(), &dir) {
            Ok(()) => {
                #[cfg(unix)]
                File::open(parent)?.sync_all()?;
            }
            Err(_) if dir.is_dir() => {
                if self.read_attachment(chat, id)? != attachment {
                    return Err(Error::Refused(
                        "attachment ID already has different content".into(),
                    ));
                }
            }
            Err(e) => return Err(e.into()),
        }
        Ok(attachment)
    }
    pub(crate) fn attachment_path(&self, a: &Attachment) -> PathBuf {
        self.attachment_dir(a.id).join("content").join(&a.name)
    }
    pub(crate) fn attachments(&self, chat: Uuid, ids: &[Uuid]) -> Result<Vec<Attachment>> {
        if ids.len() > 100 {
            return Err(Error::Refused("at most 100 attachments per message".into()));
        }
        let mut seen = BTreeSet::new();
        let mut image_bytes = 0;
        let mut out = Vec::new();
        for id in ids {
            if !seen.insert(*id) {
                return Err(Error::Refused("duplicate attachment".into()));
            }
            let a = self.read_attachment(chat, *id)?;
            if a.kind == AttachmentKind::Image {
                image_bytes += a.bytes;
            }
            if image_bytes > 80 * 1024 * 1024 {
                return Err(Error::Refused(
                    "total image bytes exceed the message limit".into(),
                ));
            }
            out.push(a);
        }
        Ok(out)
    }
    pub(crate) fn question_attachment_notes(
        &self,
        chat: Uuid,
        questions: std::collections::BTreeMap<String, Vec<Uuid>>,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        if questions.values().map(Vec::len).sum::<usize>() > 100 {
            return Err(Error::Refused("at most 100 attachments per answer".into()));
        }
        let mut notes = std::collections::BTreeMap::new();
        for (question, ids) in questions {
            let mut references = Vec::new();
            for file in self.attachments(chat, &ids)? {
                self.verified_bytes(&file)?;
                references.push(self.file_reference(&file));
            }
            if !references.is_empty() {
                notes.insert(
                    question,
                    format!(
                        "Attached files (JSON):\n{}",
                        serde_json::Value::Array(references)
                    ),
                );
            }
        }
        Ok(notes)
    }
    fn verified_bytes(&self, file: &Attachment) -> Result<Vec<u8>> {
        let bytes = fs::read(self.attachment_path(file))?;
        if bytes.len() as u64 != file.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != file.sha256
        {
            return Err(Error::Refused("attachment content changed".into()));
        }
        Ok(bytes)
    }
    fn file_reference(&self, a: &Attachment) -> serde_json::Value {
        serde_json::json!({"name":a.name,"path":self.attachment_path(a),"mime":a.mime})
    }
    pub(super) fn file_context(&self, attachments: &[Attachment]) -> String {
        let files = attachments
            .iter()
            .filter(|a| a.kind == AttachmentKind::File)
            .map(|a| self.file_reference(a))
            .collect::<Vec<_>>();
        if files.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nAttached files (JSON):\n{}",
                serde_json::Value::Array(files)
            )
        }
    }
    pub(crate) fn attached_input(
        &self,
        chat: Uuid,
        text: String,
        attachments: &[Attachment],
    ) -> Result<crate::input::Input> {
        let mut images = Vec::new();
        for a in attachments {
            let saved = self.read_attachment(chat, a.id)?;
            if &saved != a {
                return Err(Error::Refused("attachment metadata changed".into()));
            }
            let data = self.verified_bytes(a)?;
            if a.kind == AttachmentKind::Image {
                images.push(InputImage::from_bytes(&data)?);
            }
        }
        let text = text + &self.file_context(attachments);
        Ok(crate::input::Input::new(text, images))
    }
}
