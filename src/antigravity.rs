//! Antigravity confirms ACP selections with both the command response and a session notification.
use crate::{Error, OpenError, Record, Result, Session, SessionOptions, opening::OpenStep};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};

fn option<'a>(state: &'a Value, key: &str) -> Option<&'a Value> {
    state["configOptions"]
        .as_array()?
        .iter()
        .find(|v| v["id"] == key || v["category"] == key)
}
fn current<'a>(state: &'a Value, key: &str) -> Option<&'a str> {
    option(state, key)?["currentValue"].as_str()
}
fn selectors(state: &Value, key: &str) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(menu) = option(state, key).and_then(|o| o["options"].as_array()) {
        for entry in menu {
            if let Some(id) = entry["value"].as_str()
                && !ids.iter().any(|s| s == id)
            {
                ids.push(id.into());
            }
        }
    }
    ids
}

fn refusal(opts: &SessionOptions, state: &Value, catalog: Option<&[String]>) -> Error {
    let requested = opts.model.as_deref().unwrap_or_default();
    let reported = current(state, "model");
    OpenError::ModelConfirmation {
        message: format!("Antigravity could not confirm {requested:?}, reported model {:?} and thought level {:?}", reported, current(state, "thought_level")),
        requested: requested.into(),
        reported: reported.map(str::to_owned),
        catalog_selector_ids: catalog.map(<[String]>::to_vec),
        cause: None,
    }.redacted(&crate::options::session_secrets(opts))
}

fn observe(record: &Record, session_id: &str, key: &str, latest: &mut Option<Value>) {
    let v = &record.native;
    let update = &v["params"]["update"];
    if v["method"] == "session/update"
        && v["params"]["sessionId"] == session_id
        && update["sessionUpdate"] == "config_option_update"
        && option(update, key).is_some()
    {
        *latest = Some(update.clone());
    }
}

async fn set(
    session: &Session,
    id: &str,
    selected: &Value,
    value: &str,
    opts: &SessionOptions,
    catalog: Option<&[String]>,
) -> Result<Value> {
    let key = selected["id"]
        .as_str()
        .ok_or_else(|| Error::Protocol("Antigravity omitted its config option ID".into()))?;
    let since = session.records().last().map(|r| r.cursor());
    let mut records = session.subscribe(since.as_ref())?;
    let method = "session/set_config_option";
    let deadline = Instant::now() + opts.acp_timeout("antigravity", method);
    let args = json!({"sessionId":id,"configId":key,"value":value});
    let is_model = key == "model" || selected["category"] == "model";
    let result = if is_model {
        session.open_rpc(method, args, OpenStep::Model).await
    } else {
        session.rpc(method, args).await
    };
    let answer = match result {
        Ok(body) => Some(body),
        Err(Error::Timeout(_)) => None,
        Err(e) => return Err(e),
    };
    let mut latest = None;
    loop {
        while let Some(r) = records.try_next() {
            observe(&r, id, key, &mut latest);
        }
        if let Some(state) = &latest
            && current(state, key) == Some(value)
            && let Some(answer) = answer
        {
            if current(&answer, key) == Some(value) {
                return Ok(answer);
            }
            if is_model && catalog.is_none() {
                return Ok(state.clone());
            }
            return Err(if is_model {
                refusal(opts, &answer, catalog)
            } else {
                Error::Refused("Antigravity did not confirm the requested thought level".into())
            });
        }
        if answer.is_none() || Instant::now() >= deadline {
            break;
        }
        match timeout_at(deadline, records.next()).await {
            Ok(Some(r)) => observe(&r, id, key, &mut latest),
            Ok(None) => {
                return Err(Error::Closed(
                    "Antigravity exited during configuration".into(),
                ));
            }
            Err(_) => break,
        }
    }
    if let Some(state) = latest
        && current(&state, key) != Some(value)
    {
        return Err(if is_model {
            refusal(opts, &state, catalog)
        } else {
            Error::Refused("Antigravity reported a different thought level".into())
        });
    }
    Err(Error::Timeout(
        "confirming Antigravity configuration".into(),
    ))
}

pub(crate) async fn configure(
    session: &Session,
    id: &str,
    opened: &Value,
    opts: &SessionOptions,
) -> Result<Value> {
    let mut state = opened.clone();
    let mut effort = opts.settings.effort.as_deref();
    let mut combined = false;
    let mut chosen = None;
    let catalog = selectors(opened, "model");
    if let Some(wanted) = opts.model.as_deref() {
        let model = if catalog.iter().any(|m| m == wanted) {
            wanted
        } else {
            let mut candidates = catalog.iter().filter_map(|m| {
                Some((
                    m.as_str(),
                    wanted
                        .strip_prefix(m)?
                        .strip_prefix('-')
                        .filter(|s| !s.is_empty())?,
                ))
            });
            let Some((model, level)) = candidates.next() else {
                return Err(refusal(opts, opened, Some(&catalog)));
            };
            if candidates.next().is_some() {
                return Err(refusal(opts, opened, Some(&catalog)));
            }
            if effort.is_some_and(|s| s != level) {
                return Err(Error::Unsupported(
                    "effort conflicts with the Antigravity model selector".into(),
                ));
            }
            effort = Some(level);
            combined = true;
            model
        };
        chosen = Some(model);
        let selected = option(&state, "model")
            .ok_or_else(|| Error::Protocol("Antigravity omitted its model option ID".into()))?;
        let answer = set(
            session,
            id,
            selected,
            model,
            opts,
            combined.then_some(catalog.as_slice()),
        )
        .await?;
        state["configOptions"] = answer["configOptions"].clone();
    }
    if let Some(level) = effort {
        if combined
            && !selectors(&state, "thought_level")
                .iter()
                .any(|s| s == level)
        {
            return Err(refusal(opts, &state, Some(&catalog)));
        }
        let selected = option(&state, "thought_level")
            .ok_or_else(|| Error::Unsupported("Antigravity has no thought_level option".into()))?;
        let answer = set(session, id, selected, level, opts, None).await?;
        state["configOptions"] = answer["configOptions"].clone();
        if combined && current(&state, "model") != chosen {
            return Err(refusal(opts, &state, Some(&catalog)));
        }
    }
    Ok(state)
}
