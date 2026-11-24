use super::*;
use crate::workbench_git::ManagedWorktree;
use axum::{
    Router,
    extract::{Path, Query, State},
    routing::{get, post, put},
};

async fn drivers() -> Json<Value> {
    Json(json!(DRIVER_KINDS.iter().map(|kind|json!({
        "kind":kind,"upstream_pull_actions":declared_pull_actions(kind),"issues_in_upstream":supports_issues(kind),
        "reviews":actions::capabilities(kind),"pulls_in_upstream":*kind!="linear","enabled":*kind!="tanstack-source" || std::env::var("KAR_TANSTACK_SOURCE").as_deref()==Ok("1")
    })).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
struct Input {
    id: Uuid,
    chat: Uuid,
    #[serde(default)]
    routing: Option<routing::Expected>,
    issue_scope: Option<String>,
    #[serde(flatten)]
    operation: Operation,
}
#[derive(Deserialize)]
struct ReadInput {
    #[serde(default)]
    routing: Option<routing::Expected>,
    issue_scope: Option<String>,
    #[serde(flatten)]
    operation: Operation,
}
async fn read(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(input): Json<ReadInput>,
) -> Reply<Value> {
    let op = input.operation;
    if op.mutates() {
        return Err(ForgeError::bad(
            "use /operations and a stable request ID for writes",
        ));
    }
    providers::issue::validate(&op)?;
    actions::validate(&op)?;
    let c = Context::for_operation(app.clone(), project, &op, input.issue_scope.as_deref()).await?;
    routing::verify(&c, input.routing.as_ref()).await?;
    if let Operation::Pull { number } = op {
        return review::refresh(&app, project, &c, number).await.map(Json);
    }
    c.execute(&op).await.map(Json)
}
fn complete(app: &Workbench, id: Uuid, result: Result<Value, ForgeError>) -> Reply<Value> {
    Ok(Json(app.update(|db| {
        let m = db
            .forge
            .operations
            .get(&id)
            .ok_or_else(|| Error::Refused("unknown operation".into()))?;
        if m.result.as_ref().is_some_and(Result::is_ok) {
            return Ok(m.result.clone().unwrap());
        }
        if let Ok(v) = &result {
            let (number, kind) = match m.operation {
                Operation::CreatePull { .. } => (v["number"].as_u64(), "pull"),
                Operation::SubmitReview { number, .. }
                | Operation::ReplyToThread { number, .. }
                | Operation::SetThreadResolution { number, .. } => (Some(number), "pull"),
                Operation::CreateIssue { .. } => (v["number"].as_u64(), "issue"),
                Operation::SetIssueReaction { number, .. }
                | Operation::UpdateIssue { number, .. }
                | Operation::SetIssueState { number, .. }
                | Operation::CommentIssue { number, .. } => (Some(number), "issue"),
                _ => unreachable!(),
            };
            let number = number.ok_or_else(|| {
                Error::Refused("provider did not identify the new change request".into())
            })?;
            let binding = Binding {
                project: m.project,
                chat: m.chat,
                repository: m.repository.clone(),
                kind: kind.into(),
                number,
            };
            if !db.forge.bindings.values().any(|v| v == &binding) {
                db.forge.links_changed()?;
            }
            if kind == "pull" {
                db.forge.pull_changed()?;
            }
            db.forge
                .link_activity
                .insert(binding.chat, jiff::Timestamp::now().as_millisecond() as u64);
            db.forge.bindings.insert(id, binding);
        }
        db.forge.operations.get_mut(&id).unwrap().result = Some(result.clone());
        Ok(result)
    })??))
}
async fn execute(app: Workbench, project: Uuid, input: Input) -> Reply<Value> {
    execute_checked(app, project, input, |_, _| Ok(())).await
}

pub(in crate::workbench) async fn model_issue_write(
    app: Workbench,
    project: Uuid,
    chat: Uuid,
    id: Uuid,
    issue_scope: Option<String>,
    operation: Operation,
    authorize: impl FnOnce(&crate::workbench::Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    if !matches!(
        operation,
        Operation::CreateIssue { .. } | Operation::CommentIssue { .. } | Operation::SetIssueState { .. }
    ) {
        return Err(ForgeError::bad("unsupported model issue operation"));
    }
    model_forge_write(app,project,chat,id,issue_scope,operation,authorize).await
}
pub(super) async fn model_forge_write(
    app:Workbench,project:Uuid,chat:Uuid,id:Uuid,issue_scope:Option<String>,operation:Operation,
    authorize:impl FnOnce(&crate::workbench::Catalog,&str)->crate::Result<()>
)->Result<Value,ForgeError>{
    let input = Input {id,chat,issue_scope,operation,routing:None};
    Ok(execute_checked(app, project, input, authorize).await?.0)
}

async fn execute_checked(
    app: Workbench,
    project: Uuid,
    input: Input,
    authorize: impl FnOnce(&crate::workbench::Catalog, &str) -> crate::Result<()>,
) -> Reply<Value> {
    app.binding(project, input.chat)?;
    if !input.operation.mutates() {
        return Err(ForgeError::bad("use /query for reads"));
    }
    providers::issue::validate(&input.operation)?;
    actions::validate(&input.operation)?;
    let mut c = Context::for_operation(
        app.clone(),
        project,
        &input.operation,
        input.issue_scope.as_deref(),
    )
    .await?;
    routing::verify(&c, input.routing.as_ref()).await?;
    let identity = c.identity();
    let id = input.id;
    let remembered = app.update(|db| {
        authorize(db, &identity)?;
        if db.forge.model_link_receipts.contains_key(&id) || db.forge.bindings.contains_key(&id) && !db.forge.operations.contains_key(&id) {
            return Err(Error::Refused(
                "operation ID already belongs to a binding".into(),
            ));
        }
        if let Some(old) = db.forge.operations.get(&id) {
            if old.project != project
                || old.repository != identity
                || old.chat != input.chat
                || old.issue_scope != input.issue_scope
                || old.routing != input.routing
                || old.operation != input.operation
            {
                return Err(Error::Refused(
                    "operation ID already belongs to another request".into(),
                ));
            }
            return Ok(Some(old.result.clone()));
        }
        db.forge.operations.insert(
            id,
            Mutation {
                project,
                chat: input.chat,
                repository: identity,
                operation: input.operation.clone(),
                issue_scope: input.issue_scope.clone(),
                routing: input.routing.clone(),
                result: None,
                reaction_viewer: None,
            },
        );
        Ok(None)
    })?;
    match remembered {
        Some(Some(r)) => return r.map(Json),
        Some(None) => {
            return Err(ForgeError::uncertain(
                "operation has already been claimed; reconcile its remote result",
            ));
        }
        None => {}
    }
    let mut op = input.operation;
    if let Operation::CreatePull { body, .. } = &mut op {
        body.push_str(&format!("\n\n<!-- kar-operation:{id} -->"))
    }
    if matches!(op, Operation::SetIssueReaction { .. }) {
        let viewer = match providers::reactions::viewer(&mut c).await {
            Ok(id) => id,
            Err(e) => return complete(&app, id, Err(e)),
        };
        app.update(|db| {
            let m = db
                .forge
                .operations
                .get_mut(&id)
                .ok_or_else(|| Error::Refused("unknown operation".into()))?;
            m.reaction_viewer = Some(viewer);
            Ok(())
        })?;
    }
    let result = c.execute(&op).await;
    complete(&app, id, result)
}
pub(in crate::workbench) async fn stacked_pull(
    app: Workbench,
    project: Uuid,
    id: Uuid,
    chat: Option<Uuid>,
    target: PullTarget,
    title: String,
    body: String,
) -> Result<Value, ForgeError> {
    let PullTarget {
        head,
        base,
        repository,
        context: c,
    } = target;
    let expected = repository.as_deref().unwrap_or(&c.repository);
    let pulls = c.execute(&Operation::Pulls).await?;
    for r in pulls
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["head_ref"] == head && r["base_ref"] == base && r["state"] == "open")
    {
        if publish::matches_repository(&c, expected, r).await? {
            return Ok(json!({"status":"opened_existing","pull":r}));
        }
    }
    let mut operation = Operation::CreatePull {
        head,
        head_repository: repository,
        base,
        title,
        body,
    };
    let pull = if let Some(chat) = chat {
        execute(
            app,
            project,
            Input {
                id,
                chat,
                routing: None,
                issue_scope: None,
                operation,
            },
        )
        .await?
        .0
    } else {
        if let Operation::CreatePull { body, .. } = &mut operation {
            body.push_str(&format!("\n\n<!-- kar-operation:{id} -->"));
        }
        c.execute(&operation).await?
    };
    Ok(json!({"status":"created","pull":pull}))
}
async fn mutate(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(input): Json<Input>,
) -> Reply<Value> {
    tokio::spawn(execute(app, project, input))
        .await
        .map_err(|e| ForgeError::uncertain(e.to_string()))?
}
async fn operation(
    State(app): State<Workbench>,
    Path((project, id)): Path<(Uuid, Uuid)>,
) -> Reply<Mutation> {
    app.project(project)?;
    let db = app.0.catalog.lock().unwrap();
    Ok(Json(
        db.forge
            .operations
            .get(&id)
            .filter(|m| m.project == project)
            .cloned()
            .ok_or_else(|| ForgeError::remote(404, "unknown operation"))?,
    ))
}
#[derive(Deserialize)]
struct Reconcile {
    number: u64,
}
async fn reconcile(
    State(app): State<Workbench>,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Json(input): Json<Reconcile>,
) -> Reply<Value> {
    let Json(m) = operation(State(app.clone()), Path((project, id))).await?;
    let mut c =
        Context::for_operation(app.clone(), project, &m.operation, m.issue_scope.as_deref())
            .await?;
    routing::verify(&c, m.routing.as_ref()).await?;
    if c.identity() != m.repository {
        return Err(ForgeError::bad(
            "project origin changed since the operation",
        ));
    }
    match m.result {
        Some(Ok(value)) => return Ok(Json(value)),
        Some(Err(ref e)) if e.kind != "needs_reconciliation" => return Err(e.clone()),
        _ => {}
    }
    let value = match m.operation {
        Operation::CreatePull {
            head,
            head_repository,
            base,
            ..
        } => {
            let v = c
                .execute(&Operation::Pull {
                    number: input.number,
                })
                .await?;
            if let Some(source) = head_repository
                && !publish::matches_repository(&c, &source, &v).await?
            {
                return Err(ForgeError::bad(
                    "PR belongs to a different source repository",
                ));
            }
            if v["head_ref"] != head
                || v["base_ref"] != base
                || !v["body"]
                    .as_str()
                    .is_some_and(|s| s.contains(&format!("<!-- kar-operation:{id} -->")))
            {
                return Err(ForgeError::uncertain(
                    "remote pull request does not attest this operation ID and branch pair",
                ));
            }
            v
        }
        ref op @ Operation::SetIssueReaction { number, .. } if number == input.number => {
            let viewer = providers::reactions::viewer(&mut c).await?;
            if m.reaction_viewer.as_ref() != Some(&viewer) {
                return Err(ForgeError::uncertain(
                    "the current account cannot attest the original reaction",
                ));
            }
            providers::reactions::execute(&c, op, false).await?
        }
        Operation::UpdateIssue { number, ref fields } if number == input.number => {
            providers::issue::update::confirm(&c, number, fields).await?
        }
        Operation::SetIssueState { number, state } if number == input.number => {
            let v = c.execute(&Operation::Issue { number }).await?;
            let confirmed = match c.kind.as_str() {
                "github" => github::state_matches(&v, &state)?,
                "gitlab" | "forgejo" | "tanstack-source" => v["state"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&state)),
                _ => v["state"] == state,
            };
            if !confirmed {
                return Err(ForgeError::uncertain(
                    "issue has not confirmed the requested state",
                ));
            }
            v
        }
        Operation::SubmitReview { .. }
        | Operation::ReplyToThread { .. }
        | Operation::SetThreadResolution { .. } => {
            return Err(ForgeError::uncertain(
                "inspect the remote review before creating any new operation",
            ));
        }
        Operation::CreateIssue { .. } | Operation::CommentIssue { .. } => {
            return Err(ForgeError::uncertain(
                "remote issue writes without an authoritative receipt require manual inspection",
            ));
        }
        _ => return Err(ForgeError::bad("operation and object number disagree")),
    };
    complete(&app, id, Ok(value))
}
#[derive(Deserialize)]
struct Link {
    issue_scope: Option<String>,
    id: Uuid,
    chat: Uuid,
    kind: String,
    number: u64,
}
async fn link(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(input): Json<Link>,
) -> Reply<Binding> {
    app.binding(project, input.chat)?;
    let op = match input.kind.as_str() {
        "pull" => Operation::Pull {
            number: input.number,
        },
        "issue" => Operation::Issue {
            number: input.number,
        },
        _ => return Err(ForgeError::bad("kind must be pull or issue")),
    };
    let c = Context::for_operation(app.clone(), project, &op, input.issue_scope.as_deref()).await?;
    let object = c.execute(&op).await?;
    if checked_number(&object)? != input.number {
        return Err(ForgeError::bad("object identity mismatch"));
    }
    let binding = Binding {
        project,
        chat: input.chat,
        kind: input.kind,
        repository: c.identity(),
        number: input.number,
    };
    save_binding(&app, input.id, binding)
}
fn save_binding(app: &Workbench, id: Uuid, binding: Binding) -> Reply<Binding> {
    Ok(Json(app.update(|db| {
        if let Some(old) = db.forge.bindings.get(&id) {
            if old == &binding {
                return Ok(old.clone());
            }
            return Err(Error::Refused(
                "binding ID already belongs to another request".into(),
            ));
        }
        if db.forge.operations.contains_key(&id) || db.forge.model_link_receipts.contains_key(&id) {
            return Err(Error::Refused(
                "binding ID already belongs to an operation".into(),
            ));
        }
        if !db.forge.bindings.values().any(|b| b == &binding) {
            db.forge.links_changed()?;
            db.forge
                .link_activity
                .insert(binding.chat, jiff::Timestamp::now().as_millisecond() as u64);
        }
        db.forge.bindings.insert(id, binding.clone());
        Ok(binding)
    })?))
}

#[derive(Deserialize)]
struct OpenIssue {
    issue_scope: Option<String>,
    id: Uuid,
    runtime: String,
    model: Option<String>,
}
async fn issue_chat(
    State(app): State<Workbench>,
    Path((project, n)): Path<(Uuid, u64)>,
    Json(input): Json<OpenIssue>,
) -> Reply<Value> {
    let c = providers::linear::tracker_scope(app.clone(), project, input.issue_scope.as_deref())
        .await?;
    let issue = c.execute(&Operation::Issue { number: n }).await?;
    if checked_number(&issue)? != n {
        return Err(ForgeError::remote(502, "issue identity mismatch"));
    }
    let chat = app
        .create_chat(project, input.id, &input.runtime, input.model)
        .await?;
    let Json(binding) = save_binding(
        &app,
        input.id,
        Binding {
            project,
            chat: input.id,
            kind: "issue".into(),
            repository: c.identity(),
            number: n,
        },
    )?;
    Ok(Json(json!({"chat":chat,"issue":issue,"binding":binding})))
}
async fn links(State(app): State<Workbench>, Path(project): Path<Uuid>) -> Reply<Value> {
    app.project(project)?;
    let db = app.0.catalog.lock().unwrap();
    Ok(Json(json!(
        db.forge
            .bindings
            .iter()
            .filter(|(_, v)| v.project == project)
            .map(|(id, b)| json!({"id":id,"binding":b}))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize, Default)]
struct PathFilter {
    path: Option<String>,
}
async fn diff(
    State(app): State<Workbench>,
    headers: axum::http::HeaderMap,
    Path((project, n)): Path<(Uuid, u64)>,
    Query(q): Query<PathFilter>,
) -> Reply<Value> {
    let c = Context::open(app.clone(), project).await?;
    routing::verify(&c, routing::headers(&headers)?.as_ref()).await?;
    let pull = review::refresh(&app, project, &c, n).await?;
    let key = review::key(project, &c, n);
    let head = pull["head_sha"].as_str().unwrap_or_default();
    let reviewed = app.0.catalog.lock().unwrap().forge.reviews[&key]
        .checked
        .get(head)
        .cloned()
        .unwrap_or_default();
    tokio::task::spawn_blocking(move||{
        match q.path{
            Some(path)=>Ok(Json(json!({"patch":review::patch(&c,n,&pull,&path)?,"path":path,"pull":pull,"reviewed":reviewed.contains(&path)}))),
            None=>Ok(Json(json!({"files":review::files(&c,n,&pull)?,"pull":pull,"reviewed":reviewed}))),
        }
    }).await.map_err(|e|ForgeError::bad(e.to_string()))?
}
#[derive(Deserialize)]
struct Mark {
    head: String,
    path: String,
    reviewed: bool,
}
async fn mark(
    State(app): State<Workbench>,
    headers: axum::http::HeaderMap,
    Path((project, n)): Path<(Uuid, u64)>,
    Json(input): Json<Mark>,
) -> Reply<Value> {
    let c = Context::open(app.clone(), project).await?;
    routing::verify(&c, routing::headers(&headers)?.as_ref()).await?;
    let key = review::key(project, &c, n);
    let pull = review::refresh(&app, project, &c, n).await?;
    if pull["head_sha"] != input.head {
        return Err(ForgeError::remote(409, "pull request head changed"));
    }
    let files = tokio::task::spawn_blocking(move || review::files(&c, n, &pull))
        .await
        .map_err(|e| ForgeError::bad(e.to_string()))??;
    if !files.contains(&input.path) {
        return Err(ForgeError::bad("path not present in this revision"));
    }
    Ok(Json(app.update(|db| {
        let r = db.forge.reviews.get_mut(&key).expect("refreshed review");
        if r.pull["head_sha"] != input.head {
            return Err(Error::Busy);
        }
        let paths = r.checked.entry(input.head).or_default();
        if input.reviewed {
            paths.insert(input.path);
        } else {
            paths.remove(&input.path);
        }
        Ok(json!(paths))
    })?))
}
#[derive(Deserialize)]
struct Checkout {
    id: Uuid,
    directory: PathBuf,
    head: String,
}
async fn checkout(
    State(app): State<Workbench>,
    headers: axum::http::HeaderMap,
    Path((project, n)): Path<(Uuid, u64)>,
    Json(input): Json<Checkout>,
) -> Reply<ManagedWorktree> {
    let c = Context::open(app.clone(), project).await?;
    routing::verify(&c, routing::headers(&headers)?.as_ref()).await?;
    let pull = review::refresh(&app, project, &c, n).await?;
    if pull["head_sha"] != input.head {
        return Err(ForgeError::remote(409, "pull request head changed"));
    }
    tokio::task::spawn_blocking(move || {
        review::worktree(&app, &c, n, &pull, input.id, input.directory)
    })
    .await
    .map_err(|e| ForgeError::bad(e.to_string()))?
    .map(Json)
}

async fn issue_tracker(State(app): State<Workbench>, Path(project): Path<Uuid>) -> Reply<Value> {
    let c = providers::linear::tracker(app, project).await?;
    c.execute(&Operation::Identity).await.map(Json)
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .merge(signals::routes())
        .merge(routing::routes())
        .merge(trackers::routes())
        .merge(signin::routes())
        .merge(repository::routes())
        .merge(clone_jobs::routes())
        .merge(providers::issue_directory::routes())
        .route("/forge/drivers", get(drivers))
        .route(
            "/projects/{project}/forge/issues/{n}/detail",
            get(providers::issue_listing::details::read),
        )
        .route(
            "/projects/{project}/forge/issues/{n}/comments",
            get(providers::issue_listing::details::comments),
        )
        .route("/forge/issues/list", post(providers::issue_listing::list))
        .route("/forge/linked-chats", get(linked_chats::read))
        .route(
            "/projects/{project}/forge/issue-tracker",
            get(issue_tracker),
        )
        .route("/projects/{project}/forge/query", post(read))
        .route(
            "/projects/{project}/forge/issues/{number}/chat",
            post(issue_chat),
        )
        .route("/projects/{project}/forge/operations", post(mutate))
        .route("/projects/{project}/forge/operations/{id}", get(operation))
        .route(
            "/projects/{project}/forge/operations/{id}/reconcile",
            post(reconcile),
        )
        .route("/projects/{project}/forge/links", get(links).post(link))
        .route(
            "/projects/{project}/forge/pulls/{n}/change-breakdown",
            get(providers::file_stats::breakdown),
        )
        .route(
            "/projects/{project}/forge/pulls/{n}/stack",
            get(providers::stack::stack),
        )
        .route(
            "/projects/{project}/forge/pulls/{n}/diff/contents",
            post(providers::diff_contents::read),
        )
        .route("/projects/{project}/forge/pulls/{n}/diff", get(diff))
        .route(
            "/projects/{project}/forge/pulls/{n}/files-viewed",
            get(providers::viewed::read).put(providers::viewed::set),
        )
        .route("/projects/{project}/forge/pulls/{n}/reviewed", put(mark))
        .route(
            "/projects/{project}/forge/pulls/{n}/worktree",
            post(checkout),
        )
}
