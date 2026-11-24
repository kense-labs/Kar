//! Repository-scoped views; cursors always carry the filters and source identity they continue.
use super::*;
use crate::workbench::Catalog;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use providers::issue::child;
use sha2::{Digest, Sha256};
mod list;
mod sections;
fn encode<T: Serialize>(value: &T) -> Result<String, ForgeError> {
    serde_json::to_vec(value)
        .map(|v| URL_SAFE_NO_PAD.encode(v))
        .map_err(|_| ForgeError::bad("invalid cursor"))
}
fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, ForgeError> {
    if raw.len() > 16384 {
        return Err(ForgeError::bad("cursor too large"));
    }
    URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .ok_or_else(|| ForgeError::bad("invalid pull cursor"))
}
fn signature(c: &Context, v: &Value) -> String {
    let mut h = Sha256::new();
    h.update(c.identity());
    h.update(v.to_string());
    format!("{:x}", h.finalize())
}
pub(in crate::workbench) async fn call(
    app: Workbench,
    p: Uuid,
    chat: Uuid,
    detail: bool,
    input: Value,
    authorize: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    app.binding(p, chat)?;
    let mut c = Context::open(app.clone(), p).await?;
    if let Some(token) = providers::credentials::resolve(&c).await? {
        c.token = Some(token);
    }
    let identity = c.identity();
    authorize(&app.0.catalog.lock().unwrap(), &identity)?;
    let result = if detail {
        sections::read(
            &app,
            p,
            &c,
            serde_json::from_value(input).map_err(|e| ForgeError::bad(e.to_string()))?,
        )
        .await?
    } else {
        list::read(
            &c,
            serde_json::from_value(input).map_err(|e| ForgeError::bad(e.to_string()))?,
        )
        .await?
    };
    authorize(&app.0.catalog.lock().unwrap(), &identity)?;
    Ok(result)
}
fn repo(c: &Context) -> Result<Url, ForgeError> {
    match c.kind.as_str() {
        "azure-devops" => providers::azure::git_endpoint(c, &[]),
        "gitcafe" | "tanstack-source" | "cursor-origin" => {
            providers::hosted::resource(c, None, None)
        }
        _ => providers::rest::project(c),
    }
}
