//! A tracker-level link survives workspace moves and resolving a PR or issue through another project.
use super::*;
use axum::extract::{Query, State};
use std::{cmp::Reverse, collections::BTreeSet};

#[derive(Deserialize)]
pub(super) struct Filter {
    kind: String,
    repository: String,
    number: u64,
}

pub(super) async fn read(State(app): State<Workbench>, Query(key): Query<Filter>) -> Reply<Value> {
    if !matches!(key.kind.as_str(), "issue" | "pull")
        || key.number == 0
        || key.repository.is_empty()
        || key.repository.len() > 8192
    {
        return Err(ForgeError::bad(
            "expected a forge binding repository, issue or pull, and a positive number",
        ));
    }
    tokio::task::spawn_blocking(move || linked(&app, &key))
        .await
        .map_err(|e| ForgeError::bad(e.to_string()))?
        .map(Json)
}

fn linked(app: &Workbench, key: &Filter) -> Result<Value, ForgeError> {
    let targets = app
        .0
        .catalog
        .lock()
        .unwrap()
        .forge
        .bindings
        .values()
        .filter(|b| {
            b.kind == key.kind
                && b.number == key.number
                && b.repository.eq_ignore_ascii_case(key.repository.trim())
        })
        .map(|b| (b.chat, b.project))
        .collect::<BTreeSet<_>>();
    let mut threads = Vec::with_capacity(targets.len());
    for (id, project) in targets {
        let info = match app.0.chats.saved_info(id) {
            Ok(v) => v,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let (metadata, archived, link_activity) = {
            let catalog = app.0.catalog.lock().unwrap();
            let Some(p) = catalog.projects.get(&project) else {
                continue;
            };
            if p.metadata.deleted
                || app.allowed(&p.root).is_err()
                || app.allowed(&info.workspace).is_err()
                || !app.owns_chat(&catalog, p, &info)?
            {
                continue;
            }
            (
                catalog
                    .thread_state
                    .rows
                    .get(&id)
                    .cloned()
                    .unwrap_or_default(),
                catalog.archived.contains(&id),
                catalog.forge.link_activity.get(&id).copied().unwrap_or(0),
            )
        };
        let archived_at = metadata
            .archived_ms
            .and_then(|ms| i64::try_from(ms).ok())
            .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
        let latest = metadata
            .updated_ms
            .max(link_activity)
            .max(app.0.chats.activity_ms(id)?);
        let settled = metadata.settled_ms.is_some()
            || metadata.settled_override.as_deref() == Some("settled");
        let title = app.display_title(&super::super::ProjectChat {
            project,
            info,
            archived,
            metadata,
        })?;
        threads.push((Reverse(latest),id,json!({"id":id,"projectId":project,"title":title,"archived":archived,"archivedAt":archived_at.map(|t|t.to_string()),"settled":settled})));
    }
    threads.sort_unstable_by_key(|(latest, id, _)| (*latest, *id));
    Ok(json!({"threads":threads.into_iter().map(|(_,_,v)|v).collect::<Vec<_>>()}))
}
