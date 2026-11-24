//! Revision-checked mutations on DSH's session-owned goal service.
use super::web::Web;
use crate::{Error, Event, GoalSetOptions, GoalSnapshot, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize, PartialEq)]
struct GoalRef {
    id: String,
    revision: u64,
}

#[derive(Deserialize)]
struct View {
    #[serde(flatten)]
    reference: GoalRef,
    objective: String,
    phase: String,
    activation: String,
}

fn snapshot(v: &Value, times: &Value) -> Option<GoalSnapshot> {
    Some(GoalSnapshot {
        objective: v["objective"].as_str()?.to_owned(),
        status: v["phase"].as_str()?.to_owned(),
        created_at: times["createdAt"].as_f64().map(|t| t / 1000.0),
        updated_at: times["updatedAt"].as_f64().map(|t| t / 1000.0),
        token_budget: None,
        tokens_used: None,
        time_used_seconds: None,
    })
}

pub(super) fn baseline(values: &Value) -> Result<Option<GoalSnapshot>> {
    let invalid = || Error::Protocol("DSH omitted its native goal projection".into());
    let value = values.get("goal").ok_or_else(invalid)?;
    if value.is_null() {
        return Ok(None);
    }
    snapshot(&value["goal"], value)
        .map(Some)
        .ok_or_else(invalid)
}

pub(super) fn event(v: &Value) -> Option<Event> {
    if v["version"] != 1 {
        return None;
    }
    if v["operation"] == "clear" && v["cleared"]["id"].is_string() {
        return Some(Event::GoalChanged { goal: None });
    }
    Some(Event::GoalChanged {
        goal: Some(snapshot(&v["goal"], v)?),
    })
}

async fn read(web: &Web, id: &str) -> Result<Option<View>> {
    let raw = web.call("goals/get", json!({"agentId":id})).await?;
    Ok(serde_json::from_value(raw)?)
}

async fn set(web: &Web, id: &str, options: GoalSetOptions) -> Result<View> {
    if options.token_budget.is_some() {
        return Err(Error::Unsupported(
            "DSH uses round limits, not token budgets".into(),
        ));
    }
    if options.objective.trim().is_empty() {
        return Err(Error::Refused(
            "a DSH goal needs a nonempty objective".into(),
        ));
    }
    let target = options.status.as_deref();
    if !matches!(target, None | Some("active" | "paused" | "complete")) {
        return Err(Error::Unsupported("unsupported DSH goal phase".into()));
    }
    let objective = &options.objective;
    let mut current = match read(web, id).await? {
        Some(v) if v.phase != "complete" => v,
        _ => {
            let created = web
                .call(
                    "goals/create",
                    json!({"agentId":id,"request":{"objective":objective}}),
                )
                .await?;
            let reference: GoalRef = serde_json::from_value(created["ref"].clone())?;
            read(web, id)
                .await?
                .filter(|v| v.reference == reference)
                .ok_or_else(|| {
                    Error::Refused("DSH goal changed while it was being created".into())
                })?
        }
    };
    if current.objective != *objective {
        let reply = web
            .call(
                "goals/edit",
                json!({"agentId":id,"ref":current.reference,"request":{"objective":objective}}),
            )
            .await?;
        current = serde_json::from_value(reply)?;
    }
    let method = match target {
        Some("active") if current.phase != "active" || current.activation != "armed" => {
            Some("goals/resume")
        }
        Some("paused") if current.phase != "paused" => Some("goals/pause"),
        Some("complete") if current.phase != "complete" => Some("goals/complete"),
        _ => None,
    };
    if let Some(method) = method {
        current = serde_json::from_value(
            web.call(method, json!({"agentId":id,"ref":current.reference}))
                .await?,
        )?;
    }
    if current.objective != *objective || target.is_some_and(|phase| phase != current.phase) {
        return Err(Error::Refused(
            "DSH returned a different goal after an update".into(),
        ));
    }
    Ok(current)
}

/// The caller drains its existing event stream through this exact read cut
/// before acknowledging the change to subscribers.
pub(super) async fn control(
    web: &Web,
    id: &str,
    method: &str,
    args: Value,
) -> Result<(i64, Option<GoalSnapshot>)> {
    let current = match method {
        "goals/get" => read(web, id).await?,
        "goals/set" => Some(set(web, id, serde_json::from_value(args)?).await?),
        "goals/clear" => {
            if let Some(v) = read(web, id).await? {
                let tombstone: GoalRef = serde_json::from_value(
                    web.call("goals/clear", json!({"agentId":id,"ref":v.reference}))
                        .await?,
                )?;
                if tombstone.id != v.reference.id || tombstone.revision <= v.reference.revision {
                    return Err(Error::Protocol("invalid DSH goal clearance receipt".into()));
                }
            }
            None
        }
        _ => return Err(Error::Unsupported("unknown DSH goal operation".into())),
    };
    let cut = web
        .request("session/projections", json!({"sessionId":id}))
        .await?;
    let seq = cut["asOfSeq"]
        .as_i64()
        .filter(|_| cut["kind"] == "sequenced")
        .ok_or_else(|| Error::Protocol("DSH goal read has no sequence".into()))?;
    let goal = baseline(&cut["values"])?;
    let reference: Option<GoalRef> = serde_json::from_value(cut["values"]["goal"]["goal"].clone())?;
    if current.map(|v| v.reference) != reference {
        return Err(Error::Refused("DSH goal changed concurrently".into()));
    }
    Ok((seq, goal))
}
