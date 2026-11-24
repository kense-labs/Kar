//! Provider-native GitHub marks, and per-account, per-file revisions for environmental marks.
use super::*;
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, Weak},
};
use uuid::Uuid;
mod github;
mod patch;
mod revisions;

#[derive(Default)]
pub(in crate::workbench) struct Gates(Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>);
impl Gates {
    fn get(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.0.lock().unwrap();
        gates.retain(|_, v| v.strong_count() > 0);
        if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }
}
pub(in crate::workbench) type Stores = BTreeMap<String, BTreeMap<String, Option<String>>>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::workbench::forge) struct Batch {
    files: Vec<Press>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Press {
    path: String,
    viewed: bool,
    revision: Option<String>,
}
async fn scope(
    app: &crate::workbench::Workbench,
    p: Uuid,
    n: u64,
    headers: &HeaderMap,
) -> Result<Context, ForgeError> {
    if n == 0 {
        return Err(ForgeError::bad("expected positive pull number"));
    }
    let mut c = Context::open(app.clone(), p).await?;
    if !matches!(
        c.kind.as_str(),
        "github" | "gitlab" | "forgejo" | "bitbucket" | "azure-devops"
    ) {
        return Err(ForgeError::unsupported("host has no viewed file state"));
    }
    if let Some(t) = credentials::resolve(&c).await? {
        c.token = Some(t);
    }
    super::super::routing::verify(&c, super::super::routing::headers(headers)?.as_ref()).await?;
    Ok(c)
}
async fn viewer_key(c: &Context, n: u64) -> Result<String, ForgeError> {
    let identity = c.execute(&Operation::Identity).await?;
    let viewer = match &identity["account"] {
        Value::Null if c.token.is_none() => "",
        v => v
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid("authenticated viewer"))?,
    };
    Ok(json!([
        c.kind,
        c.host.to_lowercase(),
        c.repository.to_lowercase(),
        n,
        viewer
    ])
    .to_string())
}
pub(in crate::workbench::forge) async fn read(
    State(app): State<crate::workbench::Workbench>,
    Path((p, n)): Path<(Uuid, u64)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ForgeError> {
    let c = scope(&app, p, n, &headers).await?;
    if c.kind == "github" {
        return github::read(&c, n).await.map(Json);
    }
    let key = viewer_key(&c, n).await?;
    let (marks, truncated) = {
        let db = app.0.catalog.lock().unwrap();
        let Some(m) = db.forge.viewed.get(&key) else {
            return Ok(Json(json!({"files":[],"truncated":false})));
        };
        (
            m.iter()
                .take(500)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>(),
            m.len() > 500,
        )
    };
    let paths = marks
        .iter()
        .filter(|(_, v)| v.is_some())
        .map(|(p, _)| p.as_str())
        .collect::<Vec<_>>();
    let revisions = revisions::available(&c, n, &paths).await;
    let files = marks
        .into_iter()
        .map(|(path, rev)| {
            let changed = rev
                .as_ref()
                .zip(revisions.get(&path))
                .is_some_and(|(old, new)| old != new);
            json!({"path":path,"state":if changed{"dismissed"}else{"viewed"}})
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"files":files,"truncated":truncated})))
}
pub(in crate::workbench::forge) async fn set(
    State(app): State<crate::workbench::Workbench>,
    Path((p, n)): Path<(Uuid, u64)>,
    headers: HeaderMap,
    Json(batch): Json<Batch>,
) -> Result<Json<Value>, ForgeError> {
    if batch.files.len() > 500
        || batch.files.iter().any(|f| {
            f.path.is_empty()
                || f.path.encode_utf16().count() > 4096
                || f.path.contains('\0')
                || f.revision.as_ref().is_some_and(|s| {
                    s.is_empty() || s != s.trim() || s.encode_utf16().count() > 256
                })
        })
    {
        return Err(ForgeError::bad("invalid viewed paths or revisions"));
    }
    // Serialize before credential resolution or a slow remote revision lookup can reorder presses.
    let gate = app.0.viewed_gates.get(format!("{p}:{n}"));
    let _held = gate.lock().await;
    let c = scope(&app, p, n, &headers).await?;
    if c.kind == "github" {
        github::set(&c, n, &batch.files).await?;
        return Ok(Json(Value::Null));
    }
    let key = viewer_key(&c, n).await?;
    let missing = batch
        .files
        .iter()
        .filter(|f| f.viewed && f.revision.is_none())
        .map(|f| f.path.as_str())
        .collect::<Vec<_>>();
    let fresh = revisions::available(&c, n, &missing).await;
    app.update(|db| {
        let entries = db.forge.viewed.entry(key.clone()).or_default();
        for f in batch.files {
            if f.viewed {
                let rev = f.revision.or_else(|| fresh.get(&f.path).cloned());
                entries.insert(f.path, rev);
            } else {
                entries.remove(&f.path);
            }
        }
        if entries.is_empty() {
            db.forge.viewed.remove(&key);
        }
        Ok(())
    })?;
    Ok(Json(Value::Null))
}
