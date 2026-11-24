//! Kimi Agent Protocol over an owned loopback Web process.
mod actor;
mod projection;
mod startup;
mod web;
pub(crate) use projection::{tool_snapshot, turn_outcome};
pub(crate) use web::{Web as QueryWeb, segment as workspace_segment};

use crate::{Error, Protocol, Result, Runtime, Session, SessionOptions, stream::Journal};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;
use web::{Socket, Web, segment};

pub(crate) fn kap_candidate(v: &semver::Version) -> bool {
    v >= &semver::Version::new(2, 7, 3)
        || (v.pre.as_str().starts_with("botiverse.")
            && v >= &semver::Version::parse("2.7.2-botiverse.1").unwrap())
}

/// A host can die after the HTTP request arrives but before the native turn ID is saved.
pub(crate) fn recovery_turn<'a>(
    turns: &'a Value,
    local: &crate::chat::ChatTurn,
) -> Option<(&'a Value, bool)> {
    let key = local.id.to_string();
    let mut candidates = turns.as_array()?.iter().filter_map(|turn| {
        let native = turn["turn_id"].as_str().filter(|id| !id.is_empty())?;
        if turn["kind"] == "system" {
            return None;
        }
        let claimed = !local.native_continuation
            && turn["kind"] == "agent"
            && turn["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "user_message"
                        && item["placement"] == "opener"
                        && item["origin"]["kind"] == "user"
                        && item["prompt_ids"]
                            .as_array()
                            .is_some_and(|ids| matches!(ids.as_slice(),[id] if id==&key))
                })
            });
        let matches = local
            .native_turn_id
            .as_deref()
            .map_or(claimed, |id| id == native);
        matches.then_some((turn, claimed))
    });
    let (turn, claimed) = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    let needs_receipt = !local.input_sent || local.native_turn_id.is_none();
    (turn["status"] == "ended" || (claimed && needs_receipt)).then_some((turn, claimed))
}

pub(crate) async fn open(
    rt: &Runtime,
    mut opts: SessionOptions,
    allow_acp: bool,
    recover_turn: Option<&crate::chat::ChatTurn>,
) -> Result<Session> {
    if opts
        .resume
        .as_deref()
        .is_some_and(|id| id.is_empty() || id == "." || id == "..")
    {
        return Err(Error::Refused("invalid Kimi resume id".into()));
    }
    opts.cwd = opts.cwd.canonicalize()?;
    if !opts.cwd.is_dir() {
        return Err(Error::Refused("Kimi needs a workspace directory".into()));
    }
    if web::candidate(rt, &opts).await?
        && let Some(mut web) = Web::start(rt, &opts).await?
    {
        let mut workspace = None;
        match negotiate(&mut web, &opts, &mut workspace).await {
            Ok(Some((socket, id, initial))) => {
                let journal = Journal::new(opts.retained_bytes)?;
                journal.bind_root(&id);
                let info = json!({"sessionId":id,"protocol":"kap"});
                let (commands, rx) = tokio::sync::mpsc::channel(64);
                let process_root = web.process_root();
                let mut actor = actor::Actor::new(web, socket, id.clone(), journal.clone(), &opts);
                for frame in initial {
                    if let Some(turn) = recover_turn {
                        actor.capture_recovery(&frame, turn);
                    }
                    if let Err(e) = actor.frame(frame).await {
                        let _ = actor.close().await;
                        return Err(e);
                    }
                }
                tokio::spawn(actor.run(rx));
                let mut session =
                    Session::remote(Protocol::KimiKap, opts, commands, journal, id, info);
                session.process_root = process_root;
                return Ok(session);
            }
            result => {
                if let Some(ws) = workspace {
                    let _ = web
                        .request(
                            Method::DELETE,
                            &format!("/workspaces/{}", segment(&ws)),
                            None,
                        )
                        .await;
                }
                let _ = web.stop().await;
                result?;
            }
        }
    }
    if !allow_acp {
        return Err(Error::Unsupported(
            "the saved Kimi session requires KAP".into(),
        ));
    }
    let mut fallback = rt.clone();
    fallback.protocol = Protocol::Acp;
    fallback.args.insert(0, "acp".into());
    Session::open(&fallback, opts, None, None).await
}

async fn negotiate(
    web: &mut Web,
    opts: &SessionOptions,
    owned_workspace: &mut Option<String>,
) -> Result<Option<(Socket, String, Vec<Value>)>> {
    let meta = web.request(Method::GET, "/meta", None).await?;
    if ![
        "websocket",
        "tasks",
        "session_ephemeral_mcp",
        "session_append_system_prompt",
        "steer_parent_correlation",
    ]
    .iter()
    .all(|k| meta["capabilities"][*k] == true)
    {
        return Ok(None);
    }
    opts.settings.validate(Protocol::KimiKap, "kimi")?;
    if opts.retained_bytes == Some(0) {
        return Err(Error::Refused("retained_bytes must be positive".into()));
    }
    let mut secrets = crate::options::session_secrets(opts);
    secrets.push(web.token.clone());
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let auth = web.request(Method::GET, "/auth", None).await?;
    if auth["models_ready"] != true {
        let mut failure = crate::TurnFailure::new(
            crate::FailureClass::Auth,
            auth["message"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("Kimi has no configured model credentials"),
        );
        failure.credential = Some(crate::CredentialProblem::Missing);
        return Err(crate::OpenError::RuntimeFailure {
            failure,
            code: None,
            cause: None,
        }
        .redacted(&secrets));
    }
    let mut config = json!({"permission_mode":"manual"});
    if let Some(model) = &opts.model {
        config["model"] = model.clone().into();
    }
    if let Some(e) = &opts.settings.effort {
        config["thinking"] = e.clone().into();
    }
    let mut body = json!({"agent_config":config});
    if let Some(text) = &opts.settings.append_system_prompt {
        body["append_system_prompt"] = text.clone().into();
    }
    let mut servers = serde_json::Map::new();
    for (name, server) in &opts.mcp_servers {
        if name.is_empty() || crate::options::instructions::reserved_server(name) {
            return Err(Error::Unsupported("reserved Kimi MCP name".into()));
        }
        let mut entry = server.vendor_config()?;
        entry["transport"] = entry.as_object_mut().unwrap().remove("type").unwrap();
        servers.insert(name.clone(), entry);
    }
    // KAP connections belong to this open. An empty map clears prior ephemeral credentials on resume.
    body["mcp_servers"] = servers.into();
    let dirs = opts.settings.directories(&opts.cwd)?;
    let path = if let Some(id) = &opts.resume {
        if !dirs.is_empty() {
            return Err(Error::Unsupported(
                "Kimi accepts additional directories only at creation".into(),
            ));
        }
        let path = format!("/sessions/{}", segment(id));
        let existing = web
            .session_request(Method::GET, &path, None, Some((opts, &secrets)))
            .await?;
        if existing["archived"] == true {
            return Err(crate::OpenError::SessionUnavailable {
                session_id: id.clone(),
                message: "Kimi session is archived".into(),
                reason: crate::UnavailableReason::Archived,
                cause: crate::NativeCause {
                    method: "GET /sessions/{id}".into(),
                    native: json!({"id":existing["id"],"archived":true}),
                },
            }
            .redacted(&secrets));
        }
        if let Some(saved) = existing["cwd"].as_str()
            && !crate::opening::same_directory(saved, &opts.cwd)
        {
            return Err(crate::OpenError::cwd(
                id,
                &opts.cwd,
                crate::ResumeCwdReason::OtherCwd {
                    saved_cwd: saved.into(),
                },
            )
            .redacted(&secrets));
        }
        format!("{path}:resume")
    } else {
        if dirs.is_empty() {
            body["cwd"] = json!(opts.cwd);
        } else {
            let (id, owned) = web.workspace(&opts.cwd, Some(&dirs)).await?;
            if owned {
                *owned_workspace = Some(id.clone());
            }
            body["workspace_id"] = id.into();
        }
        "/sessions".into()
    };
    let created = web
        .session_request(Method::POST, &path, Some(body), Some((opts, &secrets)))
        .await?;
    let id = created["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Protocol("missing Kimi session id".into()))?;
    if opts
        .resume
        .as_deref()
        .is_some_and(|expected| expected != id)
    {
        return Err(Error::Protocol("Kimi resumed a different session".into()));
    }
    let mut socket = tokio::time::timeout(opts.handshake_timeout, web.socket())
        .await
        .map_err(|_| Error::Timeout("Kimi socket connection".into()))??;
    let request_id = Uuid::new_v4().to_string();
    web::send(
        &mut socket,
        &json!({"type":"subscribe","request_id":request_id,"session_id":id,"agent_ids":["main"]}),
    )
    .await?;
    let frames = tokio::time::timeout(opts.handshake_timeout, async {
        let (mut ack, mut agent, mut turns) = (false, false, false);
        let mut frames = vec![];
        while !(ack && agent && turns) {
            let v = web::read(&mut socket).await?;
            if v.get("session_id").is_some_and(|s| s != id) {
                return Err(Error::Protocol("Kimi sent another session's frame".into()));
            }
            if v["type"] == "response" && v["request_id"] == request_id {
                if v["code"] != 0 {
                    return Err(Error::Refused("Kimi refused the main subscription".into()));
                }
                ack = true;
                continue;
            }
            if v["agent_id"].as_str().unwrap_or("main") == "main" {
                match v["type"].as_str() {
                    Some("agent") => {
                        if let Some(wanted) = &opts.model
                            && v["model"] != wanted.as_str()
                        {
                            return Err(crate::OpenError::model_confirmation(
                                format!("Kimi did not confirm model {wanted:?}"),
                                wanted,
                                v["model"].as_str(),
                            )
                            .redacted(&secrets));
                        }
                        if opts
                            .settings
                            .effort
                            .as_deref()
                            .is_some_and(|s| v["thinking"] != s)
                        {
                            return Err(Error::Refused("Kimi did not confirm thinking".into()));
                        }
                        agent = true;
                    }
                    Some("turns") if v["turns"].is_array() => turns = true,
                    Some("turns.failed") => {
                        return Err(Error::Refused("Kimi failed to load session history".into()));
                    }
                    _ => {}
                }
            }
            frames.push(v);
        }
        Ok::<_, Error>(frames)
    })
    .await
    .map_err(|_| Error::Timeout("Kimi initial subscription".into()))??;
    Ok(Some((socket, id.into(), frames)))
}
