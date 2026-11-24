//! Coalesced invalidation epochs. Client reports remain separate from authoritative hosted reads.
use super::*;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use std::{collections::VecDeque, convert::Infallible, sync::Arc};
use tokio::sync::watch;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
pub(in crate::workbench) struct Epochs {
    pulls: u64,
    links: u64,
    states: u64,
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Activity {
    epochs: Epochs,
    #[serde(default)]
    reports: VecDeque<Observation>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Observation {
    key: String,
    state: String,
    draft: Option<bool>,
    ms: i64,
}
#[derive(Clone, Copy)]
enum Channel {
    Pulls,
    Links,
    States,
}
impl Epochs {
    fn read(self, channel: Channel) -> u64 {
        match channel {
            Channel::Pulls => self.pulls,
            Channel::Links => self.links,
            Channel::States => self.states,
        }
    }
}
impl ForgeState {
    pub(in crate::workbench) fn epochs(&self) -> Epochs {
        self.activity.epochs
    }
    pub(in crate::workbench) fn pull_changed(&mut self) -> crate::Result<()> {
        bump(&mut self.activity.epochs.pulls)
    }
    pub(in crate::workbench) fn links_changed(&mut self) -> crate::Result<()> {
        bump(&mut self.activity.epochs.links)
    }
    pub(in crate::workbench) fn linked_chat_changed(&mut self, chat: Uuid) -> crate::Result<()> {
        if self.bindings.values().any(|v| v.chat == chat) {
            self.links_changed()?;
        }
        Ok(())
    }
}
fn bump(n: &mut u64) -> crate::Result<()> {
    *n = n
        .checked_add(1)
        .ok_or_else(|| Error::Protocol("forge refresh sequence exhausted".into()))?;
    Ok(())
}
// Journal callbacks can run under the chat lock. A single worker owns the catalog writes.
pub(in crate::workbench) fn checkpoint_worker(
    app: &Workbench,
    rx: std::sync::mpsc::Receiver<Uuid>,
) -> crate::Result<()> {
    let weak = Arc::downgrade(&app.0);
    std::thread::Builder::new()
        .name("kar-forge-refresh".into())
        .spawn(move || {
            while let Ok(chat) = rx.recv() {
                let Some(inner) = weak.upgrade() else { break };
                let mut chats = BTreeSet::from([chat]);
                chats.extend(rx.try_iter());
                if let Err(e) = Workbench(inner).update(|db| {
                    db.forge.pull_changed()?;
                    if db.forge.bindings.values().any(|b| chats.contains(&b.chat)) {
                        db.forge.links_changed()?;
                    }
                    Ok(())
                }) {
                    eprintln!("Kar could not publish a pull refresh: {e}");
                }
            }
        })?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    project_id: Uuid,
    number: u64,
    repository: Option<String>,
    host: Option<String>,
}
impl Reference {
    async fn resolve(self, app: &Workbench) -> Result<Resolved, ForgeError> {
        if self.number == 0 {
            return Err(ForgeError::bad("pull number must be positive"));
        }
        let c = Context::open(app.clone(), self.project_id).await?;
        if self.host.is_some_and(|h| !h.eq_ignore_ascii_case(&c.host))
            || self
                .repository
                .is_some_and(|r| !r.eq_ignore_ascii_case(&c.repository))
        {
            return Err(ForgeError::bad(
                "pull reference disagrees with the project repository",
            ));
        }
        Ok(Resolved {
            key: review::key(self.project_id, &c, self.number),
            project: self.project_id,
            ctx: c,
            number: self.number,
        })
    }
}
struct Resolved {
    key: String,
    project: Uuid,
    ctx: Context,
    number: u64,
}
impl Resolved {
    fn sync_linked(self, app: Workbench) {
        let repo = self.ctx.identity();
        let linked = app
            .0
            .catalog
            .lock()
            .unwrap()
            .forge
            .bindings
            .values()
            .any(|b| {
                b.kind == "pull"
                    && b.number == self.number
                    && b.repository.eq_ignore_ascii_case(&repo)
            });
        if linked {
            tokio::spawn(async move {
                if let Err(e) = review::refresh(&app, self.project, &self.ctx, self.number).await {
                    eprintln!(
                        "Kar could not refresh linked pull {}: {}",
                        self.number, e.kind
                    );
                }
            });
        }
    }
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Invalidation {
    reference: Option<Reference>,
    #[serde(default)]
    files_viewed_only: bool,
}
async fn invalidate(State(app): State<Workbench>, Json(input): Json<Invalidation>) -> Reply<Value> {
    let key = match input.reference {
        Some(r) => Some(r.resolve(&app).await?),
        None => None,
    };
    let result = app.update(|db| {
        if !input.files_viewed_only {
            for (id, r) in &mut db.forge.reviews {
                if key.as_ref().is_none_or(|s| &s.key == id) {
                    r.applied = r.requested;
                }
            }
            db.forge.pull_changed()?;
        }
        // Viewed marks are durable per head; every read resolves the current head.
        Ok(json!({"revision":db.forge.epochs().pulls}))
    })?;
    if !input.files_viewed_only
        && let Some(reference) = key
    {
        reference.sync_linked(app);
    }
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Report {
    reference: Reference,
    state: String,
    is_draft: Option<bool>,
}
async fn report(State(app): State<Workbench>, Json(input): Json<Report>) -> Reply<Value> {
    if !matches!(input.state.as_str(), "open" | "merged" | "closed") {
        return Err(ForgeError::bad("unknown pull state"));
    }
    let key = input.reference.resolve(&app).await?;
    let ms = jiff::Timestamp::now().as_millisecond();
    let result = app.update(|db| {
        let changes = &mut db.forge.activity;
        let existing = changes.reports.iter().position(|r| r.key == key.key);
        if existing
            .and_then(|i| changes.reports.get(i))
            .is_some_and(|r| {
                r.state == input.state
                    && r.draft == input.is_draft
                    && (0..300_000).contains(&ms.saturating_sub(r.ms))
            })
        {
            return Ok(json!({"revision":changes.epochs.states,"duplicate":true}));
        }
        if let Some(i) = existing {
            changes.reports.remove(i);
        }
        if changes.reports.len() >= 2048 {
            changes.reports.pop_front();
        }
        changes.reports.push_back(Observation {
            key: key.key.clone(),
            state: input.state,
            draft: input.is_draft,
            ms,
        });
        bump(&mut changes.epochs.states)?;
        Ok(json!({"revision":changes.epochs.states,"duplicate":false}))
    })?;
    if result["duplicate"] == false {
        key.sync_linked(app);
    }
    Ok(Json(result))
}

#[derive(Deserialize, Default)]
struct Cursor {
    after: Option<u64>,
}
async fn subscribe(
    State(app): State<Workbench>,
    Path(name): Path<String>,
    Query(q): Query<Cursor>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ForgeError> {
    let channel = match name.as_str() {
        "pulls" => Channel::Pulls,
        "linked-chats" => Channel::Links,
        "reported-states" => Channel::States,
        _ => return Err(ForgeError::remote(404, "unknown forge refresh stream")),
    };
    let after = match headers.get("last-event-id") {
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ForgeError::bad("invalid forge refresh cursor"))?,
        None => q.after.unwrap_or(0),
    };
    let rx = app.0.forge_updates.subscribe();
    if after > rx.borrow().read(channel) {
        return Err(ForgeError::bad(
            "forge refresh cursor is ahead of workbench state",
        ));
    }
    let events = futures_util::stream::unfold((rx, after), move |(mut rx, seen)| async move {
        loop {
            let current = rx.borrow_and_update().read(channel);
            if current > seen {
                let event = Event::default()
                    .event("refresh")
                    .id(current.to_string())
                    .data(current.to_string());
                return Some((Ok::<_, Infallible>(event), (rx, current)));
            }
            rx.changed().await.ok()?;
        }
    });
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}
pub(in crate::workbench) fn notify(watch: &watch::Sender<Epochs>, epochs: Epochs) {
    watch.send_if_modified(|curr| {
        if *curr == epochs {
            false
        } else {
            *curr = epochs;
            true
        }
    });
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/forge/pulls/invalidate", post(invalidate))
        .route("/forge/pulls/report-state", post(report))
        .route("/forge/refreshes/{name}", get(subscribe))
}
