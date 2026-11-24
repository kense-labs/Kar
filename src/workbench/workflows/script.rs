//! Read the native Claude workflow script without executing it.
use super::super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{
    Error, Result,
    provider_instance::{self, HistoryRuntime},
};
use axum::{Json, Router, extract::Query, routing::get};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Component, PathBuf},
};

const CAP: u64 = 256 * 1024;
#[derive(Deserialize)]
struct Script {
    path: PathBuf,
    instance: Option<String>,
}
fn contents(request: Script) -> Result<Value> {
    if !request.path.is_absolute() || request.path.extension().is_none_or(|ext| ext != "js") {
        return Err(Error::Refused("invalid workflow script path".into()));
    }
    let id = request.instance.as_deref().unwrap_or("claude");
    let provider = provider_instance::configured()?
        .into_iter()
        .find(|i| i.id == id && i.runtime == HistoryRuntime::Claude)
        .ok_or_else(|| Error::Refused("Claude project root unavailable".into()))?;
    let path = request.path.canonicalize()?;
    let relative = path
        .strip_prefix(&provider.sessions)
        .map_err(|_| Error::Refused("workflow script outside Claude project root".into()))?;
    if path.extension().is_none_or(|ext| ext != "js") {
        return Err(Error::Refused(
            "resolved workflow script is not JavaScript".into(),
        ));
    }
    let mut dir = Dir::open_ambient_dir(&provider.sessions, cap_std::ambient_authority())?;
    for component in relative
        .parent()
        .ok_or_else(|| Error::Refused("expected a script".into()))?
        .components()
    {
        let Component::Normal(name) = component else {
            return Err(Error::Refused("invalid workflow directory".into()));
        };
        dir = dir.open_dir_nofollow(name)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK);
    }
    let name = relative
        .file_name()
        .ok_or_else(|| Error::Refused("expected a workflow script".into()))?;
    let file = dir.open_with(name, &options)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::Refused("expected a regular workflow script".into()));
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let visible = dir.symlink_metadata(name)?;
        if (meta.dev(), meta.ino()) != (visible.dev(), visible.ino()) {
            return Err(Error::Refused("workflow script changed during read".into()));
        }
    }
    let mut bytes = vec![];
    file.take(CAP + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > CAP;
    bytes.truncate(CAP as usize);
    Ok(json!({"script_path":path,"contents":String::from_utf8_lossy(&bytes),"truncated":truncated}))
}
async fn script(Query(request): Query<Script>) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || contents(request)).await
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new().route("/workflows/scripts", get(script))
}
