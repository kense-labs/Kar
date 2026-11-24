//! Resolve and install the pinned tunnel client in Kar's private state.
pub(super) mod tunnel;
use super::{Workbench, http::ApiError};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use futures_util::stream;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    fs::{self, File},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::sync::mpsc;

const PIN: &str = "2026.10.4";
const PROGRAM: &str = "cloudflared";
fn release() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some((
            "arm64",
            "68b6bc4080ac1330234db2a9002d07f98b000537a1bd531a687b660d4160fa7f",
        )),
        ("macos", "x86_64") => Some((
            "amd64",
            "f11fbef4207d715892331917dafd437f032cf4b7137ed64dea3008338c1c613d",
        )),
        _ => None,
    }
}
async fn version(path: &Path) -> Option<semver::Version> {
    if !path.is_file() {
        return None;
    }
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(8), command.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    if parts.next()? != PROGRAM || parts.next()? != "version" {
        return None;
    }
    semver::Version::parse(parts.next()?)
        .ok()
        .filter(|v| v >= &semver::Version::new(2026, 10, 3))
}
async fn resolve(app: &Workbench) -> Result<Value> {
    let dir = app.0.state.join("relay-client");
    let override_path = std::env::var_os("KAR_CLOUDFLARED_PATH").filter(|p| !p.is_empty());
    let mut paths = Vec::new();
    if let Some(p) = override_path {
        paths.push(("override", PathBuf::from(p)));
    } else {
        paths.push(("managed", dir.join(PIN).join(PROGRAM)));
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(
                std::env::split_paths(&path)
                    .filter(|p| p.is_absolute())
                    .map(|p| ("path", p.join(PROGRAM))),
            );
        }
    }
    for (source, path) in paths {
        if let Some(v) = version(&path).await {
            return Ok(
                json!({"status":"available","source":source,"version":v.to_string(),"executable_path":path.canonicalize()?}),
            );
        }
        if source == "override" {
            return Err(Error::Refused(
                "KAR_CLOUDFLARED_PATH must identify cloudflared 2026.10.3 or newer".into(),
            ));
        }
    }
    if release().is_none() {
        return Ok(
            json!({"status":"unsupported","platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"version":PIN}),
        );
    }
    Ok(json!({"status":"missing","version":PIN}))
}
async fn install(app: &Workbench, stage: impl Fn(&'static str) + Send + Sync) -> Result<Value> {
    stage("checking");
    let status = resolve(app).await?;
    if status["status"] != "missing" {
        return Ok(status);
    }
    let (arch, sha) = release()
        .ok_or_else(|| Error::Unsupported("managed relay client on this platform".into()))?;
    let root = app.0.state.join("relay-client");
    fs::create_dir_all(&root)?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("install.lock"))?;
    stage("waiting_for_lock");
    let _held = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
        lock.lock()?;
        Ok(lock)
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    let status = resolve(app).await?;
    if status["status"] != "missing" {
        return Ok(status);
    }
    let tmp = tempfile::tempdir_in(&root)?;
    let url=format!("https://github.com/cloudflare/cloudflared/releases/download/{PIN}/cloudflared-darwin-{arch}.tgz").parse().map_err(|_|Error::Protocol("invalid pinned relay URL".into()))?;
    let file =
        super::acp_registry::pinned_executable(tmp.path(), url, PROGRAM, sha, &stage).await?;
    stage("validating");
    if version(&file).await.is_none_or(|v| v.to_string() != PIN) {
        return Err(Error::Refused(
            "relay client failed release version validation".into(),
        ));
    }
    stage("activating");
    let target = root.join(PIN);
    fs::create_dir_all(&target)?;
    File::open(&file)?.sync_all()?;
    fs::rename(file, target.join(PROGRAM))?;
    File::open(target)?.sync_all()?;
    resolve(app).await
}
async fn status(State(app): State<Workbench>) -> std::result::Result<Json<Value>, ApiError> {
    Ok(Json(resolve(&app).await?))
}
async fn start(
    State(app): State<Workbench>,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let result = install(&app, |stage| {
            let _ = tx.send(json!({"type":"progress","stage":stage}));
        })
        .await;
        let _ = tx.send(match result {
            Ok(status) => json!({"type":"result","status":status}),
            Err(e) => json!({"type":"error","message":e.to_string()}),
        });
    });
    Sse::new(stream::unfold(rx, |mut rx| async move {
        let value = rx.recv().await?;
        Some((Ok(Event::default().data(value.to_string())), rx))
    }))
    .keep_alive(KeepAlive::default())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new().route("/access/relay-client", get(status).post(start))
}
