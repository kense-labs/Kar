use super::*;
use crate::chat::{InputStatus, TurnStatus};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    text: String,
    mode: Option<String>,
    turn_id: Option<Uuid>,
    request_id: Option<Uuid>,
    #[serde(default)]
    attachments: Vec<Uuid>,
}
pub(super) async fn call(app: &Workbench, c: &Caller, id: Uuid, row: &Record) -> Result<Value> {
    let (p, t) = (row.project, row.target);
    authorize(&app.0.catalog.lock().unwrap(), c, p, t, true)?;
    let req = row.args.clone();
    match row.name.as_str() {
        "kar_thread_interrupt" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                turn_id: Uuid,
                request_id: Option<Uuid>,
            }
            let v: Input = parse(req)?;
            let session = app
                .active_control(p, t)?
                .ok_or_else(crate::transport::AbortRequest::stale)?;
            session.abort_turn(v.turn_id, v.request_id).await?;
            Ok(json!({"interrupted":v.turn_id}))
        }
        "kar_workflow_stop" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                task_id: String,
                started_at: crate::Cursor,
            }
            let v: Input = parse(req)?;
            let s = app.active_control(p, t)?.ok_or_else(denied)?;
            s.stop_workflow(&v.task_id, v.started_at).await?;
            Ok(json!({"stopped":v.task_id}))
        }
        "kar_pending_request" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                action: String,
                cursor: crate::Cursor,
                id: Value,
                reply: crate::QuestionReply,
            }
            let v: Input = parse(req)?;
            if v.action != "respond" {
                return Err(denied());
            }
            let s = app.active_control(p, t)?.ok_or_else(denied)?;
            let r = s
                .snapshot()
                .pending_requests
                .into_iter()
                .find(|r| {
                    r.kind == crate::RequestKind::Question && r.cursor == v.cursor && r.id == v.id
                })
                .ok_or_else(|| Error::Refused("question occurrence is no longer pending".into()))?;
            s.answer_question(&r, v.reply).await?;
            Ok(json!({"answered":r.id,"cursor":r.cursor}))
        }
        "kar_thread_send" => send(app, id, row, parse(req)?).await,
        "kar_thread_configure" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Config {
                action: String,
                runtime: String,
                model: String,
            }
            let a: Config = parse(req)?;
            if a.action != "set"
                || !c
                    .policy
                    .as_ref()
                    .and_then(|p| p.thread_tools.as_ref())
                    .and_then(|s| s.models.get(&a.runtime))
                    .is_some_and(|m| m.contains(&a.model))
            {
                return Err(denied());
            }
            let r = app
                .handoff(
                    p,
                    t,
                    crate::chat::HandoffRequest {
                        id,
                        runtime: a.runtime,
                        instance_id: None,
                        model: Some(a.model),
                        budget_bytes: 65536,
                    },
                )
                .await?;
            Ok(json!({"handoff":r}))
        }
        _ => {
            let mut value = req;
            let action = value["action"].as_str().unwrap_or("").to_owned();
            if row.name == "kar_thread_update" {
                if action == "regenerate" {
                    let h = app.0.chats.history(t)?;
                    let text = h
                        .turns
                        .iter()
                        .filter_map(|r| r.prompt())
                        .find(|s| !s.trim().is_empty())
                        .ok_or_else(|| {
                            Error::Refused("thread has no user message for its title".into())
                        })?;
                    value = json!({"action":"rename","title":text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect::<String>()});
                } else if action != "rename" {
                    return Err(denied());
                }
            } else if matches!(action.as_str(), "archive" | "unarchive") {
                if action == "archive"
                    && app.active_control(p, t)?.is_some()
                    && app
                        .0
                        .chats
                        .history(t)?
                        .turns
                        .last()
                        .is_some_and(|r| r.status == TurnStatus::Running)
                {
                    return Err(Error::Busy);
                }
                return Ok(json!(app.set_archived(p, t, action == "archive").await?));
            } else if !matches!(
                action.as_str(),
                "pin"
                    | "unpin"
                    | "snooze"
                    | "unsnooze"
                    | "settle"
                    | "unsettle"
                    | "mark_unread"
                    | "visit"
            ) {
                return Err(denied());
            }
            value["command_id"] = json!(id);
            crate::workbench::thread_state::model_change(app, p, t, value, |db| {
                authorize(db, c, p, t, true)
            })
        }
    }
}
async fn send(app: &Workbench, cmd: Uuid, row: &Record, a: Send) -> Result<Value> {
    let (p, t) = (row.project, row.target);
    let mode = a.mode.as_deref().unwrap_or("auto");
    if !matches!(mode, "auto" | "queue" | "steer" | "restart") {
        return Err(Error::Refused("unknown send mode".into()));
    }
    if app.archived(t) {
        return Err(Error::Refused("chat is archived".into()));
    }
    // A recovered admission belongs to the original turn. Its inbox row, including
    // any promotion, is authoritative even if a different turn is now active.
    if let Some(prior) = app
        .0
        .chats
        .queued_inputs(t)?
        .into_iter()
        .find(|q| q.id == row.input)
    {
        app.wake_queue();
        return Ok(
            json!({"thread_id":t,"input_id":row.input,"delivery":"recovered","receipt":prior}),
        );
    }
    let source = app.0.chats.history(t)?;
    let active = source
        .turns
        .last()
        .filter(|r| r.status == TurnStatus::Running)
        .map(|r| r.id);
    if matches!(mode, "steer" | "restart") && (a.turn_id.is_none() || a.turn_id != active) {
        return Err(crate::transport::AbortRequest::stale());
    }
    let session = app.active_control(p, t)?;
    let steering = mode == "steer"
        || mode == "auto"
            && active.is_some()
            && session.as_ref().is_some_and(|s| s.capabilities().steering);
    if mode == "steer" && session.as_ref().is_none_or(|s| !s.capabilities().steering) {
        return Err(Error::Unsupported(
            "target has no live steering support".into(),
        ));
    }
    if mode == "steer" {
        let dest = crate::chat::SteerTarget {
            turn: active.unwrap(),
            request_id: a.request_id,
        };
        let receipt = crate::workbench::interactions::steer_input(
            app.clone(),
            p,
            t,
            row.input,
            cmd,
            dest,
            Some((a.text, a.attachments)),
        )
        .await?;
        return Ok(
            json!({"thread_id":t,"input_id":row.input,"delivery":"steer","receipt":receipt}),
        );
    }
    let queued = app
        .0
        .chats
        .enqueue_attached(t, row.input, a.text, &a.attachments)?;
    if mode == "restart" {
        let result = match session.as_ref() {
            Some(s) => s.abort_turn(active.unwrap(), a.request_id).await,
            None => Err(crate::transport::AbortRequest::stale()),
        };
        if let Err(e) = result {
            app.0.chats.withdraw(t, row.input)?;
            return Err(e);
        }
    }
    let mut status = json!(queued);
    let mut route = "queued";
    if steering
        && let Some(turn) = active
        && queued.status == InputStatus::Pending
    {
        let dest = crate::chat::SteerTarget {
            turn,
            request_id: a.request_id,
        };
        match crate::workbench::interactions::steer_queued(app.clone(), p, t, row.input, cmd, dest)
            .await
        {
            Ok(s) => {
                status = json!(s);
                route = "steer";
            }
            Err(e) => {
                let found = app
                    .0
                    .chats
                    .queued_inputs(t)?
                    .into_iter()
                    .find(|q| q.id == row.input)
                    .ok_or_else(denied)?;
                if found.status == InputStatus::Uncertain {
                    return Err(e);
                }
                status = json!(found);
            }
        }
    }
    app.wake_queue();
    Ok(json!({"thread_id":t,"input_id":row.input,"delivery":route,"receipt":status}))
}
