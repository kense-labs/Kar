//! A launch matrix (workspace, provider and initial input) has one durable run identity.
use super::*;
use serde_json::{Value, json};
mod http;
mod worker;
pub(super) use http::routes;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Workspace {
    #[default]
    Root,
    Existing {
        directory: PathBuf,
    },
    Worktree {
        directory: PathBuf,
        branch: String,
        #[serde(default = "head")]
        base: String,
        #[serde(default)]
        from_origin: bool,
    },
}
fn head() -> String {
    "HEAD".into()
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Launch {
    pub runtime: String,
    pub model: Option<String>,
    #[serde(default)]
    pub approvals: crate::ApprovalMode,
    #[serde(default)]
    pub settings: crate::SessionSettings,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    pub launch: Option<Launch>,
    pub title: Option<String>,
    #[serde(default)]
    pub workspace: Workspace,
    pub message: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Uuid>,
    pub requested_output_schema: Option<Value>,
    #[serde(default)]
    pub defer_release: bool,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Caller {
    pub project: Uuid,
    pub chat: Uuid,
    pub access: mcp::project_tools::Access,
}
#[derive(Clone, Serialize, Deserialize)]
struct Script {
    terminal: Uuid,
    command: String,
    asynchronous: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Worktree,
    Setup,
    Ready,
    Releasing,
    Released,
    Failed,
}
#[derive(Clone, Serialize, Deserialize)]
struct Event {
    sequence: u64,
    attempt: u64,
    stage: Stage,
    detail: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Job {
    project: Uuid,
    chat: Uuid,
    run: Uuid,
    caller: Option<Caller>,
    input: Input,
    attempt: u64,
    stage: Stage,
    events: Vec<Event>,
    cwd: Option<PathBuf>,
    script: Option<Script>,
    commands: BTreeMap<Uuid, String>,
    result: Option<Value>,
}
impl Job {
    fn advance(&mut self, stage: Stage, detail: Option<String>) {
        self.stage = stage;
        self.events.push(Event {
            sequence: self.events.len() as u64 + 1,
            attempt: self.attempt,
            stage,
            detail,
        });
    }
    fn view(&self) -> Value {
        json!({"project_id":self.project,"thread_id":self.chat,"run_id":self.run,"attempt":self.attempt,"stage":self.stage,"sequence":self.events.len(),"event":self.events.last(),"workspace":self.cwd,"setup_terminal":self.script.as_ref().map(|s|s.terminal),"result":self.result})
    }
}
// A new chat cannot be claimed through an unrelated create or fork while its workspace is being prepared.
pub(super) fn reservation(db: &Catalog, chat: Uuid) -> Option<Uuid> {
    db.prepared_runs
        .values()
        .find(|j| j.chat == chat && j.input.launch.is_some() && j.stage != Stage::Released)
        .map(|j| j.run)
}
fn get(app: &Workbench, p: Uuid, c: Uuid, r: Uuid) -> Result<Job> {
    app.project(p)?;
    app.0
        .catalog
        .lock()
        .unwrap()
        .prepared_runs
        .get(&r)
        .filter(|j| j.project == p && j.chat == c)
        .cloned()
        .ok_or_else(missing)
}
fn change<T>(app: &Workbench, run: Uuid, f: impl FnOnce(&mut Job) -> Result<T>) -> Result<T> {
    let result = app.update(|db| f(db.prepared_runs.get_mut(&run).ok_or_else(missing)?))?;
    app.0.run_events.send_modify(|n| *n = n.wrapping_add(1));
    Ok(result)
}
fn authorize(db: &Catalog, job: &Job) -> Result<()> {
    if db
        .projects
        .get(&job.project)
        .is_none_or(|p| p.metadata.deleted)
        || db.family.deleted.contains_key(&job.chat)
        || db.archived.contains(&job.chat)
    {
        return Err(missing());
    }
    if let Some(c) = &job.caller
        && !c.access.authorizes_launch(db, c.project, c.chat)
    {
        return Err(denied());
    }
    Ok(())
}
fn submit(
    app: &Workbench,
    p: Uuid,
    c: Uuid,
    r: Uuid,
    input: Input,
    caller: Option<Caller>,
) -> Result<Job> {
    let project = app.project(p)?;
    app.workspace(&project)?;
    if input
        .title
        .as_ref()
        .is_some_and(|s| s.is_empty() || s.chars().count() > 200 || s.contains(['\n', '\r', '\0']))
        || (input.requested_output_schema.is_some() && input.message.is_none())
    {
        return Err(Error::Refused(
            "invalid launch title or requested schema without a message".into(),
        ));
    }
    if input.launch.is_none() {
        app.binding(p, c)?;
        if !matches!(input.workspace, Workspace::Root) {
            return Err(Error::Refused(
                "an existing chat retains its bound workspace".into(),
            ));
        }
    }
    let src = caller.as_ref().map_or(c, |m| m.chat);
    for id in &input.attachments {
        app.0.chats.visible_attachment(src, *id)?;
    }
    let job = app.update(|db| {
        if let Some(j) = db.prepared_runs.get(&r) {
            if j.project != p || j.chat != c || j.caller != caller || j.input != input {
                return Err(Error::Refused(
                    "run ID belongs to a different launch".into(),
                ));
            }
            authorize(db, j)?;
            return Ok(j.clone());
        }
        let mut legacy_workspace = None;
        if input.launch.is_some() {
            if reservation(db, c).is_some()
                || mcp::pending_launch(db, c) != caller.as_ref().map(|x| x.chat)
            {
                return Err(Error::Busy);
            }
            match app.0.chats.initial_info(c) {
                // A launch recorded by an older Kar may already have created its native
                // chat before persisting the model command receipt. Finish its stable input.
                Ok(info) if caller.is_some() && info.forked_from.is_none() => {
                    legacy_workspace = Some(info.workspace);
                }
                Ok(_) => return Err(Error::Refused("chat already exists".into())),
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let mut j = Job {
            project: p,
            chat: c,
            run: r,
            caller,
            input,
            attempt: 1,
            stage: Stage::Worktree,
            events: vec![],
            cwd: None,
            script: None,
            commands: BTreeMap::new(),
            result: None,
        };
        authorize(db, &j)?;
        if let Some(workspace) = legacy_workspace {
            j.cwd = Some(workspace);
            j.advance(Stage::Releasing, Some("resume existing launch".into()));
        } else {
            j.advance(Stage::Worktree, None);
        }
        db.prepared_runs.insert(r, j.clone());
        Ok(j)
    })?;
    worker::schedule(app, &job);
    Ok(job)
}
pub(super) async fn model_launch(
    app: &Workbench,
    p: Uuid,
    c: Uuid,
    r: Uuid,
    input: Input,
    caller: Caller,
) -> Result<Value> {
    let mut rx = app.0.run_events.subscribe();
    submit(app, p, c, r, input, Some(caller))?;
    loop {
        let j = get(app, p, c, r)?;
        match j.stage {
            Stage::Released => {
                return j
                    .result
                    .ok_or_else(|| Error::Protocol("missing launch result".into()));
            }
            Stage::Ready if j.input.defer_release => return Ok(j.view()),
            Stage::Failed => {
                return Err(Error::Refused(
                    j.events
                        .last()
                        .and_then(|e| e.detail.clone())
                        .unwrap_or_else(|| "launch preparation failed".into()),
                ));
            }
            _ => rx
                .changed()
                .await
                .map_err(|_| Error::Closed("preparation stopped".into()))?,
        }
    }
}
pub(super) fn recover(app: &Workbench) {
    let jobs: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .prepared_runs
        .values()
        .filter(|j| {
            matches!(j.stage, Stage::Worktree | Stage::Setup | Stage::Releasing)
                || (j.stage == Stage::Ready && !j.input.defer_release)
        })
        .cloned()
        .collect();
    for job in jobs {
        worker::schedule(app, &job);
    }
}
