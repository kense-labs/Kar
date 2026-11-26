use super::*;
use crate::chat::{Item, Role};
use axum::{extract::Path, routing::post};
use std::time::Duration;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Card {
    pub id: Uuid,
    pub project: Uuid,
    pub chat: Uuid,
    turn: Uuid,
    kind: String,
    pub details: Value,
    status: String,
    created: u64,
    expires: u64,
    result: Value,
    trace: Option<Value>,
    notification: Uuid,
    notified: bool,
}
fn now() -> u64 {
    crate::workbench::access::now()
}
fn current_turn(app: &Workbench, g: &Grant) -> Result<Uuid> {
    app.active_control(g.project, g.chat)?
        .and_then(|s| s.model_caller_turn())
        .ok_or_else(|| bad("this tool requires a current caller turn"))
}
fn changed(app: &Workbench) {
    app.0.card_updates.send_modify(|n| *n = n.wrapping_add(1));
}
#[allow(clippy::too_many_arguments)]
pub(super) fn create(
    app: &Workbench,
    c: &Caller,
    g: &Grant,
    req: Uuid,
    kind: &str,
    details: Value,
    global: bool,
    timeout: Option<u64>,
) -> Result<Value> {
    let turn = current_turn(app, g)?;
    let result = app.update(|db| {
        if global {
            check(db, c, g)?;
        } else if !g.active()
            || c.plan
            || !Policy::current(db, g.project, g.chat).is_some_and(Policy::executes)
        {
            return Err(denied());
        }
        let rows = &mut db.environment.cards;
        if let Some(row) = rows.get(&req) {
            if (row.project, row.chat, row.turn) != (g.project, g.chat, turn)
                || row.kind != kind
                || row.details != details
            {
                return Err(bad("request ID already belongs to a different action"));
            }
            return Ok(json!({"status":row.status,"request_id":row.id}));
        }
        if rows.len() >= 512 {
            let old = rows
                .values()
                .filter(|r| {
                    r.notified
                        || r.kind != "link"
                            && !matches!(r.status.as_str(), "pending" | "submitting")
                })
                .min_by_key(|r| r.created)
                .map(|r| r.id)
                .ok_or_else(|| bad("too many pending collaboration requests"))?;
            rows.remove(&old);
        }
        let ms = now();
        rows.insert(
            req,
            Card {
                id: req,
                project: g.project,
                chat: g.chat,
                turn,
                kind: kind.into(),
                details,
                status: "pending".into(),
                created: ms,
                expires: ms.saturating_add(timeout.unwrap_or(24 * 3600000)),
                result: Value::Null,
                trace: None,
                notification: Uuid::new_v4(),
                notified: false,
            },
        );
        Ok(json!({"status":"pending","request_id":req}))
    })?;
    changed(app);
    Ok(result)
}
pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    if !execute(c) {
        return vec![];
    }
    let id = json!({"type":"string","format":"uuid"});
    vec![
        tool(
            "kar_request_secret",
            "Ask the owner for a private single-line value. Returns a one-use secret_ref, or owner-only files for separate fields. Never print the values.",
            json!({"request_id":id,"label":{"type":"string"},"reason":{"type":"string"},"url":{"type":"string"},"placeholder":{"type":"string"},"value":{"type":"string"},"delivery":{"enum":["secret_ref","file"]},"fields":{"type":"array","items":{"type":"object"}},"timeout_ms":{"type":"integer","minimum":1000,"maximum":3600000}}),
            &["request_id", "label", "reason"],
        ),
        tool(
            "kar_feedback",
            "Record feedback about Kar. User-requested reports show an owner approval card with optional conversation excerpts.",
            json!({"request_id":id,"kind":{"enum":["bug","confusing","missing_capability","wrong_docs","other"]},"subject":{"type":"string"},"expected":{"type":"string"},"actual":{"type":"string"},"workaround":{"type":"string"},"user_requested":{"type":"boolean"},"turns":{"type":"integer"}}),
            &["request_id", "kind", "subject", "expected", "actual"],
        ),
    ]
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Feedback {
    request_id: Uuid,
    kind: String,
    subject: String,
    expected: String,
    actual: String,
    #[serde(default)]
    workaround: String,
    #[serde(default)]
    user_requested: bool,
    turns: Option<usize>,
}
fn text_trace(app: &Workbench, chat: Uuid, n: usize) -> Result<Value> {
    let h = app.0.chats.history(chat)?;
    let mut output = Vec::new();
    let mut remaining = 32000;
    for turn in h.turns.iter().rev().take(n).rev() {
        let mut messages = Vec::new();
        for item in &turn.items {
            if let Item::Message {
                role: role @ (Role::User | Role::Assistant),
                text,
                ..
            } = &item.item
                && remaining > 0
            {
                let s = text.chars().take(remaining.min(4000)).collect::<String>();
                remaining -= s.chars().count();
                messages.push(json!({"role":role,"text":s}));
            }
        }
        output.push(json!({"messages":messages}));
    }
    Ok(json!(output))
}
pub(super) async fn call(
    app: &Workbench,
    c: &Caller,
    g: &Grant,
    name: &str,
    mut input: Value,
) -> Result<Value> {
    if name == "kar_feedback" {
        let m: Feedback = serde_json::from_value(input)?;
        let turns = m.turns.unwrap_or(1);
        if turns == 0
            || turns > 32
            || !matches!(
                m.kind.as_str(),
                "bug" | "confusing" | "missing_capability" | "wrong_docs" | "other"
            )
            || [&m.subject, &m.expected, &m.actual, &m.workaround]
                .into_iter()
                .any(|s| s.chars().count() > 300)
        {
            return Err(bad("invalid feedback report"));
        }
        let mut details = json!(m);
        let p = app.project(g.project)?;
        let mut redact = vec![
            p.root.to_string_lossy().into_owned(),
            g.project.to_string(),
            g.chat.to_string(),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            redact.push(home.to_string_lossy().into_owned());
        }
        crate::transport::redact(&mut details, &redact);
        let pending = create(app, c, g, m.request_id, "feedback", details, false, None)?;
        if pending["status"] != "pending" {
            return Ok(pending);
        }
        if m.user_requested {
            let trace = text_trace(app, g.chat, turns)?;
            app.update(|db| {
                let card = db
                    .environment
                    .cards
                    .get_mut(&m.request_id)
                    .ok_or_else(denied)?;
                if card.status == "pending" && card.trace.is_none() {
                    card.trace = Some(trace);
                }
                Ok(())
            })?;
            return Ok(pending);
        }
        return approve(
            app,
            g.project,
            g.chat,
            m.request_id,
            true,
            json!({"include_conversation":false}),
        )
        .await;
    }
    let pasted = input.as_object_mut().and_then(|o| o.remove("value"));
    if let Some(v) = &pasted {
        let text = v
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 65536 && !s.contains(['\r', '\n', '\0']))
            .ok_or_else(|| bad("invalid pasted private value"))?;
        let turn = current_turn(app, g)?;
        let h = app.0.chats.history(g.chat)?;
        if !h
            .turns
            .iter()
            .find(|t| t.id == turn)
            .and_then(|t| t.prompt())
            .is_some_and(|p| p.contains(text))
        {
            return Err(bad(
                "a pasted value must come from this turn's user message",
            ));
        }
    }
    let (req, details, ms) = secrets::spec(input)?;
    let mut changed = app.0.card_updates.subscribe();
    create(app, c, g, req, "secret", details, false, Some(ms))?;
    if let Some(value) = pasted {
        return approve(app, g.project, g.chat, req, true, json!({"value":value})).await;
    }
    let until = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let row = app
            .0
            .catalog
            .lock()
            .unwrap()
            .environment
            .cards
            .get(&req)
            .cloned()
            .ok_or_else(denied)?;
        if !matches!(row.status.as_str(), "pending" | "submitting") {
            return Ok(if row.result.is_null() {
                json!({"status":row.status})
            } else {
                row.result
            });
        }
        let live = current_turn(app, g).ok() == Some(row.turn)
            && g.active()
            && Policy::current(&app.0.catalog.lock().unwrap(), row.project, row.chat)
                .is_some_and(Policy::executes);
        let end = !live || row.expires <= now() || tokio::time::Instant::now() >= until;
        if end && row.status == "pending" {
            let status = if live { "timed_out" } else { "cancelled" };
            return finish(app, req, status, json!({"status":status}));
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_millis(250))=>{},result=changed.changed()=>{result.map_err(|_|denied())?;}}
    }
}
fn finish(app: &Workbench, req: Uuid, status: &str, result: Value) -> Result<Value> {
    let response = app.update(|db| {
        let r = db.environment.cards.get_mut(&req).ok_or_else(denied)?;
        if !matches!(r.status.as_str(), "submitting" | "pending") {
            return Ok(r.result.clone());
        }
        r.status = status.into();
        r.result = result.clone();
        Ok(result)
    })?;
    changed(app);
    Ok(response)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    accept: bool,
    #[serde(default)]
    input: Value,
}
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/projects/{project}/chats/{chat}/collaboration/requests",
            get(list),
        )
        .route(
            "/projects/{project}/chats/{chat}/collaboration/requests/{id}",
            post(decide),
        )
}
async fn list(State(app): State<Workbench>, Path((p, c)): Path<(Uuid, Uuid)>) -> Reply {
    app.binding(p, c)?;
    let db = app.0.catalog.lock().unwrap();
    Ok(Json(
        json!({"requests":db.environment.cards.values().filter(|r|r.chat==c&&r.project==p).collect::<Vec<_>>()}),
    ))
}
async fn decide(
    State(app): State<Workbench>,
    Path((p, c, id)): Path<(Uuid, Uuid, Uuid)>,
    Json(d): Json<Decision>,
) -> Reply {
    app.binding(p, c)?;
    let result = tokio::spawn(async move { approve(&app, p, c, id, d.accept, d.input).await })
        .await
        .map_err(|_| bad("private response task interrupted"))??;
    Ok(Json(result))
}
async fn approve(
    app: &Workbench,
    p: Uuid,
    c: Uuid,
    id: Uuid,
    allow: bool,
    input: Value,
) -> Result<Value> {
    let row = app.update(|db| {
        let r = db
            .environment
            .cards
            .get_mut(&id)
            .filter(|r| r.project == p && r.chat == c)
            .ok_or_else(denied)?;
        if r.status == "submitting" {
            return Err(Error::Busy);
        }
        if r.status != "pending" {
            return Ok((false, r.clone()));
        }
        if r.expires <= now() {
            r.status = "timed_out".into();
            r.result = json!({"status":"timed_out"});
            return Ok((false, r.clone()));
        }
        if !allow {
            r.status = "declined".into();
            r.result = json!({"status":"declined"});
            return Ok((false, r.clone()));
        }
        r.status = "submitting".into();
        Ok((true, r.clone()))
    })?;
    changed(app);
    if !row.0 {
        notify_link(app, &row.1)?;
        return Ok(row.1.result);
    }
    let row = row.1;
    let done = match row.kind.as_str() {
        "link" => links::approve(app, &row, input).await,
        "secret" => {
            if app
                .active_control(p, c)?
                .and_then(|s| s.model_caller_turn())
                != Some(row.turn)
            {
                Ok(json!({"status":"cancelled"}))
            } else {
                secrets::accept(app, &row, input)
            }
        }
        "feedback" => feedback(app, &row, input).await,
        _ => Err(denied()),
    };
    let response = match done {
        Ok(result) => finish(app, id, "resolved", result)?,
        Err(e) => {
            finish(
                app,
                id,
                "failed",
                json!({"status":"failed","reason":e.to_string()}),
            )?;
            notify_link(app, &row)?;
            return Err(e);
        }
    };
    notify_link(app, &row)?;
    Ok(response)
}
fn notify_link(app: &Workbench, r: &Card) -> Result<()> {
    if r.kind != "link" || r.notified {
        return Ok(());
    }
    let result = app
        .0
        .catalog
        .lock()
        .unwrap()
        .environment
        .cards
        .get(&r.id)
        .filter(|r| !matches!(r.status.as_str(), "pending" | "submitting"))
        .map(|r| r.result.clone());
    let Some(result) = result else {
        return Ok(());
    };
    if app.archived(r.chat) || app.binding(r.project, r.chat).is_err() {
        return app.update(|db| {
            if let Some(saved) = db.environment.cards.get_mut(&r.id) {
                saved.notified = true;
            }
            Ok(())
        });
    }
    app.0.chats.enqueue(
        r.chat,
        r.notification,
        format!("Kar environment link request {}: {}", r.id, result),
    )?;
    app.wake_queue();
    app.update(|db| {
        if let Some(row) = db.environment.cards.get_mut(&r.id) {
            row.notified = true;
        }
        Ok(())
    })
}
async fn feedback(_app: &Workbench, row: &Card, input: Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Choice {
        #[serde(default)]
        include_conversation: bool,
    }
    let choice: Choice = serde_json::from_value(if input.is_null() { json!({}) } else { input })?;
    let mut report = row.details.clone();
    report.as_object_mut().unwrap().remove("request_id");
    if choice.include_conversation {
        report["conversation"] = row
            .trace
            .clone()
            .ok_or_else(|| bad("no approved conversation preview"))?;
    }
    let Some(url) = std::env::var("KAR_FEEDBACK_URL")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(json!({"status":"stored","sent":false}));
    };
    let target = url::Url::parse(&url).map_err(|_| bad("invalid host feedback destination"))?;
    if !matches!(target.scheme(), "https" | "http") {
        return Err(bad("invalid host feedback destination"));
    }
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(target)
        .header("Idempotency-Key", row.id.to_string())
        .json(&report)
        .send()
        .await;
    Ok(match r {
        Ok(r) => {
            json!({"status":"stored","sent":r.status().is_success(),"http_status":r.status().as_u16()})
        }
        Err(_) => json!({"status":"delivery_unknown","sent":null}),
    })
}
pub(in crate::workbench) fn recover(app: &Workbench) -> Result<()> {
    let pending: Vec<_> = app
        .0
        .catalog
        .lock()
        .unwrap()
        .environment
        .cards
        .values()
        .filter(|r| {
            r.status == "submitting"
                || r.status == "pending" && r.kind == "secret"
                || r.kind == "link" && !r.notified
        })
        .cloned()
        .collect();
    for row in pending {
        if row.status == "submitting" {
            let linked = app
                .0
                .catalog
                .lock()
                .unwrap()
                .environment
                .links
                .values()
                .find(|p| p.request_id == row.id)
                .map(|p| p.id);
            let result = linked.map_or_else(
                || json!({"status":"outcome_unknown"}),
                |id| json!({"status":"linked","environment_id":id}),
            );
            finish(app, row.id, "resolved", result)?;
        } else if row.kind == "secret" && row.status == "pending" {
            finish(app, row.id, "cancelled", json!({"status":"cancelled"}))?;
        }
        notify_link(app, &row)?;
    }
    Ok(())
}
