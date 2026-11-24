use super::Session;
use crate::{Error, ImageSubmission, InputImage, Result, Turn};
use serde_json::json;
use std::path::{Path, PathBuf};
use uuid::Uuid;

impl Session {
    /// Submit local files as a single input. Each path retains the spelling
    /// supplied by the caller in the public request, including relative paths.
    pub async fn prompt_image_paths(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
    ) -> Result<Turn> {
        let id = Uuid::new_v4();
        let text = text.into();
        let images = self.load_image_paths("prompt", id, &text, paths).await?;
        self.prompt_images_with_id(id, text, images).await
    }

    /// Read all files at queue admission so later file changes cannot alter this input.
    pub async fn queue_image_paths_with_id(
        &self,
        id: Uuid,
        text: impl Into<String>,
        paths: &[PathBuf],
    ) -> Result<Turn> {
        let text = text.into();
        let images = self.load_image_paths("queue", id, &text, paths).await?;
        self.queue_images_with_id(id, text, images).await
    }

    async fn load_image_paths(
        &self,
        control: &str,
        id: Uuid,
        text: &str,
        paths: &[PathBuf],
    ) -> Result<Vec<InputImage>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let refused = |error| {
            let submission = ImageSubmission::new(
                text,
                paths.iter().map(|p| Some(p.as_path())),
                &self.preflight_secrets,
            );
            self.refuse_input(control, id, Uuid::new_v4(), submission, error)
        };
        if !self.image_input {
            return Err(refused(Error::Unsupported(
                "runtime does not advertise image input".into(),
            )));
        }
        let files = paths.to_vec();
        tokio::task::spawn_blocking(move || {
            files.iter().map(|p| load(p)).collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
        .map_err(refused)
    }

    pub(super) fn check_input(
        &self,
        control: &str,
        id: Uuid,
        request: Uuid,
        text: &str,
        images: &[InputImage],
    ) -> Result<()> {
        let reason = if text.is_empty() && images.is_empty() {
            crate::input::EMPTY_INPUT
        } else if !images.is_empty() && control == "steer" && !self.capabilities().steering {
            "runtime does not support image steering"
        } else if !images.is_empty() && !self.image_input {
            "runtime does not advertise image input"
        } else {
            return Ok(());
        };
        let submission = ImageSubmission::loaded(text, images, &self.preflight_secrets)
            .unwrap_or_else(|| ImageSubmission {
                input: String::new(),
                images: vec![],
            });
        Err(self.refuse_input(
            control,
            id,
            request,
            submission,
            Error::Unsupported(reason.into()),
        ))
    }

    fn refuse_input(
        &self,
        control: &str,
        id: Uuid,
        request: Uuid,
        submission: ImageSubmission,
        error: Error,
    ) -> Error {
        let (code, mut reason) = match &error {
            Error::Unsupported(reason) => ("unsupported", reason.clone()),
            _ => ("error", error.to_string()),
        };
        crate::transport::redact_text(&mut reason, &self.preflight_secrets);
        let mut body =
            json!({"id":request,"inputId":id,"input":submission.input,"images":submission.images});
        let mut stamp = crate::stream::RecordStamp::from(crate::Direction::ToRuntime);
        if control == "prompt" {
            // Earlier recordings use type=prompt for an actual native dispatch.
            body["control"] = "prompt".into();
            stamp = stamp.with_prompt_request(
                id,
                self.protocol == crate::Protocol::Pi,
                request,
                (!submission.images.is_empty()).then_some(submission),
            );
        } else {
            body["type"] = control.into();
        }
        self.journal
            .refused_input(stamp, body, request, code, &reason)
            .err()
            .unwrap_or(error)
    }
}

fn load(path: &Path) -> Result<InputImage> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    ) {
        return Err(Error::Unsupported(format!(
            "not a png, jpeg, gif or webp image: {}",
            path.display()
        )));
    }
    InputImage::read(path).map_err(|error| match error {
        Error::Io(e) => Error::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read image {}: {e}", path.display()),
        )),
        other => other,
    })
}
