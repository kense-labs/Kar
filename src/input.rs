use crate::{Error, Protocol, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const EMPTY_INPUT: &str = "empty input: give text or images";

/// Pi may resize or re-encode images and append resize hints before echoing and
/// saving the user's message. Bind recovery to the ordered native content.
pub(crate) fn pi_image_content_sha256(content: &Value) -> Option<String> {
    use sha2::{Digest, Sha256};
    let parts = content.as_array()?;
    if !parts.iter().any(|b| b["type"] == "image") {
        return None;
    }
    let mut digest = Sha256::new();
    for part in parts {
        let fields = match part["type"].as_str()? {
            "text" => ["text", "", part["text"].as_str()?],
            "image" => ["image", part["mimeType"].as_str()?, part["data"].as_str()?],
            _ => return None,
        };
        for field in fields {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
    }
    Some(format!("{:x}", digest.finalize()))
}

/// Remove raw image bytes from a Pi user echo after retaining its native identity.
pub(crate) fn pi_user_echo(packet: &mut Value) {
    if matches!(
        packet["type"].as_str(),
        Some("message_start" | "message_end")
    ) && packet["message"]["role"] == "user"
    {
        let digest = pi_image_content_sha256(&packet["message"]["content"]);
        packet
            .as_object_mut()
            .unwrap()
            .remove("karPiImageContentSha256");
        if let Some(digest) = digest {
            packet["karPiImageContentSha256"] = digest.into();
        }
        omit_image_bytes(&mut packet["message"]["content"]);
    }
}

/// Remove image bytes from a public copy of a native prompt, after dispatch.
pub(crate) fn omit_image_bytes(packet: &mut Value) {
    match packet {
        Value::Object(obj) => {
            if matches!(
                obj.get("type").and_then(Value::as_str),
                Some("image" | "base64")
            ) {
                obj.remove("data");
                if let Some(source) = obj.get_mut("source").and_then(Value::as_object_mut) {
                    source.remove("data");
                }
                if obj
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| u.starts_with("data:image/"))
                {
                    obj.remove("url");
                }
            }
            if matches!(
                obj.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url")
            ) {
                if obj
                    .get("image_url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| u.starts_with("data:image/"))
                {
                    obj.remove("image_url");
                } else if let Some(url) = obj.get_mut("image_url").and_then(Value::as_object_mut)
                    && url
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|u| u.starts_with("data:image/"))
                {
                    url.remove("url");
                }
            }
            if let Some(images) = obj.get_mut("images").and_then(Value::as_array_mut) {
                for image in images {
                    if image.as_str().is_some_and(|u| u.starts_with("data:image/")) {
                        *image = Value::Null;
                    }
                }
            }
            obj.values_mut().for_each(omit_image_bytes);
        }
        Value::Array(items) => items.iter_mut().for_each(omit_image_bytes),
        _ => {}
    }
}

/// Remove images only from the public copy of native inputs, echoes and thread history.
pub(crate) fn omit_codex_input_bytes(packet: &mut Value) {
    match packet["method"].as_str() {
        Some("thread/queue/add" | "turn/steer" | "turn/start") => {
            if let Some(items) = packet.pointer_mut("/params/input") {
                omit_image_bytes(items);
            }
        }
        Some("item/started" | "item/completed")
            if packet["params"]["item"]["type"] == "userMessage" =>
        {
            if let Some(content) = packet.pointer_mut("/params/item/content") {
                omit_image_bytes(content);
            }
        }
        Some("codex/event/user_message" | "codex/event")
            if packet["params"]["msg"]["type"] == "user_message" =>
        {
            if let Some(message) = packet.pointer_mut("/params/msg") {
                omit_image_bytes(message);
            }
        }
        _ => {}
    }
    if let Some(submission) = packet.pointer_mut("/result/queuedSubmission") {
        omit_image_bytes(submission);
    }
    for pointer in ["/params/turn", "/result/turn"] {
        if let Some(turn) = packet.pointer_mut(pointer) {
            omit_codex_turn_input(turn);
        }
    }
    if let Some(turns) = packet
        .pointer_mut("/result/thread/turns")
        .and_then(Value::as_array_mut)
    {
        turns.iter_mut().for_each(omit_codex_turn_input);
    }
}

pub(crate) fn omit_codex_turn_input(turn: &mut Value) {
    if let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            if item["type"] == "userMessage"
                && let Some(content) = item.get_mut("content")
            {
                omit_image_bytes(content);
            }
        }
    }
}

pub(crate) fn text_limit(runtime: &str) -> Option<usize> {
    // Antigravity can silently truncate a larger UTF-8 prompt in its native worker.
    (runtime == "antigravity").then_some(185_000)
}

pub(crate) fn check_text(text: &str, limit: Option<usize>) -> Result<()> {
    if let Some(limit) = limit
        && text.len() > limit
    {
        return Err(Error::InputTooLarge {
            bytes: text.len(),
            limit,
        });
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct InputImage {
    mime: &'static str,
    pub(crate) byte_len: usize,
    data: std::sync::Arc<str>,
    uri: Option<url::Url>,
    path: Option<PathBuf>,
}
impl InputImage {
    pub fn mime_type(&self) -> &'static str {
        self.mime
    }
    pub(crate) fn bytes(&self) -> Result<Vec<u8>> {
        STANDARD
            .decode(self.data.as_ref())
            .map_err(|_| Error::Protocol("invalid encoded image bytes".into()))
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err(Error::Unsupported(
                "image encoding; expected PNG, JPEG, GIF or WebP".into(),
            ));
        };
        Ok(Self {
            mime,
            byte_len: bytes.len(),
            data: STANDARD.encode(bytes).into(),
            uri: None,
            path: None,
        })
    }
    pub fn read(path: &Path) -> Result<Self> {
        let mut image = Self::from_bytes(&std::fs::read(path)?)?;
        image.path = Some(path.to_owned());
        image.uri = std::path::absolute(path)
            .ok()
            .and_then(|p| url::Url::from_file_path(p).ok());
        Ok(image)
    }
    fn block(&self, protocol: Protocol) -> Value {
        match protocol {
            Protocol::Cybion => self.data_url().into(),
            Protocol::KimiKap => {
                json!({"type":"image","source":{"kind":"base64","media_type":self.mime,"data":self.data.as_ref()}})
            }
            Protocol::Morph => unreachable!("Morph has no image input"),
            Protocol::Codex => {
                json!({"type":"image","url":format!("data:{};base64,{}",self.mime,self.data)})
            }
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen => {
                json!({"type":"image","source":{"type":"base64","media_type":self.mime,"data":self.data.as_ref()}})
            }
            Protocol::Factory => {
                json!({"type":"base64","mediaType":self.mime,"data":self.data.as_ref()})
            }
            Protocol::Dsh => unreachable!("DSH sends staged upload receipts"),
            Protocol::Acp | Protocol::Pi => {
                let mut block =
                    json!({"type":"image","mimeType":self.mime,"data":self.data.as_ref()});
                if protocol == Protocol::Acp
                    && let Some(uri) = &self.uri
                {
                    block["uri"] = uri.as_str().into();
                }
                block
            }
        }
    }
    pub(crate) fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }
}

impl crate::ImageSubmission {
    pub(crate) fn new<'a>(
        text: &str,
        paths: impl Iterator<Item = Option<&'a Path>>,
        secrets: &[String],
    ) -> Self {
        let mut input = text.to_owned();
        crate::transport::redact_text(&mut input, secrets);
        let images = paths
            .map(|path| crate::ImageReference {
                path: path.map(|p| {
                    let mut path = p.to_string_lossy().into_owned();
                    crate::transport::redact_text(&mut path, secrets);
                    path
                }),
            })
            .collect();
        Self { input, images }
    }

    pub(crate) fn loaded(text: &str, images: &[InputImage], secrets: &[String]) -> Option<Self> {
        (!images.is_empty())
            .then(|| Self::new(text, images.iter().map(|i| i.path.as_deref()), secrets))
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Input {
    /// Identity of this local control attempt, never part of a provider's input.
    pub request_id: uuid::Uuid,
    pub carried: Vec<uuid::Uuid>,
    pub origin: Option<crate::InputOrigin>,
    pub text: String,
    pub images: Vec<InputImage>,
}
impl Input {
    pub fn new(text: String, images: Vec<InputImage>) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4(),
            carried: Vec::new(),
            origin: None,
            text,
            images,
        }
    }
    #[cfg(test)]
    pub fn text(text: String) -> Self {
        Self::new(text, Vec::new())
    }
    pub(crate) fn dispatch_queued(mut self, id: uuid::Uuid) -> Self {
        self.request_id = uuid::Uuid::new_v4();
        if !self.carried.contains(&id) {
            self.carried.push(id);
        }
        self
    }
    pub fn blocks(&self, protocol: Protocol) -> Vec<Value> {
        if matches!(
            protocol,
            Protocol::KimiKap | Protocol::Acp | Protocol::Claude
        ) {
            let mut parts: Vec<_> = self.images.iter().map(|i| i.block(protocol)).collect();
            if !self.text.is_empty() || parts.is_empty() {
                parts.push(json!({"type":"text","text":self.text}));
            }
            return parts;
        }
        (protocol != Protocol::Codex || !self.text.is_empty() || self.images.is_empty())
            .then(|| json!({"type":"text","text":self.text}))
            .into_iter()
            .chain(self.images.iter().map(|i| i.block(protocol)))
            .collect()
    }
    pub fn stream_user(&self, protocol: Protocol, session: Option<&str>, id: uuid::Uuid) -> Value {
        let content = if self.images.is_empty() {
            Value::String(self.text.clone())
        } else {
            self.blocks(protocol).into()
        };
        json!({"type":"user","uuid":id,"session_id":session,"parent_tool_use_id":null,"message":{"role":"user","content":content}})
    }
    pub fn factory(&self, id: uuid::Uuid) -> Value {
        let mut params = json!({"messageId":id,"text":self.text});
        if !self.images.is_empty() {
            params["images"] = self
                .images
                .iter()
                .map(|image| image.block(Protocol::Factory))
                .collect::<Vec<_>>()
                .into();
        }
        params
    }
    pub fn pi(&self) -> Value {
        let mut params = json!({"message":self.text});
        if !self.images.is_empty() {
            params["images"] = self
                .images
                .iter()
                .map(|i| i.block(Protocol::Pi))
                .collect::<Vec<_>>()
                .into();
        }
        params
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_history_suppresses_user_image_bytes_and_preserves_tool_output() {
        let image = json!({"type":"image","url":"data:image/png;base64,example"});
        let user = json!({"type":"userMessage","clientId":"input","content":[image]});
        let tool = json!({"type":"mcpToolCall","result":{"content":[image]}});
        let turn = json!({"id":"turn","items":[user,tool]});
        let mut history = json!({"result":{"thread":{"turns":[turn]}}});
        omit_codex_input_bytes(&mut history);
        let items = &history["result"]["thread"]["turns"][0]["items"];
        assert_eq!(items[0]["content"][0], json!({"type":"image"}));
        assert_eq!(items[0]["clientId"], "input");
        assert_eq!(items[1], tool);

        let mut queue =
            json!({"result":{"queuedSubmission":{"clientUserMessageId":"input","input":[image]}}});
        omit_codex_input_bytes(&mut queue);
        assert_eq!(
            queue["result"]["queuedSubmission"]["input"][0],
            json!({"type":"image"})
        );
        assert_eq!(
            queue["result"]["queuedSubmission"]["clientUserMessageId"],
            "input"
        );
    }
}
