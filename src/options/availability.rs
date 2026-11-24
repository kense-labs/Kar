//! Static option refusals shared by the launcher and the pre-open declaration.
use super::SessionSettings;
use crate::{Error, Protocol, Result};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
struct Setting {
    name: &'static str,
    allowed: bool,
    reason: &'static str,
    supplied: fn(&SessionSettings) -> bool,
}

fn rules(protocol: Protocol, id: &str) -> [Setting; 15] {
    use Protocol::*;
    let core = matches!(protocol, Codex | Claude | Pi | Qwen | Qoder);
    let acp = protocol == Acp;
    let grok = acp && id == "grok";
    let buddy = acp && id == "codebuddy";
    let opencode = acp && id == "opencode";
    let dsh = id == "dsh" && matches!(protocol, Dsh | Acp);
    let rule = |name, allowed, reason, supplied| Setting {
        name,
        allowed,
        reason,
        supplied,
    };
    [
        rule(
            "system_prompt",
            core || protocol == Factory || grok || buddy || opencode || dsh,
            "system prompt replacement on this adapter",
            |s| s.system_prompt.is_some(),
        ),
        rule(
            "append_system_prompt",
            core || matches!(protocol, Factory | KimiKap)
                || grok
                || opencode
                || dsh
                || (acp && id == "antigravity"),
            "system prompt append on this adapter",
            |s| s.append_system_prompt.is_some(),
        ),
        rule(
            "pi_project_trusted",
            protocol == Pi,
            "Pi project trust on this adapter",
            |s| s.pi_project_trusted.is_some(),
        ),
        rule(
            "disallowed_tools",
            core || buddy || dsh,
            "native tool exclusions on this adapter",
            |s| !s.disallowed_tools.is_empty(),
        ),
        rule(
            "end_turn_processes_on_abort",
            protocol == Codex || grok,
            "end_turn_processes_on_abort on this adapter",
            |s| s.end_turn_processes_on_abort,
        ),
        rule(
            "session_log_upload",
            id == "dsh" && matches!(protocol, Dsh | Acp),
            "session_log_upload requires DeepSeek Harness 0.2.1-rc.5 or later",
            |s| s.session_log_upload.is_some(),
        ),
        rule(
            "native_memory_default",
            protocol == Qwen,
            "native_memory_default requires verified Qwen Code 0.26.0 or later",
            |s| s.native_memory_default.is_some(),
        ),
        rule(
            "model_requests",
            matches!(protocol, Claude | Pi),
            "model_requests on this adapter",
            |s| s.model_requests.is_some(),
        ),
        rule(
            "exclude_user_configuration",
            protocol == Claude,
            "user configuration exclusion on this adapter",
            |s| !s.exclude_user_configuration.is_empty(),
        ),
        rule(
            "output_schema",
            protocol.supports_output_schema(id),
            "native output schema on this adapter",
            |s| s.output_schema.is_some(),
        ),
        rule(
            "goals",
            matches!(protocol, Codex | Dsh),
            "native goal controls on this adapter",
            |s| s.goals,
        ),
        rule(
            "effort",
            !matches!(protocol, Cybion | Morph),
            "native effort on this adapter",
            |s| s.effort.is_some(),
        ),
        rule(
            "service_tier",
            matches!(protocol, Claude | Codex),
            "native service tier on this adapter",
            |s| s.service_tier.is_some(),
        ),
        rule(
            "name",
            matches!(protocol, Codex | Claude | Pi) || dsh,
            "session name on this adapter",
            |s| s.name.is_some(),
        ),
        rule(
            "additional_directories",
            matches!(protocol, Codex | Claude | Qwen | Qoder | KimiKap)
                || opencode
                || (acp && id == "kimi")
                || (id == "dsh"
                    && matches!(protocol, Dsh | Acp)
                    && cfg!(any(target_os = "macos", target_os = "linux"))),
            "additional workspace directories on this adapter",
            |s| !s.additional_directories.is_empty(),
        ),
    ]
}

pub(crate) fn refused_settings(
    protocol: Protocol,
    id: &str,
) -> BTreeMap<&'static str, &'static str> {
    rules(protocol, id)
        .into_iter()
        .filter(|r| !r.allowed)
        .map(|r| (r.name, r.reason))
        .collect()
}

pub(super) fn validate(settings: &SessionSettings, protocol: Protocol, id: &str) -> Result<()> {
    if let Some(rule) = rules(protocol, id)
        .into_iter()
        .find(|r| !r.allowed && (r.supplied)(settings))
    {
        return Err(Error::Unsupported(rule.reason.into()));
    }
    Ok(())
}

pub(crate) fn credential_refusal(protocol: Protocol, id: &str) -> Option<&'static str> {
    match protocol {
        Protocol::Acp if matches!(id, "devin" | "antigravity" | "cursor") => {
            Some("session MCP credential isolation has not been verified for this ACP adapter")
        }
        Protocol::Claude if cfg!(windows) => {
            Some("Claude session MCP credentials are unavailable on Windows")
        }
        Protocol::Pi => {
            Some("Pi RPC does not accept per-server environment variables or HTTP headers")
        }
        Protocol::Dsh => Some(
            "DSH session MCP requires a stdio command and arguments without credential environment or headers",
        ),
        Protocol::Acp if id == "dsh" => Some(
            "DSH session MCP requires a stdio command and arguments without credential environment or headers",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{McpServer, Runtime, Session, SessionOptions};

    #[tokio::test]
    async fn raw_http_and_stdio_credentials_are_refused_before_launch() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["antigravity", "devin", "cursor"] {
            let rt = Runtime::new(id, dir.path().join("absent"), &[], Protocol::Acp);
            let secret = "private-fixture";
            for server in [
                McpServer::Http {
                    url: "http://127.0.0.1:1".into(),
                    headers: [("key".into(), secret.into())].into(),
                },
                McpServer::Stdio {
                    command: "unused".into(),
                    args: vec![],
                    env: [("key".into(), secret.into())].into(),
                },
            ] {
                let mut opts = SessionOptions::new(dir.path());
                opts.mcp_servers.insert("sample".into(), server);
                let e = Session::open(&rt, opts, None, None).await.err().unwrap();
                assert!(
                    matches!(e, Error::Unsupported(reason) if Some(reason.as_str()) == credential_refusal(rt.protocol, id) && !reason.contains(secret))
                );
            }
        }
    }
}
