//! DSH Web stages each image for one live session before its prompt references the receipt.
use crate::{Error, InputImage, Result, SessionOptions};
use reqwest::header;
use serde_json::{Value, json};
use std::{ffi::OsString, io::Write};
use tempfile::NamedTempFile;
use tokio::process::Command;

const TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

pub(super) fn install(
    opts: &SessionOptions,
    rows: &mut Vec<Value>,
    cmd: &mut Command,
) -> Result<Option<NamedTempFile>> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    let addon: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/dsh-images.node")));
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let addon: Option<&[u8]> = None;
    let Some(addon) = addon else { return Ok(None) };
    let probe = crate::installation::Probe::for_session(opts)?;
    let mut node = probe
        .get("NODE_OPTIONS")
        .map(OsString::from)
        .unwrap_or_default();
    if node
        .to_string_lossy()
        .split_whitespace()
        .any(|s| s == "--no-addons")
    {
        return Ok(None);
    }
    let mut file = tempfile::Builder::new()
        .prefix("kar-dsh-images-")
        .suffix(".node")
        .tempfile()?;
    file.write_all(addon)?;
    let url = url::Url::from_file_path(file.path())
        .map_err(|_| Error::Protocol("invalid image add-on path".into()))?;
    rows.push(json!({"insert":[{"id":"kar-image-types","name":url.as_str()}]}));
    node.push(" --experimental-addon-modules");
    cmd.env("NODE_OPTIONS", node);
    Ok(Some(file))
}

#[derive(Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Limits {
    pub media_types: Vec<String>,
    max_images_per_message: usize,
    max_image_bytes: usize,
    max_message_image_bytes: usize,
}
impl Limits {
    fn check(&self, images: &[InputImage]) -> Result<()> {
        if images
            .iter()
            .any(|image| !self.media_types.iter().any(|t| t == image.mime_type()))
        {
            return Err(Error::Unsupported(
                "DSH model or attachment store does not accept the requested image type".into(),
            ));
        }
        let total = images
            .iter()
            .try_fold(0usize, |total, image| total.checked_add(image.byte_len));
        if images.len() > self.max_images_per_message
            || images.iter().any(|im| im.byte_len > self.max_image_bytes)
            || total.is_none_or(|n| n > self.max_message_image_bytes)
        {
            return Err(Error::Refused(
                "DSH image input exceeds the native attachment limits".into(),
            ));
        }
        Ok(())
    }
}

pub(super) struct ImageStore<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub cookie: &'a str,
    pub timeout: std::time::Duration,
    pub enabled: bool,
}
impl ImageStore<'_> {
    pub(super) async fn image_limits(&self, id: &str, fallback: &Value) -> Limits {
        if !self.enabled {
            return Limits::default();
        }
        let mut limits = async {
            self.client
                .post(format!("{}/api/kar/imageTypes", self.base))
                .header(header::COOKIE, self.cookie)
                .timeout(self.timeout)
                .json(&json!({"sessionId":id,"fallback":fallback}))
                .send()
                .await?
                .error_for_status()?
                .json::<Limits>()
                .await
        }
        .await
        .unwrap_or_default();
        limits.media_types.retain(|t| TYPES.contains(&t.as_str()));
        if limits.max_image_bytes == 0
            || limits.max_message_image_bytes == 0
            || limits.max_images_per_message == 0
        {
            return Limits::default();
        }
        limits
    }
    pub(super) async fn stage_images(
        &self,
        id: &str,
        fallback: &Value,
        input: &crate::input::Input,
    ) -> Result<Vec<Value>> {
        let mut blocks = Vec::new();
        if !input.text.is_empty() || input.images.is_empty() {
            blocks.push(json!({"type":"text","text":input.text}));
        }
        if !input.images.is_empty() {
            self.image_limits(id, fallback).await.check(&input.images)?;
        }
        let mut receipts = std::collections::HashSet::new();
        for im in &input.images {
            let receipt = self
                .upload_image(id, im)
                .await
                .map_err(|error| match error {
                    Error::Refused(_) => error,
                    _ => Error::Refused(
                        "DSH did not confirm storage of every submitted image".into(),
                    ),
                })?;
            if !receipts.insert(receipt.clone()) {
                return Err(Error::Refused(
                    "DSH reused an image receipt before prompt admission".into(),
                ));
            }
            blocks.push(json!({"type":"file","receiptId":receipt}));
        }
        Ok(blocks)
    }
    async fn upload_image(&self, id: &str, image: &InputImage) -> Result<String> {
        let reply = self
            .client
            .post(format!("{}/api/session/uploadFileBinary", self.base))
            .header(header::COOKIE, self.cookie)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .query(&[("sessionId", id), ("mediaType", image.mime_type())])
            .body(image.bytes()?)
            .timeout(self.timeout)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        image_receipt(&reply)
    }
}
fn image_receipt(reply: &Value) -> Result<String> {
    if reply["ok"] == false {
        return Err(Error::Refused(format!(
            "DSH image upload refused ({})",
            reply
                .pointer("/error/details/reason")
                .and_then(Value::as_str)
                .unwrap_or("native_attachment_error")
        )));
    }
    let native = &reply["value"]["image"];
    if reply["ok"] != true
        || native["attachmentId"].as_str().is_none_or(str::is_empty)
        || !TYPES.contains(&native["mediaType"].as_str().unwrap_or(""))
        || !["width", "height", "bytes"]
            .iter()
            .all(|k| native[*k].is_u64())
    {
        return Err(Error::Protocol(
            "DSH omitted the stored image reference".into(),
        ));
    }
    reply["value"]["receiptId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol("DSH omitted the image upload receipt".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_the_whole_batch_before_upload_or_dispatch() {
        let image = InputImage::from_bytes(b"GIF89a").unwrap();
        let mut limits = Limits {
            media_types: vec!["image/gif".into()],
            max_images_per_message: 2,
            max_image_bytes: 6,
            max_message_image_bytes: 12,
        };
        let both = vec![image.clone(), image.clone()];
        assert!(limits.check(&both).is_ok());
        limits.max_message_image_bytes = 11;
        assert!(matches!(limits.check(&both), Err(Error::Refused(_))));
        limits.max_image_bytes = 5;
        assert!(matches!(
            limits.check(std::slice::from_ref(&image)),
            Err(Error::Refused(_))
        ));
        limits.max_image_bytes = 6;
        limits.max_message_image_bytes = 12;
        limits.max_images_per_message = 1;
        assert!(matches!(limits.check(&both), Err(Error::Refused(_))));
        limits.media_types = vec!["image/jpeg".into()];
        assert!(matches!(
            limits.check(std::slice::from_ref(&image)),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            Limits::default().check(&both),
            Err(Error::Unsupported(_))
        ));
    }
    #[test]
    fn requires_both_a_staged_receipt_and_a_native_image() {
        let good = json!({"ok":true,"value":{"receiptId":"r1","image":{"attachmentId":"i1","mediaType":"image/png","width":3,"height":2,"bytes":100}}});
        assert_eq!(image_receipt(&good).unwrap(), "r1");
        for key in ["receiptId", "image"] {
            let mut bad = good.clone();
            bad["value"].as_object_mut().unwrap().remove(key);
            assert!(image_receipt(&bad).is_err());
        }
        let rejected = json!({"ok":false,"error":{"details":{"reason":"UNSUPPORTED_IMAGE_TYPE"}}});
        assert!(
            matches!(image_receipt(&rejected),Err(Error::Refused(reason)) if reason.contains("UNSUPPORTED_IMAGE_TYPE"))
        );
    }
}

#[cfg(test)]
mod protocol {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Query, State},
        routing::post,
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[derive(Clone, Default)]
    struct Server {
        mode: Arc<AtomicUsize>,
        uploads: Arc<Mutex<Vec<Vec<u8>>>>,
    }
    async fn limits(
        State(state): State<Server>,
        h: axum::http::HeaderMap,
        Json(req): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(h["cookie"], "auth=scope");
        assert_eq!(req["sessionId"], "selected");
        assert_eq!(req["fallback"]["model"], "test");
        if state.mode.load(Ordering::Relaxed) == 1 {
            return Json(json!({}));
        }
        Json(
            json!({"maxImagesPerMessage":2,"maxImageBytes":10,"maxMessageImageBytes":18,"mediaTypes":["image/gif","image/png","text/html"]}),
        )
    }
    async fn store(
        State(state): State<Server>,
        h: axum::http::HeaderMap,
        Query(q): Query<HashMap<String, String>>,
        body: Bytes,
    ) -> Json<Value> {
        assert_eq!(h["cookie"], "auth=scope");
        assert_eq!(h["content-type"], "application/octet-stream");
        assert_eq!(q["sessionId"], "selected");
        let total = {
            let mut u = state.uploads.lock().unwrap();
            u.push(body.to_vec());
            u.len()
        };
        let id = format!("receipt-{total}");
        if total % 2 == 0 && state.mode.load(Ordering::Relaxed) == 2 {
            return Json(json!({"ok":true,"value":{"receiptId":id}}));
        }
        Json(
            json!({"ok":true,"value":{"receiptId":id,"image":{"attachmentId":format!("image-{total}"),"mediaType":q["mediaType"],"width":4,"height":4,"bytes":body.len()}}}),
        )
    }
    #[tokio::test]
    async fn negotiated_mime_and_session_scoped_uploads_precede_prompt_references() {
        let state = Server::default();
        let app = Router::new()
            .route("/api/kar/imageTypes", post(limits))
            .route("/api/session/uploadFileBinary", post(store))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let api = ImageStore {
            client: &client,
            base: &base,
            cookie: "auth=scope",
            timeout: Duration::from_secs(2),
            enabled: true,
        };
        let fallback = json!({"provider":"local","model":"test"});
        let gif = InputImage::from_bytes(b"GIF89a").unwrap();
        let png = InputImage::from_bytes(b"\x89PNG\r\n\x1a\n").unwrap();
        let input = crate::input::Input::new(String::new(), vec![png.clone(), gif.clone()]);
        let limits = api.image_limits("selected", &fallback).await;
        assert_eq!(limits.media_types, ["image/gif", "image/png"]);
        assert_eq!(
            api.stage_images("selected", &fallback, &input)
                .await
                .unwrap(),
            [
                json!({"type":"file","receiptId":"receipt-1"}),
                json!({"type":"file","receiptId":"receipt-2"})
            ]
        );
        assert_eq!(
            *state.uploads.lock().unwrap(),
            [png.bytes().unwrap(), gif.bytes().unwrap()]
        );
        state.mode.store(1, Ordering::Relaxed);
        assert!(matches!(
            api.stage_images("selected", &fallback, &input).await,
            Err(Error::Unsupported(_))
        ));
        assert_eq!(state.uploads.lock().unwrap().len(), 2);
        state.mode.store(2, Ordering::Relaxed);
        assert!(matches!(
            api.stage_images("selected", &fallback, &input).await,
            Err(Error::Refused(_))
        ));
        assert_eq!(state.uploads.lock().unwrap().len(), 4);
        server.abort();
    }
}
