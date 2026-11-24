//! Durable PR watches link the pull to a chat, baseline old activity, and reserve each wake
//! before submitting it through the chat's ordinary idempotent input queue.
mod observe;
use super::*;
use crate::workbench::{Catalog, mcp::Policy};
use observe::Baseline;
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::workbench) enum Until {
    Checks,
    Merged,
}
#[derive(Clone, Serialize, Deserialize)]
struct Notice {
    id: Uuid,
    text: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Watch {
    id: Uuid,
    project: Uuid,
    chat: Uuid,
    repo: String,
    number: u64,
    until: Until,
    active: bool,
    last: Baseline,
    next_ms: u64,
    comments_only: u8,
    awaiting_since: Option<u64>,
    pending: Option<Notice>,
    error: Option<String>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Command {
    project: Uuid,
    chat: Uuid,
    repo: String,
    number: u64,
    action: String,
    until: Option<Until>,
    result: Value,
}
fn now() -> u64 {
    jiff::Timestamp::now().as_millisecond().max(0) as u64
}
fn permitted(db: &Catalog, row: &Watch) -> bool {
    !db.model_tasks.contains_key(&row.chat)
        && Policy::current(db, row.project, row.chat)
            .is_some_and(|p| p.writes_pulls() && p.pull_repository.as_deref() == Some(&row.repo))
}
pub(in crate::workbench) fn waiting(db: &Catalog) -> BTreeSet<Uuid> {
    db.forge
        .watches
        .values()
        .filter(|w| w.active && permitted(db, w) && (w.until == Until::Merged || !w.last.settled()))
        .map(|w| w.chat)
        .collect()
}
pub(in crate::workbench) async fn command(
    app: Workbench,
    p: Uuid,
    chat: Uuid,
    n: u64,
    cmd: Option<Uuid>,
    action: &str,
    until: Option<Until>,
    authorize: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    if n == 0 {
        return Err(ForgeError::bad("invalid pull number"));
    }
    app.binding(p, chat)?;
    let c = Context::open(app.clone(), p).await?;
    let repo = c.identity();
    let lookup = |db: &Catalog| {
        db.forge
            .watches
            .values()
            .find(|r| r.project == p && r.chat == chat && r.repo == repo && r.number == n)
            .cloned()
    };
    let key = crate::chat::stable_id([
        chat.as_bytes().as_slice(),
        repo.as_bytes(),
        &n.to_be_bytes(),
        b"pull watch",
    ]);
    let gate = app.0.mcp.command_gate(key);
    let _guard = gate.lock().await;
    let request = Command {
        project: p,
        chat,
        repo: repo.clone(),
        number: n,
        action: action.to_owned(),
        until,
        result: Value::Null,
    };
    let receipt = |db: &Catalog| -> Result<Option<Value>, ForgeError> {
        authorize(db, &repo)?;
        if db.model_tasks.contains_key(&chat) {
            return Err(ForgeError::bad("delegated chats cannot own pull watches"));
        }
        if let Some(old) = cmd.and_then(|id| db.forge.watch_commands.get(&id)) {
            let mut same = old.clone();
            same.result = Value::Null;
            if same != request {
                return Err(ForgeError::bad("watch command ID already used"));
            }
            return Ok(Some(old.result.clone()));
        }
        Ok(None)
    };
    {
        let db = app.0.catalog.lock().unwrap();
        if let Some(result) = receipt(&db)? {
            return Ok(result);
        }
        if action == "watch_status" {
            let r = lookup(&db);
            return Ok(
                json!({"number":n,"watching":r.as_ref().is_some_and(|w|w.active),"until":r.as_ref().map(|w|w.until),"head_sha":r.as_ref().and_then(|w|w.last.head.as_deref()),"error":r.and_then(|w|w.error)}),
            );
        }
    }
    let cmd = cmd.ok_or_else(|| ForgeError::bad("watch command needs a command ID"))?;
    let baseline = if action == "watch" {
        Some(observe::poll(&c, n).await?)
    } else {
        None
    };
    let result = app.update(|db| {
        if let Some(result) = receipt(db).map_err(|e| Error::Refused(e.detail))? {
            return Ok(result);
        }
        let prior = lookup(db);
        let was = prior.as_ref().is_some_and(|w| w.active);
        let result = if let Some(snapshot) = baseline {
            if !matches!(snapshot.state.as_str(), "open" | "opened") {
                if let Some(w) = db.forge.watches.get_mut(&key) {
                    w.active = false;
                }
                json!({"number":n,"was_watching":was,"watching":false,"state":snapshot.state})
            } else {
                let stamp = now();
                let id = prior.as_ref().map(|w| w.id).unwrap_or(key);
                let mut row = if was {
                    prior.unwrap()
                } else {
                    Watch {
                        id,
                        project: p,
                        chat,
                        repo: repo.clone(),
                        number: n,
                        until: Until::Checks,
                        active: true,
                        last: snapshot,
                        comments_only: 0,
                        awaiting_since: Some(stamp),
                        next_ms: stamp + 30_000,
                        pending: None,
                        error: None,
                    }
                };
                if let Some(until) = until {
                    row.until = until;
                }
                let done = row.last.passed();
                let until = row.until;
                db.forge.watches.insert(id, row);
                if !db.forge.bindings.values().any(|b| {
                    b.project == p
                        && b.chat == chat
                        && b.kind == "pull"
                        && b.repository == repo
                        && b.number == n
                }) {
                    db.forge.bindings.insert(
                        Uuid::new_v4(),
                        Binding {
                            project: p,
                            chat,
                            repository: repo.clone(),
                            kind: "pull".into(),
                            number: n,
                        },
                    );
                    db.forge.links_changed()?;
                    db.forge.link_activity.insert(chat, stamp);
                }
                json!({"number":n,"was_watching":was,"watching":true,"until":until,"passed":done})
            }
        } else {
            if let Some(old) = prior
                && let Some(w) = db.forge.watches.get_mut(&old.id)
            {
                w.active = false;
                w.pending = None;
            }
            json!({"number":n,"was_watching":was,"watching":false})
        };
        db.forge.watch_commands.insert(
            cmd,
            Command {
                result: result.clone(),
                ..request.clone()
            },
        );
        Ok(result)
    })?;
    Ok(result)
}
pub(in crate::workbench) fn recover(app: &Workbench) {
    let stamp = now();
    let ids: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .forge
        .watches
        .values()
        .filter(|r| r.pending.is_some() || r.active && r.next_ms <= stamp)
        .map(|r| r.id)
        .collect();
    for id in ids {
        let app = app.clone();
        tokio::spawn(async move {
            let gate = app.0.mcp.command_gate(id);
            let Ok(_guard) = gate.try_lock() else { return };
            let row = {
                let db = app.0.catalog.lock().unwrap();
                db.forge.watches.get(&id).cloned()
            };
            let Some(r) = row else { return };
            let allowed = {
                let db = app.0.catalog.lock().unwrap();
                permitted(&db, &r)
            };
            if !allowed {
                let _ = app.update(|db| {
                    if let Some(w) = db.forge.watches.get_mut(&id) {
                        w.active = false;
                        w.pending = None;
                    }
                    Ok(())
                });
                return;
            }
            if let Some(n) = &r.pending {
                if app.0.chats.enqueue(r.chat, n.id, n.text.clone()).is_ok() {
                    let _ = app.update(|db| {
                        if let Some(w) = db.forge.watches.get_mut(&id)
                            && w.pending.as_ref().is_some_and(|v| v.id == n.id)
                        {
                            w.pending = None;
                        }
                        Ok(())
                    });
                    app.wake_queue();
                }
                return;
            }
            if !r.active || r.next_ms > now() {
                return;
            }
            let observed = async {
                let c = Context::open(app.clone(), r.project).await?;
                if c.identity() != r.repo {
                    return Err(ForgeError::bad("watched repository has changed"));
                }
                observe::poll(&c, r.number).await
            }
            .await;
            let _ = app.update(|db| {
                let allowed = permitted(db, &r);
                let Some(w) = db
                    .forge
                    .watches
                    .get_mut(&id)
                    .filter(|w| w.active && w.pending.is_none() && w.next_ms == r.next_ms)
                else {
                    return Ok(());
                };
                w.next_ms = now() + 30_000;
                if !allowed {
                    w.active = false;
                    return Ok(());
                }
                let mut current = match observed {
                    Ok(v) => v,
                    Err(e) => {
                        w.error = Some(e.detail);
                        return Ok(());
                    }
                };
                w.error = None;
                let mut changes = current.changes(&w.last);
                if current.head != w.last.head {
                    w.awaiting_since = Some(now());
                }
                if current.checks.as_ref().is_some_and(|s| !s.is_empty()) {
                    w.awaiting_since = None;
                } else if current.checks.is_some()
                    && w.repo.starts_with("github://")
                    && w.awaiting_since
                        .is_some_and(|t| now().saturating_sub(t) >= 900_000)
                {
                    changes.push("no checks reported for this head after 15 minutes".into());
                    w.awaiting_since = None;
                }
                if current.head == w.last.head && current.checks.is_none() {
                    current.checks = w.last.checks.take();
                }
                if current.remarks.is_none() {
                    current.remarks = w.last.remarks.take();
                }
                if !changes.is_empty() {
                    if changes.iter().all(|c| c.starts_with("new review activity")) {
                        w.comments_only = w.comments_only.saturating_add(1);
                    } else {
                        w.comments_only = 0;
                    }
                    if w.comments_only <= 10 {
                        let summary = changes.into_iter().take(12).collect::<Vec<_>>().join("\n");
                        w.pending = Some(Notice {
                            id: Uuid::new_v4(),
                            text: format!(
                                "Pull request #{} in {} changed. Review before taking action.\n{}",
                                w.number, w.repo, summary
                            ),
                        });
                    }
                }
                if matches!(current.state.as_str(), "merged" | "closed" | "declined") {
                    w.active = false;
                }
                w.last = current;
                Ok(())
            });
            app.wake_queue();
        });
    }
}
