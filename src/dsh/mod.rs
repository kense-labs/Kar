//! DeepSeek Harness's authenticated native Web API. One loopback host per controller.
mod actor;
mod compaction;
mod goals;
mod images;
pub(crate) mod mcp_bridge;
pub use mcp_bridge::run_dsh_mcp_bridge;
pub(crate) mod profile;
mod projection;
pub(crate) mod recovery;
mod web;

use crate::{Error, Event, Protocol, Result, Runtime, Session, SessionOptions, stream::Journal};
use serde_json::{Value, json};
use web::{Socket, Web};

pub(crate) fn uses_web(version: &semver::Version) -> bool {
    version >= &semver::Version::parse("0.2.2-rc.5").unwrap()
}

pub(crate) fn directory_refusal(version: Option<&semver::Version>) -> Option<&'static str> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        Some("DSH session-scoped writable roots for file and shell tools require macOS or Linux")
    } else if version.is_some_and(|v| v < &semver::Version::parse("0.2.2-rc.3").unwrap()) {
        Some("DSH session-scoped writable roots require 0.2.2-rc.3 or later")
    } else {
        None
    }
}

pub(crate) async fn release(
    rt: &Runtime,
    opts: &SessionOptions,
    known: Option<&str>,
) -> Result<semver::Version> {
    let required = || {
        Error::Unsupported(
            "session_log_upload requires verified DeepSeek Harness 0.2.1-rc.5 or later".into(),
        )
    };
    let observed;
    let raw = match known {
        Some(version) => version,
        None => {
            observed = rt.launch_version(opts).await.map_err(|e| {
                if opts.settings.session_log_upload.is_some() {
                    required()
                } else {
                    e
                }
            })?;
            &observed
        }
    };
    let parsed = crate::installation::read_version("dsh", raw);
    if !opts.settings.additional_directories.is_empty()
        && let Some(reason) = directory_refusal(parsed.as_ref())
    {
        return Err(Error::Unsupported(reason.into()));
    }
    if opts.settings.session_log_upload.is_some()
        && parsed
            .as_ref()
            .is_none_or(|v| v < &semver::Version::parse("0.2.1-rc.5").unwrap())
    {
        return Err(required());
    }
    parsed.ok_or_else(|| Error::Protocol("DSH reported an unknown version".into()))
}

pub(crate) async fn open(
    rt: &Runtime,
    mut opts: SessionOptions,
    fork: Option<(&str, Option<&str>)>,
    recover: Option<&crate::chat::ChatTurn>,
    version: Option<&str>,
) -> Result<Session> {
    if opts.workbench_registration.is_some() {
        let release = release(rt, &opts, version)
            .await
            .ok()
            .map(|v| v.to_string());
        opts.add_dsh_workbench(release.as_deref());
    }
    opts.cwd = opts.cwd.canonicalize()?;
    if opts.cwd.to_str().is_none() {
        return Err(Error::Unsupported(
            "DSH requires a UTF-8 workspace path".into(),
        ));
    }
    opts.settings.validate(Protocol::Dsh, &rt.id)?;
    if !rt.args.is_empty() {
        return Err(Error::Unsupported("extra DSH launch arguments".into()));
    }

    let route = opts
        .model
        .as_deref()
        .map(|m| {
            m.split_once('/')
                .filter(|(p, m)| !p.is_empty() && !m.is_empty())
                .ok_or_else(|| Error::Refused("DSH expects a provider/model route".into()))
        })
        .transpose()?;
    // The Web UI normally persists model selections as a global preference. A
    // private invocation overlay confines model selection to this native session.
    let rows = vec![
        json!({"id":"config-editor","disabled":true}),
        json!({"id":"session-title-llm","disabled":true}),
    ];
    let mut web = Web::start(rt, &opts, rows, version).await?;
    let journal = rt.session_journal(opts.retained_bytes)?;
    let ready = tokio::time::timeout(
        opts.handshake_timeout,
        negotiate(&mut web, &opts, route, &journal, fork, recover),
    )
    .await;
    let (id, ws, baseline, client) =
        match ready.unwrap_or_else(|_| Err(Error::Timeout("DSH initial subscriptions".into()))) {
            Ok(v) => v,
            Err(e) => {
                let _ = web.stop().await;
                return Err(e);
            }
        };
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let file = web.file.clone();
    let process_root = web.process_root();
    let selected = &baseline["projections"]["values"]["modelSelection"];
    let fallback = route
        .map(|(p, m)| json!({"provider":p,"model":m}))
        .or_else(|| {
            selected["next"]
                .is_object()
                .then(|| selected["next"].clone())
        })
        .unwrap_or_else(|| selected["lastUsed"].clone());
    let limits = web.images().image_limits(&id, &fallback).await;
    let mut actor = actor::Actor::new(
        web,
        ws,
        id.clone(),
        &baseline,
        client,
        &opts,
        journal.clone(),
    );
    actor.image_fallback = fallback;
    if opts.settings.goals {
        let ready = tokio::time::timeout(opts.handshake_timeout, actor.initialize_goals()).await;
        if let Err(e) =
            ready.unwrap_or_else(|_| Err(Error::Timeout("DSH goal service activation".into())))
        {
            actor.close_web().await;
            return Err(e);
        }
    }
    if let Err(error) = actor.confirm_root_settings(&opts, &baseline).await {
        actor.close_web().await;
        return Err(error);
    }
    tokio::spawn(actor.run(rx));
    let mut session = Session::remote(
        Protocol::Dsh,
        opts,
        tx,
        journal,
        id.clone(),
        json!({"sessionId":id,"imageTypes":limits.media_types,"imageLimits":limits}),
    );
    session.image_input = !limits.media_types.is_empty();
    session.process_root = process_root;
    session.dsh_file = Some(file);
    Ok(session)
}
async fn negotiate(
    web: &mut Web,
    opts: &SessionOptions,
    route: Option<(&str, &str)>,
    journal: &Journal,
    fork: Option<(&str, Option<&str>)>,
    recover: Option<&crate::chat::ChatTurn>,
) -> Result<(String, Socket, Value, String)> {
    let mut ws = web.socket().await?;
    web::subscribe(&mut ws, "interactions", "$events", json!({})).await?;
    let ready = web::read(&mut ws).await?;
    let client = ready["value"]["clientId"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && ready["type"] == "item"
                && ready["streamId"] == "interactions"
                && ready["value"]["type"] == "ready"
        })
        .ok_or_else(|| Error::Protocol("DSH event channel did not become ready".into()))?
        .to_owned();
    let id = if let Some((source, anchor)) = fork {
        let snapshot = follow(&mut ws, "source", source, opts, false).await?;
        let mut request = json!({"sessionId":source,"allowMigration":false});
        if let Some(anchor) = anchor {
            let seq = anchor
                .parse::<u64>()
                .map_err(|_| Error::Refused("invalid DSH fork sequence".into()))?;
            if !snapshot["cursor"].as_u64().is_some_and(|last| seq <= last) {
                return Err(Error::Refused(
                    "DSH fork cut is outside the source session".into(),
                ));
            }
            let page=web.request("session/events",json!({"address":{"kind":"session","sessionId":source},"throughSeq":seq,"maxMessages":1})).await?;
            if !page["records"].as_array().into_iter().flatten().any(|row| {
                row["event"]["seq"].as_u64() == Some(seq) && row["event"]["type"] == "turn/end"
            }) {
                return Err(Error::Refused(
                    "DSH fork cut must identify a native turn ending".into(),
                ));
            }
            request["atSeq"] = seq.into();
        }
        web::cancel(&mut ws, "source").await?;
        if let Some(binding) = web.patch.agent_binding() {
            binding.begin()?;
        }
        let response = web.request("session/fork", request).await?;
        response["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty() && *id != source)
            .ok_or_else(|| Error::Protocol("DSH fork omitted the child session id".into()))?
            .to_owned()
    } else if let Some(id) = &opts.resume {
        id.clone()
    } else {
        if let Some(binding) = web.patch.agent_binding() {
            binding.begin()?;
        }
        let r = web
            .request("session/create", json!({"cwd":opts.cwd}))
            .await?;
        r["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("missing DSH session identity".into()))?
            .to_owned()
    };
    if let Some(binding) = web.patch.agent_binding() {
        binding.select(&id)?;
    }
    let snapshot = follow(&mut ws, "root", &id, opts, recover.is_some()).await?;
    journal.bind_root(&id);
    if let Some(turn) = recover {
        recovery::capture(
            web,
            &id,
            &snapshot,
            turn,
            journal,
            &crate::options::environment_secrets(&opts.env),
        )
        .await?;
    }
    let current = &snapshot["projections"]["values"]["modelSelection"];
    let selected = current
        .get("next")
        .filter(|v| v.is_object())
        .unwrap_or(&current["lastUsed"]);
    if route.is_some() || opts.settings.effort.is_some() {
        let default;
        let (provider, model) = if let Some(route) = route {
            route
        } else {
            default = if selected["model"].is_string() {
                selected.clone()
            } else {
                web.call("session/modelCatalog", json!({})).await?["default"].clone()
            };
            (
                default["provider"]
                    .as_str()
                    .ok_or_else(|| Error::Refused("DSH omitted the default provider".into()))?,
                default["model"]
                    .as_str()
                    .ok_or_else(|| Error::Refused("DSH omitted the default model".into()))?,
            )
        };
        let mut req = json!({"sessionId":id,"provider":provider,"model":model});
        if let Some(e) = &opts.settings.effort {
            req["reasoningEffort"] = e.clone().into();
        }
        let r = web.request("session/selectModel", req).await?;
        let selected = &r["selected"];
        let reported = selected["provider"]
            .as_str()
            .zip(selected["model"].as_str())
            .map(|(p, m)| format!("{p}/{m}"));
        opts.confirm_model("DSH", reported.as_deref())?;
        if r["selected"]["provider"] != provider
            || r["selected"]["model"] != model
            || opts
                .settings
                .effort
                .as_ref()
                .is_some_and(|s| r["selected"]["reasoningEffort"] != *s)
        {
            return Err(Error::Refused(
                "DSH did not confirm the model selection".into(),
            ));
        }
        journal.host_event(Event::Model {
            model: format!("{provider}/{model}"),
        });
        if let Some(e) = r["selected"]["reasoningEffort"].as_str() {
            journal.host_event(Event::Effort { effort: e.into() });
        }
    }
    if opts.resume.is_some() || opts.settings.goals {
        // Preserve the passive follow cursor while promoting the validated workspace.
        web::subscribe(
            &mut ws,
            "activate",
            "session/follow",
            json!({"request":{
                "address":{"kind":"session","sessionId":id},"activate":true,"maxMessages":1
            }}),
        )
        .await?;
    }
    Ok((id, ws, snapshot, client))
}
pub(super) fn turn_id(v: &Value) -> Option<String> {
    v.as_u64().map(|n| n.to_string())
}

async fn follow(
    ws: &mut Socket,
    stream: &str,
    id: &str,
    opts: &SessionOptions,
    recovering: bool,
) -> Result<Value> {
    let mut request = json!({"address":{"kind":"session","sessionId":id},"assistantStream":true});
    if stream == "root" && (profile::agent_controls(&opts.settings) || opts.settings.name.is_some())
    {
        request["activate"] = true.into();
    }
    // Usually the interrupted turn is the newest. Start with that turn's
    // window, then page backwards at its cursor only if the request is older.
    if recovering {
        request["turnWindow"] = json!({"minMessages":1,"minTurns":1});
    }
    web::subscribe(ws, stream, "session/follow", json!({"request":request})).await?;
    loop {
        let f = web::read(ws).await?;
        if f["streamId"] != stream {
            continue;
        }
        let v = &f["value"];
        if f["type"] != "item" || v["type"] != "snapshot" {
            if (opts.resume.as_deref() == Some(id) || stream == "source")
                && f["type"] == "error"
                && f["error"]["code"] == "session/not-found"
                && f["error"]["details"]["sessionId"] == id
            {
                return Err(crate::OpenError::SessionNotFound {
                    session_id: id.into(),
                    message: f["error"]["message"]
                        .as_str()
                        .unwrap_or("DSH session was not found")
                        .into(),
                    cause: crate::NativeCause {
                        method: "session/follow".into(),
                        native: f["error"].clone(),
                    },
                }
                .redacted(&crate::options::session_secrets(opts)));
            }
            return Err(Error::Refused("DSH could not open session history".into()));
        }
        if v["header"]["id"] != id {
            return Err(Error::Protocol(
                "DSH returned another session's header".into(),
            ));
        }
        let cwd = v["header"]["cwd"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("DSH session header omitted its directory".into()))?;
        if !crate::opening::same_directory(cwd, &opts.cwd) {
            return Err(crate::OpenError::cwd(
                id,
                &opts.cwd,
                crate::ResumeCwdReason::OtherCwd {
                    saved_cwd: cwd.into(),
                },
            )
            .redacted(&crate::options::session_secrets(opts)));
        }
        return Ok(v.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn log_upload_requires_a_supported_version_and_runtime() {
        let mut opts = SessionOptions::new(".");
        opts.settings.session_log_upload = Some(crate::SessionLogUpload::Off);
        let rt = Runtime::new("dsh", "not-a-dsh-executable", &[], Protocol::Dsh);
        for value in [Some("0.2.1-rc.4"), Some("unknown"), None] {
            assert!(matches!(
                release(&rt, &opts, value).await,
                Err(Error::Unsupported(_))
            ));
        }
        assert!(release(&rt, &opts, Some("dsh 0.2.1-rc.5")).await.is_ok());
        for (protocol, id) in [
            (Protocol::Claude, "claude"),
            (Protocol::Acp, "opencode"),
            (Protocol::Qwen, "qwen"),
        ] {
            assert!(matches!(
                opts.settings.validate(protocol, id),
                Err(Error::Unsupported(_))
            ));
        }
    }
}
