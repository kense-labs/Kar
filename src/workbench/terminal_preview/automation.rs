use super::*;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::BTreeSet;

type Ids = BTreeSet<Uuid>;
#[allow(clippy::too_many_arguments)]
pub(in crate::workbench) async fn call(
    app: &Workbench,
    project: Uuid,
    chat: Uuid,
    shared: &Ids,
    terminals: &Ids,
    origins: &BTreeSet<String>,
    upload: bool,
    name: &str,
    mut input: Value,
    check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
) -> Result<Value> {
    let state = &app.0.terminal_preview;
    let args = input.as_object_mut().ok_or_else(missing)?;
    let tab: Option<Uuid> = serde_json::from_value(args.remove("tab_id").unwrap_or(Value::Null))?;
    if name == "kar_preview_open" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Open {
            terminal_id: Option<Uuid>,
            url: Option<String>,
            port: Option<u16>,
            path: Option<String>,
            reuse_existing_tab: Option<bool>,
        }
        let q: Open = serde_json::from_value(input)?;
        let grant = if let Some(id) = tab {
            if q.url.is_some()
                || q.terminal_id.is_some()
                || q.port.is_some()
                || q.reuse_existing_tab == Some(false)
            {
                return Err(Error::Refused(
                    "existing tab conflicts with a new listener".into(),
                ));
            }
            state.model_grant(project, chat, shared, terminals, id)?
        } else if let Some(url) = &q.url {
            if q.terminal_id.is_some() || q.port.is_some() {
                return Err(Error::Refused(
                    "pass a public URL or a terminal listener".into(),
                ));
            }
            let url = reqwest::Url::parse(url)
                .map_err(|_| Error::Refused("invalid public URL".into()))?;
            state
                .grants
                .open_public(project, chat, url, origins.clone(), check.clone())
                .await?
        } else {
            let terminal = q
                .terminal_id
                .ok_or_else(|| Error::Refused("specify terminal_id and port".into()))?;
            let port = q
                .port
                .ok_or_else(|| Error::Refused("specify terminal_id and port".into()))?;
            if !terminals.contains(&terminal) {
                return Err(missing());
            }
            let term = state.find(project, terminal)?;
            let root = term.snapshot().process.ok_or_else(missing)?;
            let mut ports = tokio::task::spawn_blocking(move || process::ports(root))
                .await
                .map_err(|e| Error::Closed(e.to_string()))??;
            ports.retain(|p| p.port == port);
            ports.sort_by_key(|p| !p.address.is_ipv4());
            let target = *ports.first().ok_or_else(|| {
                Error::Refused("port is not open in the granted project terminal".into())
            })?;
            let existing = state.grants.list(project, None).into_iter().find(|t| {
                t.chat == Some(chat) && t.terminal == Some(terminal) && t.target == Some(target)
            });
            check()?;
            let id = if let Some(t) = existing.filter(|_| q.reuse_existing_tab != Some(false)) {
                t.id
            } else {
                state
                    .grants
                    .open(
                        project,
                        Some(chat),
                        term,
                        target,
                        None,
                        "incognito".into(),
                        Default::default(),
                    )
                    .await?
                    .id
            };
            state.model_grant(project, chat, shared, terminals, id)?
        };
        if grant.info.runtime == "desktop" {
            return Err(Error::Unsupported("this preview is owned by the paired desktop; automatic actions require a server tab".into()));
        }
        check()?;
        if let Some(path) = q.path {
            state
                .browsers
                .action(grant.clone(), "kar_preview_navigate", &json!({"url":path}))
                .await?;
        }
        state
            .browsers
            .control(grant.clone(), None, Default::default())
            .await?;
        return state.browsers.model_status(grant).await;
    }
    let id = if let Some(tab) = tab {
        tab
    } else {
        let visible = state.model_tabs(project, chat, shared, terminals);
        let [only] = visible.as_slice() else {
            return Err(Error::Refused(
                "specify a tab_id from kar_preview_list".into(),
            ));
        };
        serde_json::from_value(only["tab_id"].clone())?
    };
    let grant = state.model_grant(project, chat, shared, terminals, id)?;
    check()?;
    if grant.info.runtime == "desktop" {
        return Err(Error::Unsupported(
            "this preview is owned by the paired desktop; automatic actions require a server tab"
                .into(),
        ));
    }
    let browsers = &state.browsers;
    match name {
        "kar_preview_status" => browsers.model_status(grant).await,
        "kar_preview_resize" => {
            browsers
                .control(
                    grant,
                    Some(serde_json::from_value(input["viewport"].clone())?),
                    Default::default(),
                )
                .await
        }
        "kar_preview_set_appearance" => {
            browsers
                .control(
                    grant,
                    None,
                    browser::Adjust {
                        color_scheme: Some(serde_json::from_value(input["appearance"].clone())?),
                        ..Default::default()
                    },
                )
                .await
        }
        "kar_preview_snapshot" => {
            let mut meta = browsers.action(grant.clone(), name, &input).await?;
            if input["include_image"] == true || input["save"] == true {
                let bytes = browsers.screen(grant).await?;
                check()?;
                if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    return Err(Error::Protocol("invalid PNG from Chromium".into()));
                }
                let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
                let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
                if input["save"] == true {
                    meta["attachment"] = json!(app.0.chats.persist_attachment(
                        chat,
                        Uuid::new_v4(),
                        "preview.png",
                        "image/png",
                        crate::chat::AttachmentKind::Image,
                        &bytes
                    )?);
                }
                if input["include_image"] == true {
                    meta["screenshot"] = json!({"mimeType":"image/png","width":width,"height":height,"data":STANDARD.encode(bytes)});
                }
            }
            Ok(meta)
        }
        "kar_preview_act" if input["action"] == "upload" => {
            if !upload {
                return Err(Error::Refused(
                    "workspace upload permission required".into(),
                ));
            }
            let cwd = app.binding(project, chat)?.workspace;
            let ws = app.workspace_at(&cwd)?;
            let paths: Vec<PathBuf> = serde_json::from_value(input["paths"].clone())?;
            if paths.len() > 20 {
                return Err(Error::Refused("too many upload files".into()));
            }
            let dir = tempfile::tempdir()?;
            let mut copies = Vec::new();
            let mut remaining = 50 * 1024 * 1024;
            for (i, path) in paths.iter().enumerate() {
                let rel = ws.target(
                    path.strip_prefix(&cwd)
                        .map_err(|_| crate::workbench::denied())?,
                    false,
                )?;
                let bytes = ws.read_bytes(&rel, remaining)?;
                remaining -= bytes.len();
                let dest = dir.path().join(i.to_string());
                std::fs::create_dir(&dest)?;
                let dest = dest.join(path.file_name().ok_or_else(missing)?);
                std::fs::write(&dest, bytes)?;
                copies.push(dest);
            }
            input["paths"] = json!(copies);
            check()?;
            browsers.upload(grant, &input, dir).await
        }
        "kar_preview_recording_start" => browsers.record_start(grant, check).await,
        "kar_preview_recording_stop" => {
            let (video, at) = browsers.record_stop(grant).await?;
            check()?;
            let a = app.0.chats.persist_attachment(
                chat,
                Uuid::new_v4(),
                "preview.webm",
                "video/webm",
                crate::chat::AttachmentKind::File,
                &video,
            )?;
            Ok(
                json!({"id":a.id,"tab_id":id,"mimeType":a.mime,"sizeBytes":a.bytes,"createdAt":at,"attachment":a}),
            )
        }
        "kar_preview_wait_for"
        | "kar_preview_dialog"
        | "kar_preview_evaluate"
        | "kar_preview_act"
        | "kar_preview_navigate" => {
            let result = browsers.action(grant, name, &input).await?;
            check()?;
            Ok(result)
        }
        _ => Err(Error::Unsupported("unknown preview operation".into())),
    }
}
