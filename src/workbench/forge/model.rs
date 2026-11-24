//! Repository-pinned reads and durable thread links shared with the forge catalog.
use super::*;
use crate::workbench::Catalog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::workbench) enum Kind {
    Issue,
    Pull,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pull => "pull",
        }
    }
    fn read(self, n: u64) -> Operation {
        match self {
            Self::Issue => Operation::Issue { number: n },
            Self::Pull => Operation::Pull { number: n },
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::workbench) enum Change {
    Link,
    Unlink,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct LinkReceipt {
    project: Uuid,
    chat: Uuid,
    repository: String,
    kind: Kind,
    change: Change,
    numbers: Vec<u64>,
    result: Value,
}

pub(in crate::workbench) async fn pull_repository(
    app: Workbench,
    p: Uuid,
) -> Result<String, ForgeError> {
    let c = Context::open(app, p).await?;
    if c.kind == "linear" {
        return Err(ForgeError::unsupported(
            "selected host has no pull requests",
        ));
    }
    Ok(c.identity())
}

pub(in crate::workbench) async fn links(
    app: Workbench,
    project: Uuid,
    chat: Uuid,
    kind: Kind,
    scope: Option<&str>,
    change: Option<(Change, Uuid, Vec<u64>)>,
    authorize: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    app.binding(project, chat)?;
    let ctx = Context::for_operation(app.clone(), project, &kind.read(1), scope).await?;
    let repository = ctx.identity();
    {
        let db = app.0.catalog.lock().unwrap();
        authorize(&db, &repository)?;
        if change.is_none() {
            let mut nums: BTreeSet<_> = Default::default();
            let links = db
                .forge
                .bindings
                .values()
                .filter(|v| {
                    v.project == project
                        && v.chat == chat
                        && v.kind == kind.name()
                        && v.repository == repository
                        && nums.insert(v.number)
                })
                .collect::<Vec<_>>();
            return Ok(json!({"links":links}));
        }
    }
    let (action, id, numbers) = change.unwrap();
    if numbers.is_empty()
        || numbers.len() > 100
        || numbers.iter().any(|n| *n == 0)
        || numbers.iter().collect::<BTreeSet<_>>().len() != numbers.len()
    {
        return Err(ForgeError::bad("expected 1 to 100 unique positive numbers"));
    }
    let previous = |db: &Catalog| -> crate::Result<Option<Value>> {
        match db.forge.model_link_receipts.get(&id) {
            Some(r)
                if (
                    r.project,
                    r.chat,
                    &r.repository,
                    r.kind,
                    r.change,
                    &r.numbers,
                ) == (project, chat, &repository, kind, action, &numbers) =>
            {
                Ok(Some(r.result.clone()))
            }
            Some(_) => Err(Error::Refused("link command already used".into())),
            None => Ok(None),
        }
    };
    if let Some(result) = previous(&app.0.catalog.lock().unwrap())? {
        return Ok(result);
    }
    if action == Change::Link {
        for n in &numbers {
            let value = ctx.execute(&kind.read(*n)).await?;
            if checked_number(&value)? != *n {
                return Err(ForgeError::bad("remote object identity mismatch"));
            }
        }
    }
    Ok(app.update(|db| {
        authorize(db, &repository)?;
        if let Some(result) = previous(db)? {
            return Ok(result);
        }
        if db.forge.operations.contains_key(&id) || db.forge.bindings.contains_key(&id) {
            return Err(Error::Refused(
                "command ID already belongs to another forge request".into(),
            ));
        }
        let mut changes = Vec::new();
        let mut changed = false;
        for n in &numbers {
            let link = Binding {
                project,
                chat,
                repository: repository.clone(),
                kind: kind.name().into(),
                number: *n,
            };
            let existed = db.forge.bindings.values().any(|b| b == &link);
            match action {
                Change::Link if !existed => {
                    db.forge.bindings.insert(Uuid::new_v4(), link);
                    changed = true;
                }
                Change::Unlink if existed => {
                    db.forge.bindings.retain(|_, b| b != &link);
                    changed = true;
                }
                _ => {}
            }
            changes.push(
                json!({"number":n,"previously_linked":existed,"linked":action==Change::Link}),
            );
        }
        if changed {
            db.forge.links_changed()?;
            db.forge
                .link_activity
                .insert(chat, jiff::Timestamp::now().as_millisecond() as u64);
        }
        let result = json!({"changes":changes});
        db.forge.model_link_receipts.insert(
            id,
            LinkReceipt {
                project,
                chat,
                repository: repository.clone(),
                kind,
                change: action,
                numbers: numbers.clone(),
                result: result.clone(),
            },
        );
        Ok(result)
    })?)
}

pub(in crate::workbench) async fn pull(
    app: Workbench,
    p: Uuid,
    chat: Uuid,
    command: Option<Uuid>,
    op: Operation,
    authorize: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    match op {
        Operation::Pulls
        | Operation::Pull { .. }
        | Operation::ReviewThreads { .. }
        | Operation::ThreadComments { .. } => {
            if command.is_some() {
                return Err(ForgeError::bad("reads do not accept a command ID"));
            }
            actions::validate(&op)?;
            let c = Context::open(app.clone(), p).await?;
            let repo = c.identity();
            authorize(&app.0.catalog.lock().unwrap(), &repo)?;
            let result = if let Operation::Pull { number } = &op {
                review::refresh(&app, p, &c, *number).await?
            } else {
                c.execute(&op).await?
            };
            authorize(&app.0.catalog.lock().unwrap(), &repo)?;
            Ok(result)
        }
        Operation::CreatePull { .. }
        | Operation::SubmitReview { .. }
        | Operation::ReplyToThread { .. }
        | Operation::SetThreadResolution { .. } => {
            routes::model_forge_write(
                app,
                p,
                chat,
                command.ok_or_else(|| ForgeError::bad("command ID required"))?,
                None,
                op,
                authorize,
            )
            .await
        }
        _ => Err(ForgeError::bad("unsupported pull action")),
    }
}
