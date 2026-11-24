//! Publish a chat's admitted commit without taking the workspace write lease held by its model.
use super::*;
use crate::workbench::Catalog;
use crate::workbench_git::Repository;
#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(in crate::workbench) struct Request {
    title: String,
    #[serde(default)]
    body: String,
    base: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    private: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Receipt {
    project: Uuid,
    chat: Uuid,
    request: Request,
    repository: String,
    workspace: PathBuf,
    branch: String,
    commit: String,
    head: String,
    base: String,
    #[serde(default)]
    pushed: bool,
    result: Option<Value>,
}
fn previous(
    app: &Workbench,
    id: Uuid,
    project: Uuid,
    chat: Uuid,
    req: &Request,
    repo: &str,
) -> Result<Option<Receipt>, ForgeError> {
    let db = app.0.catalog.lock().unwrap();
    match db.forge.model_publications.get(&id) {
        Some(r)
            if (r.project, r.chat, &r.request, r.repository.as_str())
                == (project, chat, req, repo) =>
        {
            Ok(Some(r.clone()))
        }
        Some(_) => Err(ForgeError::bad(
            "publication command already belongs to another request",
        )),
        None => Ok(None),
    }
}
fn verify_push_destination(
    app: &Workbench,
    c: &Context,
    push: &str,
    fetch: &str,
) -> Result<(), ForgeError> {
    let local = if push.starts_with('/') {
        Some(PathBuf::from(push))
    } else if push.starts_with("file:") {
        Some(
            Url::parse(push)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .ok_or_else(|| ForgeError::bad("invalid local push URL"))?,
        )
    } else {
        None
    };
    let identity = if let Some(path) = local {
        app.allowed(
            &path
                .canonicalize()
                .map_err(|_| ForgeError::bad("local publication path cannot be resolved"))?,
        )?;
        remote_identity(fetch)?
    } else {
        if push.contains("::") {
            return Err(ForgeError::bad(
                "external Git remote helpers are not publication destinations",
            ));
        }
        if push.contains("://")
            && !Url::parse(push).is_ok_and(|u| matches!(u.scheme(), "https" | "ssh"))
        {
            return Err(ForgeError::bad("untrusted publication transport"));
        }
        remote_identity(push)?
    };
    if !identity.0.eq_ignore_ascii_case(&c.host) || !identity.1.eq_ignore_ascii_case(&c.repository)
    {
        return Err(ForgeError::bad(
            "push destination is outside the granted repository",
        ));
    }
    Ok(())
}
async fn verify_private(c: &Context) -> Result<(), ForgeError> {
    let raw = match c.kind.as_str() {
        "github" | "bitbucket" | "gitlab" | "forgejo" => {
            providers::get(c, providers::rest::project(c)?).await?
        }
        "gitcafe" | "cursor-origin" | "tanstack-source" => {
            providers::get(c, providers::hosted::resource(c, None, None)?).await?
        }
        _ => {
            return Err(ForgeError::unsupported(
                "the granted driver cannot verify private repository visibility",
            ));
        }
    };
    if !(raw["private"] == true || raw["is_private"] == true || raw["visibility"] == "private") {
        return Err(ForgeError::bad(
            "the granted repository is not confirmed private",
        ));
    }
    Ok(())
}
pub(in crate::workbench) async fn run(
    app: Workbench,
    project: Uuid,
    chat: Uuid,
    id: Uuid,
    req: Request,
    authorize: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    if req.title.trim().is_empty() {
        return Err(ForgeError::bad("pull title is required"));
    }
    let info = app.binding(project, chat)?;
    let repo = Repository::open(&info.workspace)?;
    let project_repo = app.repository(project)?;
    if !repo.same_repository(&project_repo) {
        return Err(ForgeError::bad(
            "chat checkout no longer belongs to the granted repository",
        ));
    }
    let c = Context::open(app.clone(), project).await?;
    let identity = c.identity();
    authorize(&app.0.catalog.lock().unwrap(), &identity)?;
    let previous = previous(&app, id, project, chat, &req, &identity)?;
    if let Some(value) = previous.as_ref().and_then(|r| r.result.clone()) {
        return Ok(value);
    }
    if req.draft && matches!(c.kind.as_str(), "bitbucket" | "forgejo") {
        return Err(ForgeError::unsupported(
            "this driver does not expose draft creation",
        ));
    }
    if req.private {
        verify_private(&c).await?;
    }
    let status = repo.status()?;
    let branch = status
        .branch
        .ok_or_else(|| ForgeError::bad("cannot publish detached HEAD"))?;
    let commit = status
        .head
        .ok_or_else(|| ForgeError::bad("cannot publish an empty branch"))?;
    let target = repo.publish_target(&branch)?;
    let pull = PullTarget::resolve(
        app.clone(),
        project,
        &repo,
        &branch,
        &target,
        req.base.as_deref(),
    )
    .await?;
    if pull.repository.is_some() {
        return Err(ForgeError::bad(
            "the push target requires a separate repository write grant",
        ));
    }
    let (fetch, urls) = repo.publish_urls(&target)?;
    if urls.len() != 1 {
        return Err(ForgeError::bad(
            "model publication requires exactly one push destination",
        ));
    }
    let push = urls[0].clone();
    verify_push_destination(&app, &c, &push, &fetch)?;
    let proposal = Receipt {
        project,
        chat,
        workspace: info.workspace.clone(),
        branch: branch.clone(),
        repository: identity.clone(),
        head: pull.head,
        base: pull.base,
        commit,
        request: req.clone(),
        pushed: false,
        result: None,
    };
    let record = app.update(|db| {
        authorize(db, &identity)?;
        if let Some(r) = db.forge.model_publications.get(&id) {
            if r.project != project
                || r.chat != chat
                || r.request != req
                || r.repository != identity
                || r.workspace != proposal.workspace
                || r.branch != branch
                || r.head != proposal.head
                || r.base != proposal.base
            {
                return Err(Error::Refused(
                    "publication target or request changed".into(),
                ));
            }
            return Ok(r.clone());
        }
        if db.forge.operations.contains_key(&id) || db.forge.model_link_receipts.contains_key(&id) {
            return Err(Error::Refused("command ID already used".into()));
        }
        db.forge.model_publications.insert(id, proposal.clone());
        Ok(proposal)
    })?;
    if !record.pushed {
        let commit = record.commit.clone();
        let head = record.head.clone();
        authorize(&app.0.catalog.lock().unwrap(), &identity)?;
        tokio::task::spawn_blocking(move || repo.publish_commit(&commit, &head, &push))
            .await
            .map_err(|e| ForgeError::uncertain(e.to_string()))??;
        app.update(|db| {
            db.forge
                .model_publications
                .get_mut(&id)
                .ok_or_else(|| Error::Refused("missing publication".into()))?
                .pushed = true;
            db.forge.pull_changed()
        })?;
    }
    authorize(&app.0.catalog.lock().unwrap(), &identity)?;
    let list = c.execute(&Operation::Pulls).await?;
    let mut existing = None;
    for row in list
        .as_array()
        .ok_or_else(|| ForgeError::bad("invalid pull list"))?
        .iter()
        .filter(|r| {
            r["head_ref"] == record.head && r["base_ref"] == record.base && r["state"] == "open"
        })
    {
        if publish::matches_repository(&c, &c.repository, row).await? {
            existing = Some(row.clone());
            break;
        }
    }
    let create_id = crate::chat::stable_id([id.as_bytes().as_slice(), b"model publish pull"]);
    let (value, created) = match existing {
        Some(row) => {
            // If a successful remote create preceded a crash, its immutable marker identifies it.
            let ours = row["body"]
                .as_str()
                .is_some_and(|s| s.contains(&format!("<!-- kar-operation:{create_id} -->")));
            (row, ours)
        }
        None => {
            let op = Operation::CreatePull {
                head: record.head.clone(),
                head_repository: None,
                base: record.base.clone(),
                title: req.title,
                body: req.body,
                draft: req.draft,
            };
            (
                routes::model_forge_write(
                    app.clone(),
                    project,
                    chat,
                    create_id,
                    None,
                    op,
                    &authorize,
                )
                .await?,
                true,
            )
        }
    };
    let number = checked_number(&value)?;
    let result = json!({"status":if created{"created"}else{"opened_existing"},"created":created,"pull":value,"head_sha":record.commit,"linked":true});
    app.update(|db| {
        authorize(db, &identity)?;
        let binding = Binding {
            project,
            chat,
            kind: "pull".into(),
            repository: identity.clone(),
            number,
        };
        if !db.forge.bindings.values().any(|b| b == &binding) {
            db.forge.bindings.insert(Uuid::new_v4(), binding);
            db.forge.links_changed()?;
            db.forge
                .link_activity
                .insert(chat, jiff::Timestamp::now().as_millisecond() as u64);
        }
        let r = db
            .forge
            .model_publications
            .get_mut(&id)
            .ok_or_else(|| Error::Refused("unknown publication".into()))?;
        Ok(r.result.get_or_insert(result).clone())
    })
    .map_err(Into::into)
}
