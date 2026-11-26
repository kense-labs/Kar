//! Context transfers have durable source points. A merge is applied to a future user turn.
use super::*;
use crate::chat::{ChatHistory, ForkPolicy, Item, MergedContext, Role, TurnStatus};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Point {
    LatestStable,
    Run { turn_id: Uuid },
    Checkpoint { turn_id: Uuid },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: String,
    source_point: Option<Point>,
    target_thread_id: Option<Uuid>,
    title: Option<String>,
    policy: Option<ForkPolicy>,
}

pub(super) fn select(h: &ChatHistory, args: &Value) -> Result<Uuid> {
    let req: Request = parse(args.clone())?;
    let source = match req.source_point.unwrap_or(Point::LatestStable) {
        Point::LatestStable => h
            .turns
            .iter()
            .rev()
            .find(|r| r.status == TurnStatus::Completed),
        Point::Run { turn_id } => h
            .turns
            .iter()
            .find(|r| r.id == turn_id && r.status == TurnStatus::Completed),
        Point::Checkpoint { turn_id } => h.turns.iter().find(|r| {
            r.id == turn_id
                && r.status == TurnStatus::Completed
                && r.checkpoint
                    .as_ref()
                    .is_some_and(|cp| cp.after_tree.is_some())
        }),
    };
    source
        .map(|r| r.id)
        .ok_or_else(|| Error::Refused("thread transfer needs a stable source point".into()))
}
pub(super) fn fork_project(db: &Catalog, caller: Uuid, target: Uuid) -> Option<Uuid> {
    db.model_thread_commands
        .get(&target)
        .filter(|r| {
            r.source == caller
                && r.name == "kar_thread_transfer"
                && r.args["action"] == "fork"
                && r.result.as_ref().is_some_and(|r| r.is_ok())
        })
        .map(|r| r.project)
}
pub(super) fn status(app: &Workbench, c: &Caller, p: Uuid, chat: Uuid) -> Result<Value> {
    let observed: Vec<_> = {
        let db = app.0.catalog.lock().unwrap();
        db.model_thread_commands
            .iter()
            .filter(|(id, r)| {
                r.project == p
                    && r.name == "kar_thread_transfer"
                    && (r.target == chat
                        || **id == chat && r.args["action"] == "fork"
                        || (r.args["action"] == "merge_back"
                            && r.args["target_thread_id"] == json!(chat)))
            })
            .map(|(id, r)| (*id, r.clone()))
            .collect()
    };
    let mut rows = vec![];
    for (id, r) in observed {
        let dest = if r.args["action"] == "fork" {
            id
        } else {
            parse(r.args["target_thread_id"].clone())?
        };
        if !visible(&app.0.catalog.lock().unwrap(), c, p, r.target)
            || !visible(&app.0.catalog.lock().unwrap(), c, p, dest)
        {
            continue;
        }
        let mut status = json!({"id":id,"action":r.args["action"],"source_thread_id":r.target,"target_thread_id":dest,"turn_id":r.transfer_turn,"status":match &r.result{None=>"admitted",Some(Err(_))=>"failed",Some(Ok(_))=>"completed"}});
        if let Some(Err(reason)) = r.result {
            status["error"] = reason.into();
        }
        if r.args["action"] == "merge_back"
            && let Some(s) = app.0.chats.context_transfer_status(dest)?.remove(&id)
        {
            status
                .as_object_mut()
                .unwrap()
                .extend(s.as_object().unwrap().clone());
        }
        rows.push(status);
    }
    Ok(json!({"transfers":rows}))
}
pub(super) async fn apply(app: &Workbench, c: &Caller, id: Uuid, row: &Record) -> Result<Value> {
    let args: Request = parse(row.args.clone())?;
    let (p, source) = (row.project, row.target);
    let turn = row
        .transfer_turn
        .ok_or_else(|| Error::Protocol("missing saved fork point".into()))?;
    match args.action.as_str() {
        "fork" => {
            if args.target_thread_id.is_some() {
                return Err(Error::Refused(
                    "fork uses the command ID as its new thread ID".into(),
                ));
            }
            if let Some(title) = &args.title
                && (title.trim().is_empty()
                    || title.chars().count() > 200
                    || title.contains(['\r', '\n', '\0']))
            {
                return Err(Error::Refused("invalid fork title".into()));
            }
            let info = app.binding(p, source)?;
            app.workspace_at(&info.workspace)?;
            may_run(
                app,
                p,
                c.grant.as_ref().unwrap().project,
                row.source,
                source,
            )?;
            app.reserve_fork_directory(
                source,
                p,
                Some(turn),
                id,
                p,
                &info.workspace,
                false,
                false,
            )?;
            let child = app
                .0
                .chats
                .fork_context(source, turn, id, args.policy.unwrap_or_default())
                .await?;
            if let Some(title) = args.title {
                crate::workbench::thread_state::model_change(
                    app,
                    p,
                    id,
                    json!({"command_id":id,"action":"rename","title":title}),
                    |db| authorize(db, c, p, source, true),
                )?;
            }
            Ok(
                json!({"action":"fork","target_thread_id":id,"source_thread_id":source,"turn_id":turn,"mode":child.forked_from.unwrap().mode}),
            )
        }
        "merge_back" => {
            if args.title.is_some() || args.policy.is_some() {
                return Err(denied());
            }
            let dest = args
                .target_thread_id
                .ok_or_else(|| Error::Refused("merge needs a target thread ID".into()))?;
            target(app, c, Some(p), Some(dest), true)?;
            may_run(app, p, c.grant.as_ref().unwrap().project, row.source, dest)?;
            let h = app.0.chats.history(source)?;
            let origin = h
                .chat
                .forked_from
                .filter(|f| f.chat == dest)
                .ok_or_else(|| Error::Refused("merge target must be the fork's parent".into()))?;
            let start = h
                .turns
                .iter()
                .position(|t| t.id == origin.turn)
                .ok_or_else(|| Error::Refused("fork base is outside the current history".into()))?;
            let end = h
                .turns
                .iter()
                .position(|t| t.id == turn && t.status == TurnStatus::Completed)
                .ok_or_else(|| Error::Refused("source point is no longer present".into()))?;
            if start >= end {
                return Err(Error::Refused(
                    "no fork contributions after this base".into(),
                ));
            }
            let mut text = String::new();
            for turn in &h.turns[start + 1..=end] {
                for item in turn.items.iter().filter(|i| i.source.path.is_empty()) {
                    let Item::Message {
                        role: role @ (Role::User | Role::Assistant),
                        text: content,
                        ..
                    } = &item.item
                    else {
                        continue;
                    };
                    text.push_str(if *role == Role::User {
                        "User: "
                    } else {
                        "Assistant: "
                    });
                    text.push_str(content);
                    text.push('\n');
                }
            }
            if text.len() > 48 * 1024 {
                let mut first = text.len() - 48 * 1024;
                while !text.is_char_boundary(first) {
                    first += 1;
                }
                text = format!("[Earlier text omitted]\n{}", &text[first..]);
            }
            app.0.chats.prepare_merge(
                dest,
                MergedContext {
                    id,
                    source,
                    turn,
                    base: origin.turn,
                    text,
                },
            )?;
            Ok(
                json!({"action":"merge_back","target_thread_id":dest,"source_thread_id":source,"turn_id":turn,"status":"pending"}),
            )
        }
        _ => Err(denied()),
    }
}
