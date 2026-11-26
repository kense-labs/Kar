use crate::{Error, Protocol, Result, Runtime, SessionOptions};

/// Normalized option name for checking ownership. Preserve the original
/// token and its value when an allowed argument is passed to the CLI.
fn flag(arg: &str, camel_case: bool) -> String {
    let name = arg.split('=').next().unwrap_or(arg);
    if name.starts_with("--") && camel_case {
        let mut out = String::new();
        for c in name.chars() {
            if c.is_ascii_uppercase() {
                out.push('-');
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    } else if name.starts_with('-') && !name.starts_with("--") {
        name.chars().take(2).collect()
    } else {
        name.to_owned()
    }
}

const QWEN_MANAGED: &[&str] = &[
    "--input-format",
    "--output-format",
    "--channel",
    "--approval-mode",
    "--yolo",
    "-y",
    "--include-partial-messages",
    "--no-include-partial-messages",
    "--session-id",
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--fork-session",
    "--acp",
    "--prompt",
    "-p",
    "--json-schema",
    "-o",
    "--prompt-interactive",
    "-i",
    "--model",
    "-m",
    "--system-prompt",
    "--append-system-prompt",
    "--experimental-acp",
    "--json-fd",
    "--json-file",
    "--input-file",
    "--sandbox-session-id",
    "--list-extensions",
    "-l",
    "--",
];

const CLAUDE_MANAGED: &[&str] = &[
    "--continue",
    "-c",
    "--resume",
    "-r",
    "--session-id",
    "--fork-session",
    "--permission-prompt-tool",
    "--print",
    "-p",
    "--input-format",
    "--output-format",
    "--include-partial-messages",
    "--no-include-partial-messages",
    "--replay-user-messages",
    "--no-replay-user-messages",
    "--model",
    "--effort",
    "--system-prompt",
    "--system-prompt-file",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--name",
    "-n",
    "--json-schema",
    "--",
    // Kar routes tool decisions through the host, including its deny policy.
    "--dangerously-skip-permissions",
    "--permission-prompts",
];
const FACTORY_MANAGED: &[&str] = &[
    "--input-format",
    "--output-format",
    "-o",
    "--session-id",
    "-s",
    "--fork",
    "--cwd",
    "-f",
    "--file",
    "--",
];
const CODEBUDDY_MANAGED: &[&str] = &[
    "--acp",
    "--acp-transport",
    "--serve",
    "--print",
    "-p",
    "--input-format",
    "--output-format",
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--session-id",
    "--fork-session",
    "--permission-mode",
    "--permission-mode-before-plan",
    "--model",
    "-m",
    "--system-prompt",
    "--system-prompt-file",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--",
];
pub(crate) const KIMI_MANAGED: &[&str] = &[
    "--json",
    "--host",
    "--port",
    "--open",
    "--no-open",
    "--socket",
    "--remote-control",
    "--rc",
    "--dangerous-bypass-auth",
    "--allow-remote-shutdown",
    "--debug-endpoints",
    "--managed-by",
    "--",
];

pub(crate) fn refusal(protocol: Protocol, id: &str) -> Option<&'static str> {
    match (protocol, id) {
        (Protocol::Dsh, _) | (Protocol::Acp, "dsh") => {
            Some("DSH launch overlays can disable the session bridge")
        }
        (Protocol::Pi, _) => Some("Kar owns the Pi RPC startup arguments"),
        (Protocol::Cybion, _) => Some("Cybion workers own their launch arguments"),
        (Protocol::Morph, _) => Some("Morph's shared Console owns its launch arguments"),
        _ => None,
    }
}

pub(crate) fn claude_allow_permission_args(options: &SessionOptions) -> &'static [&'static str] {
    if options.approvals != crate::ApprovalMode::Allow {
        return &[];
    }
    let has_mode = options
        .launch_args
        .iter()
        .any(|arg| flag(arg, false) == "--permission-mode");
    if has_mode {
        if options
            .launch_args
            .iter()
            .any(|arg| flag(arg, false) == "--permission-prompts")
        {
            &[]
        } else {
            &["--permission-prompts", "none"]
        }
    } else if options
        .launch_args
        .iter()
        .any(|arg| flag(arg, false) == "--dangerously-skip-permissions")
    {
        &[]
    } else {
        &["--dangerously-skip-permissions"]
    }
}

pub(crate) fn validate(runtime: &Runtime, options: &SessionOptions, fork: bool) -> Result<()> {
    use crate::UserConfigurationCategory::{Instructions, Mcp, Settings};
    let args = &options.launch_args;
    if args.is_empty() {
        return Ok(());
    }
    if let Some(reason) = refusal(runtime.protocol, &runtime.id) {
        return Err(Error::Unsupported(reason.into()));
    }
    let allow = options.approvals == crate::ApprovalMode::Allow;
    let host_questions = options.user_questions == Some(crate::runtime::UserQuestionsMode::Host)
        && options.approvals != crate::ApprovalMode::Host;
    let (managed, camel): (&[&str], bool) = match (runtime.protocol, runtime.id.as_str()) {
        (Protocol::Qwen, _) => (QWEN_MANAGED, true),
        (Protocol::Codex, _) => (&["--listen", "--"], false),
        (Protocol::Claude, _) => (CLAUDE_MANAGED, false),
        (Protocol::KimiKap, _) => (KIMI_MANAGED, false),
        (Protocol::Factory, _) | (Protocol::Acp, "factory") => (FACTORY_MANAGED, false),
        (Protocol::Qoder, _) | (Protocol::Acp, "qoder") => {
            return args
                .iter()
                .try_for_each(|arg| crate::qoder::validate_arg(arg));
        }
        (Protocol::Acp, "codebuddy") => (CODEBUDDY_MANAGED, true),
        (Protocol::Acp, "antigravity") => (
            if allow {
                &["--process-per-session", "--"]
            } else {
                &[
                    "--process-per-session",
                    "--dangerously-skip-permissions",
                    "--",
                ]
            },
            false,
        ),
        (Protocol::Acp, id) if id != "dsh" => (&["--"], false),
        _ => {
            return Err(Error::Unsupported(format!(
                "Kar does not accept session launch arguments for {}",
                runtime.id
            )));
        }
    };
    let settings = &options.settings;
    let excluded = &settings.exclude_user_configuration;
    let has_claude_mode = runtime.protocol == Protocol::Claude
        && args.iter().any(|a| flag(a, false) == "--permission-mode");
    for (i, arg) in args.iter().enumerate() {
        let name = flag(arg, camel);
        let value = || {
            arg.split_once('=')
                .map(|(_, v)| v)
                .or_else(|| args.get(i + 1).map(String::as_str))
        };
        let claude_allow_override = runtime.protocol == Protocol::Claude
            && allow
            && !host_questions
            && match name.as_str() {
                "--dangerously-skip-permissions" => !has_claude_mode,
                "--permission-prompts" => value() == Some("none"),
                _ => false,
            };
        let claude_conflict = runtime.protocol == Protocol::Claude
            && match name.as_str() {
                "--resume-session-at" => fork,
                "--setting-sources" => excluded.contains(&Settings),
                "--strict-mcp-config" => excluded.contains(&Mcp),
                "--settings" => {
                    excluded.contains(&Instructions)
                        || settings.service_tier.is_some()
                        || settings.model_requests.is_some()
                }
                "--permission-mode" if host_questions => true,
                "--permission-mode" => {
                    if allow {
                        !matches!(
                            value(),
                            Some("default" | "plan" | "dontAsk" | "acceptEdits")
                        )
                    } else {
                        !matches!(value(), Some("default" | "plan" | "dontAsk"))
                    }
                }
                _ => false,
            };
        if claude_conflict || (!claude_allow_override && managed.contains(&name.as_str())) {
            return Err(Error::Unsupported(format!(
                "Kar manages the {name} argument for {}",
                runtime.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalMode, runtime::UserQuestionsMode};

    #[test]
    fn claude_managed_flags_stay_protected_under_deny_and_host_and_respect_questions() {
        let rt = Runtime::new("claude", "claude", &[], Protocol::Claude);
        let mut opts = SessionOptions::new("/tmp");
        opts.launch_args = vec!["--dangerously-skip-permissions".into()];
        assert!(validate(&rt, &opts, false).is_err());
        opts.approvals = ApprovalMode::Host;
        assert!(validate(&rt, &opts, false).is_err());

        opts.approvals = ApprovalMode::Allow;
        opts.launch_args.clear();
        assert_eq!(
            claude_allow_permission_args(&opts),
            &["--dangerously-skip-permissions"]
        );

        opts.user_questions = Some(UserQuestionsMode::Host);
        opts.launch_args = vec!["--permission-mode".into(), "dontAsk".into()];
        assert!(validate(&rt, &opts, false).is_err());
    }
}
