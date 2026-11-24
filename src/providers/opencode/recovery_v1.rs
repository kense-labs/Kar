//! OpenCode 1's native export uses a user parent for each assistant step.
use super::{Part, Turn, Witness};
use crate::{FailureClass, Outcome, SessionOptions, TurnFailure, chat::Role};
use serde_json::Value;
pub(super) fn settled(
    doc: &Value,
    root: &str,
    opts: &SessionOptions,
    witness: &Witness<'_>,
    prompt: &str,
) -> Option<(Turn, Outcome)> {
    let meta = &doc["info"];
    if meta["id"] != root || !crate::opening::same_directory(meta["directory"].as_str()?, &opts.cwd)
    {
        return None;
    }
    let rows = doc["messages"].as_array()?;
    let (user, witness_index) = owner(rows, root, witness)?;
    let mut inputs = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r["info"]["id"] == user);
    let (index, entry) = inputs.next()?;
    if inputs.next().is_some() || entry["info"]["role"] != "user" {
        return None;
    }
    let [text] = entry["parts"].as_array()?.as_slice() else {
        return None;
    };
    if text["type"] != "text"
        || text["text"] != prompt
        || text["messageID"] != user
        || text["sessionID"] != root
    {
        return None;
    }

    let next_user = rows[index + 1..]
        .iter()
        .position(|row| row["info"]["role"] == "user")
        .map_or(rows.len(), |i| index + 1 + i);
    let steps = index + 1..next_user;
    if !steps.contains(&witness_index) {
        return None;
    }
    let mut turn = Turn::new(user);
    let mut outcome = None;
    let mut message_ids = std::collections::HashSet::new();
    for row in &rows[steps] {
        let info = &row["info"];
        if outcome.is_some()
            || info["parentID"] != user
            || info["role"] != "assistant"
            || !info["time"]["completed"].is_number()
        {
            return None;
        }
        let id = info["id"].as_str()?;
        if !message_ids.insert(id) {
            return None;
        }
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
    Some((turn, outcome?))
}

fn owner<'a>(rows: &'a [Value], root: &str, witness: &Witness<'_>) -> Option<(&'a str, usize)> {
    let mut owner = None;
    for (index, row) in rows.iter().enumerate() {
        let info = &row["info"];
        if info["sessionID"] != root {
            return None;
        }
        if info["role"] != "assistant" {
            continue;
        }
        let matches = match witness {
            Witness::Message(id) => usize::from(info["id"].as_str() == Some(*id)),
            Witness::ReasoningPart(id) => row["parts"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "reasoning" && part["id"].as_str() == Some(*id))
                .count(),
            Witness::Tool(id) => row["parts"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "tool" && part["callID"].as_str() == Some(*id))
                .count(),
        };
        if matches > 1 {
            return None;
        }
        if matches == 1 && owner.replace((info["parentID"].as_str()?, index)).is_some() {
            return None;
        }
    }
    owner
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
            TurnFailure::new(crate::failure::prose(reason), reason)
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
        let run = |doc: &Value| settled(doc, "s", &opts, &Witness::Tool("call"), "same");
        assert!(matches!(run(&doc), Some((_, Outcome::Completed))));
        assert!(matches!(
            settled(&doc, "s", &opts, &Witness::ReasoningPart("r"), "same"),
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
