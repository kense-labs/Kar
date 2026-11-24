//! Pi RPC clones its active leaf or forks immediately before a native user entry.
use crate::{Error, Result, Runtime, Session, SessionOptions};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

// A get_entries reply can include inactive siblings. An incremental reply can
// start partway along the branch; callers that need the root require it explicitly.
fn branch(body: &Value, full: bool) -> Option<Vec<&Value>> {
    let rows = body["entries"].as_array()?;
    let mut entries = HashMap::new();
    for row in rows {
        let id = row["id"].as_str()?;
        if id.is_empty() || entries.insert(id, row).is_some() {
            return None;
        }
    }
    let mut cursor = body["leafId"].as_str()?;
    let mut seen = HashSet::new();
    let mut path = Vec::new();
    loop {
        let Some(row) = entries.get(cursor) else {
            if full || path.is_empty() {
                return None;
            }
            break;
        };
        if !seen.insert(cursor) {
            return None;
        }
        path.push(*row);
        match row.get("parentId")? {
            Value::String(id) => cursor = id,
            Value::Null => break,
            _ => return None,
        }
    }
    path.reverse();
    Some(path)
}

pub(crate) fn settled_anchor(body: &Value, users: &[u64], assistant: u64) -> Option<String> {
    if users.is_empty() {
        return None;
    }
    let mut expected: HashSet<_> = users.iter().map(|&stamp| ("user", stamp)).collect();
    expected.insert(("assistant", assistant));
    let mut seen = HashSet::new();
    for row in body["entries"].as_array()? {
        let m = &row["message"];
        if let (Some(role), Some(stamp)) = (m["role"].as_str(), m["timestamp"].as_u64())
            && expected.contains(&(role, stamp))
            && !seen.insert((role, stamp))
        {
            return None;
        }
    }
    let path = branch(body, false)?;
    let mut messages = path.iter().rev().filter(|e| e["type"] == "message");
    let answer = &messages.next()?["message"];
    if answer["role"] != "assistant"
        || answer["timestamp"].as_u64() != Some(assistant)
        || !matches!(answer["stopReason"].as_str(), Some("stop" | "length"))
    {
        return None;
    }
    let mut native_users = messages.filter(|e| e["message"]["role"] == "user");
    for &stamp in users.iter().rev() {
        if native_users.next()?["message"]["timestamp"].as_u64() != Some(stamp) {
            return None;
        }
    }
    Some(body["leafId"].as_str()?.to_owned())
}

fn invalid() -> Error {
    Error::Protocol("Pi did not confirm the requested native branch".into())
}

struct Cut {
    next_user: Option<String>,
    prefix: Vec<Value>,
}

// Native cloning regenerates labels and rewires parentIds across them. The
// ancestry traversal above establishes order; compare every other field.
fn retained(rows: &[&Value]) -> Vec<Value> {
    rows.iter()
        .filter(|r| r["type"] != "label")
        .map(|r| {
            let mut row = (*r).clone();
            row.as_object_mut().unwrap().remove("parentId");
            row
        })
        .collect()
}

impl Cut {
    fn select(body: &Value, through: Option<&str>) -> Result<Self> {
        let path = branch(body, true).ok_or_else(invalid)?;
        let end = match through {
            None => path.len(),
            Some(anchor) => {
                path.iter().position(|r| r["id"] == anchor).ok_or_else(|| {
                    Error::Refused("Pi fork anchor is absent from the active branch".into())
                })? + 1
            }
        };
        let next_user =
            match path[end..].iter().find(|r| r["type"] != "label") {
                None => None,
                Some(row) if row["type"] == "message" && row["message"]["role"] == "user" => {
                    Some(row["id"].as_str().ok_or_else(invalid)?.to_owned())
                }
                _ => return Err(Error::Unsupported(
                    "Pi RPC can fork at its current leaf or immediately before the next user input"
                        .into(),
                )),
            };
        Ok(Self {
            next_user,
            prefix: retained(&path[..end]),
        })
    }

    fn verify(&self, body: &Value, reopening: bool) -> Result<()> {
        let path = branch(body, true).ok_or_else(invalid)?;
        if body["entries"].as_array().map(Vec::len) != Some(path.len()) {
            return Err(invalid());
        }
        let actual = retained(&path);
        if !actual.starts_with(&self.prefix)
            || !actual[self.prefix.len()..].iter().all(|r| {
                reopening
                    && matches!(
                        r["type"].as_str(),
                        Some("model_change" | "thinking_level_change" | "session_info")
                    )
            })
        {
            return Err(invalid());
        }
        Ok(())
    }

    async fn create(&self, worker: &Session, source: &str) -> Result<String> {
        let response = if let Some(id) = &self.next_user {
            worker.rpc("fork", json!({"entryId":id})).await?
        } else {
            worker.rpc("clone", json!({})).await?
        };
        if response["cancelled"] != false {
            return Err(Error::Refused("Pi did not accept the session fork".into()));
        }
        let state = worker.rpc("get_state", json!({})).await?;
        let child = state["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != source)
            .ok_or_else(invalid)?;
        let body = worker.rpc("get_entries", json!({})).await?;
        self.verify(&body, false)?;
        Ok(child.to_owned())
    }
}

pub(crate) async fn fork(
    rt: &Runtime,
    mut options: SessionOptions,
    source: &str,
    through: Option<&str>,
) -> Result<Session> {
    let mut parent = options.clone();
    parent.resume = Some(source.to_owned());
    // Apply the requested child settings only when opening its confirmed ID.
    parent.model = None;
    parent.settings.name = None;
    parent.settings.effort = None;
    let worker = Session::open(rt, parent, None, None).await?;
    let result = async {
        if worker.info()["isStreaming"] == true
            || worker.info()["isCompacting"] == true
            || worker.info()["pendingMessageCount"]
                .as_u64()
                .is_some_and(|n| n > 0)
        {
            return Err(Error::Busy);
        }
        let body = worker.rpc("get_entries", json!({})).await?;
        let cut = Cut::select(&body, through)?;
        let id = cut.create(&worker, source).await?;
        Ok::<_, Error>((cut, id))
    }
    .await;
    let closed = worker.close().await;
    let (cut, id) = result?;
    options.resume = Some(id);
    closed?;
    let session = Session::open(rt, options, None, None).await?;
    let checked = session
        .rpc("get_entries", json!({}))
        .await
        .and_then(|body| cut.verify(&body, true));
    if let Err(e) = checked {
        let _ = session.close().await;
        return Err(e);
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, parent: Option<&str>, role: &str, stamp: u64) -> Value {
        json!({"type":"message","id":id,"parentId":parent,"message":{"role":role,"timestamp":stamp,"stopReason":"stop","content":"same text"}})
    }

    #[test]
    fn settled_leaf_matches_all_native_inputs_and_the_final_assistant() {
        let mut reply = json!({"leafId":"meta","entries":[
            message("u", Some("prior"), "user", 1), message("steer", Some("u"), "user", 2),
            message("a", Some("steer"), "assistant", 3), {"id":"meta","parentId":"a","type":"compaction"}
        ]});
        assert_eq!(settled_anchor(&reply, &[1, 2], 3).as_deref(), Some("meta"));
        assert!(settled_anchor(&reply, &[1], 3).is_none());
        assert!(settled_anchor(&reply, &[2, 1], 3).is_none());
        assert!(settled_anchor(&reply, &[1, 2], 9).is_none());
        reply["entries"]
            .as_array_mut()
            .unwrap()
            .push(message("elsewhere", None, "user", 1));
        assert!(settled_anchor(&reply, &[1, 2], 3).is_none());
    }

    #[test]
    fn native_child_must_contain_exactly_the_selected_context() {
        let user = message("u", None, "user", 1);
        let answer = message("a", Some("u"), "assistant", 2);
        let next = message("u2", Some("a"), "user", 3);
        let sibling = message("x", Some("u"), "assistant", 4);
        let raw = json!({"leafId":"u2","entries":[user, answer, sibling, next]});
        assert!(Cut::select(&raw, Some("x")).is_err());
        let cut = Cut::select(&raw, Some("a")).unwrap();
        assert_eq!(cut.next_user.as_deref(), Some("u2"));
        let mut child = json!({"leafId":"a","entries":[user,answer]});
        assert!(cut.verify(&child, false).is_ok());
        let hidden = json!({"leafId":"a","entries":[user,answer,sibling]});
        assert!(cut.verify(&hidden, true).is_err());
        assert!(cut.verify(&raw, true).is_err());
        child["entries"][1]["message"]["content"] = json!("different");
        assert!(cut.verify(&child, true).is_err());
        let changed = json!({"leafId":"compact","entries":[user, answer, {"type":"compaction","id":"compact","parentId":"a","summary":"later"}]});
        assert!(Cut::select(&changed, Some("a")).is_err());
        assert!(cut.verify(&changed, true).is_err());
        assert!(
            Cut::select(&changed, Some("compact"))
                .unwrap()
                .next_user
                .is_none()
        );
    }
}
