//! Named, retryable Git operations with replayable progress across disconnected clients.
use super::{
    Workbench, forge,
    http::{ApiError, blocking},
    missing,
};
use crate::{Error, Result, SessionOptions, workbench_git::StackCheckout};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Commit,
    Push,
    CreatePr,
    CommitPush,
    CommitPushPr,
}
impl Action {
    fn commit(self) -> bool {
        matches!(self, Self::Commit | Self::CommitPush | Self::CommitPushPr)
    }
    fn pr(self) -> bool {
        matches!(self, Self::CreatePr | Self::CommitPushPr)
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Input {
    id: Uuid,
    action: Action,
    chat: Option<Uuid>,
    commit_message: Option<String>,
    #[serde(default)]
    feature_branch: bool,
    branch_name: Option<String>,
    files: Option<Vec<PathBuf>>,
    base: Option<String>,
    title: Option<String>,
    #[serde(default)]
    body: String,
    writer_runtime: Option<String>,
    writer_model: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    project: Uuid,
    request: Input,
    events: Vec<Value>,
    terminal: bool,
}
pub(super) fn recover(app: &Workbench) -> Result<()> {
    if app
        .0
        .catalog
        .lock()
        .unwrap()
        .git_actions
        .values()
        .all(|r| r.terminal)
    {
        return Ok(());
    }
    app.update(|c|{
        for r in c.git_actions.values_mut().filter(|r|!r.terminal) {
            r.events.push(json!({"kind":"action_failed","reason":"host_restarted","message":"Host restarted during a Git operation. Inspect the commit, remote and PR before starting another operation."}));
            r.terminal=true;
        }
        Ok(())
    })
}
struct Operation {
    app: Workbench,
    project: Uuid,
    input: Input,
}
impl Operation {
    fn record(&self, event: Value, terminal: bool) -> Result<()> {
        self.app.update(|c| {
            let r = c.git_actions.get_mut(&self.input.id).ok_or_else(missing)?;
            if r.terminal {
                return Err(Error::Refused("Git operation already ended".into()));
            }
            r.events.push(event);
            r.terminal = terminal;
            Ok(())
        })
    }
    fn phase(&self, phase: &str) -> Result<()> {
        self.record(json!({"kind":"phase_started","phase":phase}), false)
    }
    async fn message(&self, snapshot: &StackCheckout, files: &[PathBuf]) -> Result<String> {
        if let Some(message) = &self.input.commit_message {
            return Ok(message.clone());
        }
        let context = snapshot.summary(files)?;
        let rt = crate::runtime(self.input.writer_runtime.as_deref().unwrap_or("codex"))?;
        // The writer receives the diff as input. It has no task files to modify.
        let cwd = tempfile::tempdir()?;
        let mut opts = SessionOptions::new(cwd.path());
        opts.model = self.input.writer_model.clone();
        opts.unset_env.insert("KAR_WORKBENCH_TOKEN".into());
        let session = rt.open(opts).await?;
        let result=async {
            let report=session.prompt(format!("Write a Git commit message for this change. Return just a concise subject and optional body.\n\n{context}")).await?.report().await?;
            let text=report.text.trim();
            if report.outcome!=crate::Outcome::Completed || text.is_empty() || text.len()>10000 {
                return Err(Error::Refused("native writer returned no usable commit message".into()));
            }
            Ok(text.to_owned())
        }.await;
        let closed = session.close().await;
        let text = result?;
        closed?;
        Ok(text)
    }
    async fn run(self: &Arc<Self>) -> Result<Value> {
        let app = self.app.clone();
        let project = self.project;
        let chat = self.input.chat;
        let checkout = Arc::new(
            tokio::task::spawn_blocking(move || {
                let repo = match chat {
                    Some(chat_id) => {
                        let info = app.binding(project, chat_id)?;
                        if app.archived(chat_id) {
                            return Err(Error::Refused("chat is archived".into()));
                        }
                        crate::workbench_git::Repository::open(&info.workspace)?
                    }
                    None => app.repository(project)?,
                };
                StackCheckout::acquire(repo)
            })
            .await
            .map_err(|e| Error::Closed(e.to_string()))??,
        );
        let status = checkout.repo.status()?;
        let input = &self.input;
        let planned = input.action;
        // A pull upstream may be current while the separately configured fork is stale.
        let wants_push = matches!(
            planned,
            Action::Push | Action::CommitPush | Action::CommitPushPr | Action::CreatePr
        );
        let phases: Vec<_> = [
            (input.feature_branch, "branch"),
            (planned.commit(), "commit"),
            (wants_push, "push"),
            (planned.pr(), "pr"),
        ]
        .into_iter()
        .filter_map(|(on, label)| on.then_some(label))
        .collect();
        self.record(json!({"kind":"action_started","phases":phases}), false)?;
        if !input.feature_branch && (wants_push || planned.pr()) && status.branch.is_none() {
            return Err(Error::Refused("cannot publish a detached HEAD".into()));
        }
        let skipped = || json!({"status":"skipped_not_requested"});
        let mut result = json!({"action":planned,"branch":skipped(),"commit":skipped(),"push":skipped(),"pr":skipped()});
        let mut branch = status.branch;
        let mut message = None;
        if planned.commit() {
            let files = checkout.selected(input.files.as_deref())?;
            if input.feature_branch {
                self.phase("branch")?;
                if files.is_empty() && input.branch_name.is_none() {
                    return Err(Error::Refused("no changes for a feature branch".into()));
                }
            }
            if !files.is_empty() {
                message = Some(self.message(&checkout, &files).await?);
            }
            if input.feature_branch {
                let name = match &input.branch_name {
                    Some(name) => name.clone(),
                    None => generated_branch(message.as_deref().unwrap_or_default(), input.id),
                };
                branch = Some(name);
            }
        }
        let target = if wants_push {
            let name = branch
                .as_deref()
                .ok_or_else(|| Error::Refused("no branch to publish".into()))?;
            Some(checkout.repo.publish_target(name)?)
        } else {
            None
        };
        let pull = if planned.pr() {
            let target = target.as_ref().expect("PR includes push");
            Some(
                forge::PullTarget::resolve(
                    self.app.clone(),
                    project,
                    &checkout.repo,
                    branch.as_deref().unwrap(),
                    target,
                    input.base.as_deref(),
                )
                .await
                .map_err(|e| Error::Refused(e.detail))?,
            )
        } else {
            None
        };
        if planned.commit() {
            if input.feature_branch {
                let name = branch.as_deref().expect("feature branch was selected");
                checkout.branch(name)?;
                result["branch"] = json!({"status":"created","name":name});
            }
            self.phase("commit")?;
            let commit = if let Some(message) = message.clone() {
                let check = checkout.clone();
                let op = self.clone();
                let paths = input.files.clone();
                tokio::task::spawn_blocking(move || {
                    check.commit(&message, paths.as_deref(), |event| {
                        op.record(json!(event), false)
                    })
                })
                .await
                .map_err(|e| Error::Closed(e.to_string()))??
            } else {
                None
            };
            result["commit"] = match commit {
                Some(sha) => json!({"status":"created","commit":sha}),
                None => json!({"status":"skipped_no_changes"}),
            };
        }
        if let Some(target) = target {
            self.phase("push")?;
            let check = checkout.clone();
            let local = branch.clone().expect("publish requires a branch");
            let (state, set_upstream, target) =
                tokio::task::spawn_blocking(move || -> Result<_> {
                    let (state, set_upstream) = check.push(&local, &target)?;
                    Ok((state, set_upstream, target))
                })
                .await
                .map_err(|e| Error::Closed(e.to_string()))??;
            self.app.update(|db| db.forge.pull_changed())?;
            result["push"] = json!({"status":state,"remote":target.remote,"branch":branch,
                "upstream_branch":format!("{}/{}",target.remote,target.branch),"published_branch":target.branch,"set_upstream":set_upstream});
        }
        if let Some(pull) = pull {
            self.phase("pr")?;
            let head = pull.head.clone();
            let title = input
                .title
                .clone()
                .or_else(|| {
                    message
                        .as_deref()
                        .and_then(|s| s.lines().next())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| head.clone());
            result["pr"] = forge::stacked_pull(
                self.app.clone(),
                project,
                input.id,
                input.chat,
                pull,
                title,
                input.body.clone(),
            )
            .await
            .map_err(|e| Error::Refused(format!("{}: {}", e.kind, e.detail)))?;
        }
        Ok(result)
    }
}
fn generated_branch(text: &str, id: Uuid) -> String {
    let mut words = Vec::new();
    for word in text
        .lines()
        .next()
        .unwrap_or_default()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(8)
    {
        words.push(word.to_ascii_lowercase());
    }
    let suffix = &id.simple().to_string()[..8];
    if words.is_empty() {
        format!("kar/{suffix}")
    } else {
        format!("kar/{}-{suffix}", words.join("-"))
    }
}
fn record(app: &Workbench, project: Uuid, id: Uuid) -> Result<Record> {
    app.project(project)?;
    app.0
        .catalog
        .lock()
        .unwrap()
        .git_actions
        .get(&id)
        .filter(|r| r.project == project)
        .cloned()
        .ok_or_else(missing)
}
async fn start(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(input): Json<Input>,
) -> std::result::Result<Json<Value>, ApiError> {
    app.project(project)?;
    if let Some(chat) = input.chat {
        app.binding(project, chat)?;
        if app.archived(chat) {
            return Err(Error::Refused("chat is archived".into()).into());
        }
    }
    if input.branch_name.is_some() && !input.feature_branch
        || input.feature_branch && !input.action.commit()
        || input.files.as_ref().is_some_and(|files| files.is_empty())
        || input
            .commit_message
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 10000)
        || input.base.as_deref() == Some("")
        || input.title.as_deref().is_some_and(|s| s.trim().is_empty())
    {
        return Err(Error::Refused(
            "invalid branch, file selection or message for this action".into(),
        )
        .into());
    }
    let id = input.id;
    let created = app.update(|c| {
        if let Some(old) = c.git_actions.get(&id) {
            if old.project != project || old.request != input {
                return Err(Error::Refused(
                    "Git action ID already belongs to a different request".into(),
                ));
            }
            return Ok(false);
        }
        c.git_actions.insert(
            id,
            Record {
                project,
                request: input.clone(),
                events: Vec::new(),
                terminal: false,
            },
        );
        Ok(true)
    })?;
    if created {
        let op = Arc::new(Operation {
            app: app.clone(),
            input,
            project,
        });
        tokio::spawn(async move {
            let outcome = op.run().await;
            let event = match outcome {
                Ok(result) => json!({"kind":"action_finished","result":result}),
                Err(e) => json!({"kind":"action_failed","message":e.to_string()}),
            };
            let _ = op.record(event, true);
        });
    }
    Ok(Json(json!({"id":id})))
}
async fn snapshot(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> std::result::Result<Json<Record>, ApiError> {
    blocking(move || record(&app, p, id)).await
}
#[derive(Deserialize, Default)]
struct After {
    #[serde(default)]
    after: usize,
}
async fn events(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Query(q): Query<After>,
) -> std::result::Result<Response, ApiError> {
    if q.after > record(&app, p, id)?.events.len() {
        return Err(Error::InvalidCursor.into());
    }
    let stream =
        futures_util::stream::unfold((app, q.after), move |(app, mut offset)| async move {
            loop {
                let row = record(&app, p, id).ok()?;
                if let Some(event) = row.events.get(offset) {
                    offset += 1;
                    let event = Event::default()
                        .id(offset.to_string())
                        .event("git_action")
                        .json_data(json!({"action_id":id,"cursor":offset,"event":event}))
                        .ok()?;
                    return Some((Ok::<_, Infallible>(event), (app, offset)));
                }
                if row.terminal {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/git/actions", post(start))
        .route("/projects/{project}/git/actions/{id}", get(snapshot))
        .route("/projects/{project}/git/actions/{id}/events", get(events))
}
