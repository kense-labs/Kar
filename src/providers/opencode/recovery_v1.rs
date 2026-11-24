//! OpenCode 1's native export uses a user parent for each assistant step.
use super::{Part, Turn, Witness};
use crate::{FailureClass, Outcome, SessionOptions, TurnFailure, chat::Role};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub(super) fn settled(
    doc: &Value,
    root: &str,
    opts: &SessionOptions,
    witness: &Witness<'_>,
    prompt: &str,
    accepted: &[&str],
) -> Option<(Turn, Outcome)> {
    let meta = &doc["info"];
    if meta["id"] != root || !crate::opening::same_directory(meta["directory"].as_str()?, &opts.cwd)
    {
        return None;
    }
    let rows = doc["messages"].as_array()?;
    let witness_index = witness_index(rows, root, witness)?;
    let mut ids = std::collections::HashSet::new();
    let mut users = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if !ids.insert(row["info"]["id"].as_str()?) {
            return None;
        }
        if row["info"]["role"] == "user" {
            users.push(i);
        }
    }
    let witnessed_user = users
        .partition_point(|&i| i < witness_index)
        .checked_sub(1)?;
    let mut resolved = None;
    for start in witnessed_user.saturating_sub(accepted.len())..=witnessed_user {
        let final_user = start + accepted.len();
        if final_user >= users.len() {
            continue;
        }
        let end = users.get(final_user + 1).copied().unwrap_or(rows.len());
        if let Some(turn) = group(&rows[users[start]..end], root, prompt, accepted)
            && resolved.replace(turn).is_some()
        {
            return None;
        }
    }
    resolved
}

fn user_text<'a>(row: &'a Value, root: &str, attachments: bool) -> Option<&'a str> {
    let info = &row["info"];
    let mut text = None;
    for part in row["parts"].as_array()? {
        if part["sessionID"] != root || part["messageID"] != info["id"] {
            return None;
        }
        match part["type"].as_str()? {
            "text" if text.is_none() => text = Some(part["text"].as_str()?),
            "file"
                if attachments
                    && part["mime"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("image/")) => {}
            _ => return None,
        }
    }
    text
}

// An accepted concurrent prompt becomes another native user parent. Follow
// that transition only while the preceding model step is still nonterminal.
fn group(rows: &[Value], root: &str, prompt: &str, accepted: &[&str]) -> Option<(Turn, Outcome)> {
    let (first, steps) = rows.split_first()?;
    if user_text(first, root, false)? != prompt {
        return None;
    }
    let mut user = first["info"]["id"].as_str()?;
    let mut turn = Turn::new(user);
    let mut outcome = None;
    for row in steps {
        if row["info"]["role"] == "user" {
            let expected = accepted.get(turn.steered_users.len())?;
            if outcome.is_some() || user_text(row, root, true)? != *expected {
                return None;
            }
            user = row["info"]["id"].as_str()?;
            turn.steered_users.push(user.to_owned());
            continue;
        }
        let info = &row["info"];
        if outcome.is_some()
            || info["parentID"] != user
            || info["role"] != "assistant"
            || !info["time"]["completed"].is_number()
        {
            return None;
        }
        let id = info["id"].as_str()?;
        append_parts(&mut turn, &row["parts"], root, id)?;
        if !info["error"].is_null() {
            let (status, failure) = native_error(&info["error"])?;
            turn.failure = failure;
            outcome = Some(status);
        } else {
            match info["finish"].as_str()? {
                "stop" => outcome = Some(Outcome::Completed),
                "tool-calls" => {}
                _ => return None,
            }
        }
    }
    (turn.steered_users.len() == accepted.len()).then_some((turn, outcome?))
}

fn witness_index(rows: &[Value], root: &str, witness: &Witness<'_>) -> Option<usize> {
    let mut owner = None;
    for (index, row) in rows.iter().enumerate() {
        let info = &row["info"];
        if info["sessionID"] != root {
            return None;
        }
        if info["role"] != "assistant" {
            continue;
        }
        let matches = identities(row).filter(|key| key == witness).count();
        if matches > 1 {
            return None;
        }
        if matches == 1 && owner.replace(index).is_some() {
            return None;
        }
    }
    owner
}

fn identities(row: &Value) -> impl Iterator<Item = Witness<'_>> {
    let message = row["info"]["id"].as_str().map(Witness::Message);
    message.into_iter().chain(
        row["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| match part["type"].as_str()? {
                "reasoning" => part["id"].as_str().map(Witness::ReasoningPart),
                "tool" => part["callID"].as_str().map(Witness::Tool),
                _ => None,
            }),
    )
}

// A v1 concurrent prompt acknowledgement may still be in flight after its
// assistant output is observed. Establish each native user parent with a
// distinct locally recorded native identity before settling its Kar receipt.
pub(super) fn witnessed_parents<'a>(
    doc: &'a Value,
    local: &'a crate::chat::ChatTurn,
    root: &str,
    turn: &Turn,
) -> bool {
    use crate::chat::Item;
    let mut witnesses = HashMap::new();
    for entry in &local.items {
        if entry.source.session_id.as_deref() != Some(root) || !entry.source.path.is_empty() {
            continue;
        }
        let key = match &entry.item {
            Item::Message {
                role: Role::Assistant,
                provider_id: Some(id),
                ..
            } => Witness::Message(id),
            Item::Message {
                role: Role::Reasoning,
                provider_id: Some(id),
                ..
            } => Witness::ReasoningPart(id),
            Item::Tool { id, .. } => Witness::Tool(id),
            _ => continue,
        };
        witnesses.insert(key, (0usize, 0usize));
    }
    let Some(rows) = doc["messages"].as_array() else {
        return false;
    };
    for (index, row) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r["info"]["role"] == "assistant")
    {
        for key in identities(row) {
            if let Some((count, entry)) = witnesses.get_mut(&key) {
                *count += 1;
                *entry = index;
            }
        }
    }
    let proven: HashSet<_> = witnesses
        .values()
        .filter(|(count, _)| *count == 1)
        .filter_map(|(_, index)| rows[*index]["info"]["parentID"].as_str())
        .collect();
    std::iter::once(&turn.user)
        .chain(&turn.steered_users)
        .all(|id| proven.contains(id.as_str()))
}

fn append_parts(turn: &mut Turn, parts: &Value, root: &str, id: &str) -> Option<()> {
    let mut text_index = None;
    for p in parts.as_array()? {
        if p["sessionID"] != root || p["messageID"] != id {
            return None;
        }
        match p["type"].as_str()? {
            "step-start" | "step-finish" | "patch" | "snapshot" => {}
            "text" => turn.assistant_text(&mut text_index, id, p["text"].as_str()?),
            "reasoning" => turn.parts.push(Part::Text(
                Role::Reasoning,
                p["id"].as_str()?.into(),
                p["text"].as_str()?.into(),
            )),
            "tool" => {
                let call = p["callID"].as_str()?;
                if !matches!(p["state"]["status"].as_str(), Some("completed" | "error"))
                    || turn
                        .tools
                        .insert(call.into(), Some(p["tool"].as_str()?.into()))
                        .is_some()
                {
                    return None;
                }
                turn.parts.push(Part::Tool(call.into()));
            }
            _ => return None,
        }
    }
    Some(())
}

// Match ACP promptResponse's mapping of OpenCode 1 assistant errors.
fn native_error(error: &Value) -> Option<(Outcome, Option<TurnFailure>)> {
    let kind = error["name"].as_str()?;
    let failure = match kind {
        "MessageAbortedError" => return Some((Outcome::Aborted, None)),
        "MessageOutputLengthError" => return Some((Outcome::Completed, None)),
        "ProviderAuthError" => {
            TurnFailure::new(FailureClass::Auth, "provider authentication required")
        }
        "ContentFilterError" => TurnFailure::new(FailureClass::ModelRefused, "refusal"),
        _ => {
            let reason = error["data"]["message"]
                .as_str()
                .unwrap_or("OpenCode prompt failed");
            let status = error["data"]["statusCode"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| (100..600).contains(n));
            let class = match crate::wire::failure::from_status(status) {
                FailureClass::Unknown => crate::failure::prose(reason),
                class => class,
            };
            TurnFailure {
                status,
                ..TurnFailure::new(class, reason)
            }
        }
    };
    Some((Outcome::Failed, Some(failure)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recovery_requires_a_unique_witness_in_a_closed_turn_of_the_original_workspace() {
        let work = tempfile::tempdir().unwrap();
        let opts = SessionOptions::new(work.path());
        let user = json!({"info":{"role":"user","id":"u","sessionID":"s"},"parts":[
            {"type":"text","text":"same","messageID":"u","sessionID":"s"}]});
        let tool = json!({"info":{"role":"assistant","id":"a","parentID":"u","sessionID":"s",
            "time":{"completed":1},"finish":"tool-calls"},"parts":[
            {"type":"tool","tool":"read","callID":"call","id":"part","messageID":"a","sessionID":"s","state":{"status":"completed"}}]});
        let end = json!({"info":{"role":"assistant","id":"b","parentID":"u","sessionID":"s",
            "time":{"completed":2},"finish":"stop"},"parts":[
            {"type":"reasoning","id":"r","messageID":"b","sessionID":"s","text":"thinking"}]});
        let doc = json!({"info":{"id":"s","directory":work.path()},"messages":[user,tool,end]});
        let run = |doc: &Value| settled(doc, "s", &opts, &Witness::Tool("call"), "same", &[]);
        assert!(matches!(run(&doc), Some((_, Outcome::Completed))));
        assert!(matches!(
            settled(&doc, "s", &opts, &Witness::ReasoningPart("r"), "same", &[]),
            Some((_, Outcome::Completed))
        ));
        let mut cut = doc.clone();
        cut["messages"].as_array_mut().unwrap().pop();
        assert!(run(&cut).is_none());
        let mut reused = doc.clone();
        reused["messages"].as_array_mut().unwrap().push(tool);
        assert!(run(&reused).is_none());
        let mut other = doc.clone();
        other["info"]["directory"] = work
            .path()
            .join("another")
            .to_string_lossy()
            .as_ref()
            .into();
        assert!(run(&other).is_none());
        let mut mixed = doc;
        mixed["messages"][2]["info"]["parentID"] = "other".into();
        assert!(run(&mixed).is_none());
    }
}
