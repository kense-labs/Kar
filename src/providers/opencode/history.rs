//! OpenCode 2's native message archive and authenticated per-workspace service.
use super::server::Server;
use crate::{
    Error, InstallationOptions, Result, SessionOptions,
    chat::{ChatTurn, Item, Role},
    installation::Probe,
};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::{collections::HashSet, path::Path};

pub(crate) enum Witness<'a> {
    Message(&'a str),
    Tool(&'a str),
    ReasoningPart(&'a str),
}

impl<'a> Witness<'a> {
    pub(crate) fn for_turn(turn: &'a ChatTurn, source: &str) -> Result<Self> {
        let mut fallback = None;
        for entry in
            turn.items.iter().rev().filter(|e| {
                e.source.session_id.as_deref() == Some(source) && e.source.path.is_empty()
            })
        {
            match &entry.item {
                Item::Message {
                    role: Role::Assistant,
                    provider_id: Some(id),
                    ..
                } => return Ok(Self::Message(id)),
                Item::Message {
                    role: Role::Reasoning,
                    provider_id: Some(id),
                    ..
                } => {
                    if let Some((id, n)) = id.rsplit_once(":reasoning:")
                        && n.parse::<u64>().is_ok()
                    {
                        fallback = Some(Self::Message(id));
                    } else if fallback.is_none() {
                        fallback = Some(Self::ReasoningPart(id));
                    }
                }
                Item::Tool { id, .. } if fallback.is_none() => fallback = Some(Self::Tool(id)),
                _ => {}
            }
        }
        fallback.ok_or_else(|| {
            Error::Unsupported("this turn has no native OpenCode message or tool identifier".into())
        })
    }
    pub(super) fn matches(&self, row: &Value) -> bool {
        match self {
            Self::ReasoningPart(_) => false, // OpenCode 2 scopes reasoning to assistant message IDs.
            Self::Message(id) => row["id"].as_str() == Some(*id),
            Self::Tool(id) => {
                row["type"] == "assistant"
                    && row["content"].as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|p| p["type"] == "tool" && p["id"].as_str() == Some(*id))
                    })
            }
        }
    }
}

pub(super) async fn visit(
    server: &Server,
    source: &str,
    mut receive: impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
    let mut cursors = HashSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = cursor
            .as_deref()
            .map_or(("order", "asc"), |c| ("cursor", c));
        let (status, value) = server
            .call_query(
                Method::GET,
                &["session", source, "message"],
                &[("limit", "32"), page],
                None,
            )
            .await?;
        if status != StatusCode::OK {
            return Err(Error::Refused(format!(
                "OpenCode could not read the source history (HTTP {status})"
            )));
        }
        let data = value["data"]
            .as_array()
            .ok_or_else(|| Error::Protocol("invalid OpenCode message page".into()))?;
        for row in data {
            receive(row)?;
        }
        let page = value["cursor"]
            .as_object()
            .ok_or_else(|| Error::Protocol("missing OpenCode pagination information".into()))?;
        let Some(next) = page.get("next").filter(|next| !next.is_null()) else {
            return Ok(());
        };
        let next = next
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("invalid OpenCode page cursor".into()))?;
        if !cursors.insert(next.to_owned()) {
            return Err(Error::Protocol(
                "OpenCode repeated its message page cursor".into(),
            ));
        }
        cursor = Some(next.to_owned());
    }
}

pub(super) fn check_workspace(info: &Value, cwd: &Path) -> Result<()> {
    let dir = info.pointer("/location/directory").and_then(Value::as_str);
    if !dir.is_some_and(|path| crate::opening::same_directory(path, cwd)) {
        return Err(Error::Refused(
            "OpenCode session belongs to a different workspace".into(),
        ));
    }
    Ok(())
}

pub(super) async fn open(
    program: &Path,
    opts: &SessionOptions,
    source: &str,
) -> Result<(Server, Value)> {
    let cwd = opts.cwd.canonicalize()?;
    let probe = Probe::new(&InstallationOptions {
        cwd: Some(cwd.clone()),
        env: opts
            .env
            .iter()
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .collect(),
    })?;
    let server = Server::launch(&probe, program, opts.handshake_timeout, Some(&cwd)).await?;
    let info = server
        .request(Method::GET, &["session", source], None)
        .await?;
    if info["data"]["id"] != source {
        return Err(Error::Protocol(
            "OpenCode returned a different source session".into(),
        ));
    }
    check_workspace(&info["data"], &cwd)?;
    Ok((server, info))
}
