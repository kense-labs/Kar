//! OpenCode 2 ACP attaches sessions. Its local HTTP service owns message-boundary forks.
use super::{
    history::{self, Witness},
    server::Server,
};
use crate::{
    Error, Result, Runtime, Session, SessionOptions,
    chat::{ChatTurn, TurnStatus},
};
use reqwest::{Method, StatusCode};
use serde_json::json;

pub(crate) enum Target<'a> {
    Message(&'a str),
    Turn(&'a ChatTurn),
}

/// Only retain the fields needed to locate the inclusive anchor. Attachments stay in their page.
struct Row {
    id: String,
    kind: String,
    outcome: Option<String>,
    matched: bool,
}
struct Boundary<'a> {
    through: &'a str,
    before: Option<&'a str>,
}
fn locate<'a>(rows: &'a [Row], terminal: Option<TurnStatus>) -> Result<Boundary<'a>> {
    let mut positions = rows.iter().enumerate().filter(|(_, row)| row.matched);
    let (mut index, _) = positions.next().ok_or_else(|| {
        Error::Refused("OpenCode history does not contain the selected anchor".into())
    })?;
    if positions.next().is_some() {
        return Err(Error::Refused(
            "OpenCode history contains more than one matching anchor".into(),
        ));
    }
    if let Some(terminal) = terminal {
        let expected = match terminal {
            TurnStatus::Completed => "succeeded",
            TurnStatus::Aborted => "interrupted",
            TurnStatus::Failed => "failed",
            _ => {
                return Err(Error::Refused(
                    "selected turn has no confirmed native outcome".into(),
                ));
            }
        };
        index += rows[index..]
            .iter()
            .take_while(|r| r.kind != "user")
            .position(|r| r.kind == "idle")
            .ok_or_else(|| Error::Refused("OpenCode has no recorded end for this turn".into()))?;
        if rows[index].outcome.as_deref() != Some(expected) {
            return Err(Error::Refused(
                "OpenCode and the saved chat disagree about the selected turn outcome".into(),
            ));
        }
    }
    // Native forks inherit the parent's current instructions, agent, model and location.
    // A later change could otherwise bring future context into the copied conversation.
    if rows[index + 1..].iter().any(|r| {
        matches!(
            r.kind.as_str(),
            "system" | "agent-switched" | "model-switched" | "location-switched"
        )
    }) {
        return Err(Error::Refused(
            "OpenCode changed session settings after the selected boundary".into(),
        ));
    }
    if rows[index].kind != "idle" {
        return Err(Error::Refused(
            "OpenCode needs a native end-of-turn message as the fork anchor".into(),
        ));
    }
    Ok(Boundary {
        through: &rows[index].id,
        before: rows.get(index + 1).map(|r| r.id.as_str()),
    })
}

// OpenCode copies API-managed instructions at fork time, including changes which have
// not reached the transcript yet. The ordinary CLI does not persist their event log.
async fn check_managed_entries(server: &Server, id: &str) -> Result<()> {
    let (status, value) = server
        .call(
            Method::GET,
            &["experimental", "session", id, "instructions", "entries"],
            None,
        )
        .await?;
    match status {
        StatusCode::OK => match value["data"].as_array() {
            Some(entries) if entries.is_empty() => Ok(()),
            Some(_) => Err(Error::Refused(
                "OpenCode has API-managed instructions whose historical values cannot be verified"
                    .into(),
            )),
            None => Err(Error::Protocol(
                "invalid OpenCode instruction entries".into(),
            )),
        },
        _ => Err(Error::Refused(format!(
            "OpenCode could not verify session instructions (HTTP {status})"
        ))),
    }
}

pub(crate) async fn open(
    rt: &Runtime,
    mut opts: SessionOptions,
    source: &str,
    target: Target<'_>,
) -> Result<(Session, String)> {
    let (witness, terminal) = match target {
        Target::Message(id) => (Witness::Message(id), None),
        Target::Turn(turn) => (Witness::for_turn(turn, source)?, Some(turn.status)),
    };
    opts.settings.validate(rt.protocol, &rt.id)?;
    if crate::account::release(&rt.launch_version(&opts).await?).is_none_or(|v| v.0 != 2) {
        return Err(Error::Unsupported(
            "message-boundary forks require OpenCode 2".into(),
        ));
    }
    let cwd = opts.cwd.canonicalize()?;
    let make = async {
        let (server, source_info) = history::open(&rt.program, &opts, source).await?;
        let mut rows = Vec::new();
        history::visit(&server, source, |row| {
            let id = row["id"].as_str().filter(|s| !s.is_empty());
            let kind = row["type"].as_str();
            let (Some(id), Some(kind)) = (id, kind) else {
                return Err(Error::Protocol("invalid OpenCode history item".into()));
            };
            rows.push(Row {
                id: id.into(),
                kind: kind.into(),
                outcome: row["outcome"].as_str().map(str::to_owned),
                matched: witness.matches(row),
            });
            Ok(())
        })
        .await?;
        let cut = locate(&rows, terminal)?;
        let payload = cut
            .before
            .map_or_else(|| json!({}), |id| json!({"before":id}));
        check_managed_entries(&server, source).await?;
        let result = server
            .request(Method::POST, &["session", source, "fork"], Some(payload))
            .await?;
        let info = &result["data"];
        let child = info["id"]
            .as_str()
            .filter(|id| !id.is_empty() && *id != source)
            .ok_or_else(|| {
                Error::Protocol("OpenCode did not create a distinct child session".into())
            })?;
        let expected = match cut.before {
            Some(id) => json!({"type":"before","messageID":id}),
            None => json!({"type":"through","messageID":cut.through}),
        };
        if info["fork"]["sessionID"] != source || info["fork"]["boundary"] != expected {
            return Err(Error::Refused(
                "OpenCode copied a different history boundary".into(),
            ));
        }
        if ["agent", "model", "permissions", "location"]
            .iter()
            .any(|field| source_info["data"].get(*field) != info.get(*field))
        {
            return Err(Error::Refused(
                "OpenCode session settings changed while copying the conversation".into(),
            ));
        }
        history::check_workspace(info, &cwd)?;
        check_managed_entries(&server, child).await?;
        server.stop().await;
        Ok((child.to_owned(), cut.through.to_owned()))
    };
    let (child, anchor) = tokio::time::timeout(opts.handshake_timeout, make)
        .await
        .map_err(|_| Error::Timeout("OpenCode session fork".into()))??;
    opts.resume = Some(child.clone());
    let session = rt.open_selected(opts).await?;
    if session.id() != child {
        session.close().await?;
        return Err(Error::Protocol(
            "OpenCode attached a different child session".into(),
        ));
    }
    Ok((session, anchor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, kind: &str, matched: bool) -> Row {
        Row {
            id: id.into(),
            kind: kind.into(),
            outcome: (kind == "idle").then(|| "succeeded".into()),
            matched,
        }
    }

    #[test]
    fn missing_end_cannot_match_the_next_turn() {
        let rows = [
            row("answer-one", "assistant", true),
            row("next-input", "user", false),
            row("end-two", "idle", false),
        ];
        assert!(locate(&rows, Some(TurnStatus::Completed)).is_err());
    }

    #[test]
    fn rejects_ambiguous_or_changed_context() {
        let mut rows = [
            row("answer-one", "assistant", true),
            row("end-one", "idle", false),
            row("answer-two", "assistant", true),
        ];
        assert!(locate(&rows, Some(TurnStatus::Completed)).is_err());
        rows[2] = row("changed-model", "model-switched", false);
        assert!(locate(&rows, Some(TurnStatus::Completed)).is_err());
        rows[2] = row("next-input", "user", false);
        let boundary = locate(&rows, Some(TurnStatus::Completed)).unwrap();
        assert_eq!(boundary.through, "end-one");
        assert_eq!(boundary.before, Some("next-input"));
        assert!(locate(&rows, Some(TurnStatus::Failed)).is_err());
    }
}
