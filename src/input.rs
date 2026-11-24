use crate::{Error, Protocol, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const EMPTY_INPUT: &str = "empty input: give text or images";

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
    data: String,
    uri: Option<url::Url>,
    path: Option<PathBuf>,
}
impl InputImage {
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
            data: STANDARD.encode(bytes),
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
                json!({"type":"image","source":{"kind":"base64","media_type":self.mime,"data":self.data}})
            }
            Protocol::Morph => unreachable!("Morph has no image input"),
            Protocol::Codex => {
                json!({"type":"image","url":format!("data:{};base64,{}",self.mime,self.data)})
            }
            Protocol::Claude | Protocol::Qoder | Protocol::Qwen => {
                json!({"type":"image","source":{"type":"base64","media_type":self.mime,"data":self.data}})
            }
            Protocol::Factory => json!({"type":"base64","mediaType":self.mime,"data":self.data}),
            Protocol::Dsh => json!({"type":"image","mediaType":self.mime,"data":self.data}),
            Protocol::Acp | Protocol::Pi => {
                let mut block = json!({"type":"image","mimeType":self.mime,"data":self.data});
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
    pub text: String,
    pub images: Vec<InputImage>,
}
impl Input {
    pub fn text(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
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
        std::iter::once(json!({"type":"text","text":self.text}))
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
