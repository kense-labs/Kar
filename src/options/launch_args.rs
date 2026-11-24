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

pub(crate) fn validate(runtime: &Runtime, options: &SessionOptions, fork: bool) -> Result<()> {
    use crate::UserConfigurationCategory::{Instructions, Mcp, Settings};
    let args = &options.launch_args;
    if args.is_empty() {
        return Ok(());
    }
    if let Some(reason) = refusal(runtime.protocol, &runtime.id) {
        return Err(Error::Unsupported(reason.into()));
    }
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
        (Protocol::Acp, "antigravity") => (&["--process-per-session", "--"], false),
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
    for (i, arg) in args.iter().enumerate() {
        let name = flag(arg, camel);
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
                "--permission-mode" => {
                    // These native modes can only restrict the host-controlled prompt path.
                    let value = arg
                        .split_once('=')
                        .map(|(_, v)| v)
                        .or_else(|| args.get(i + 1).map(String::as_str));
                    !matches!(value, Some("default" | "plan" | "dontAsk"))
                }
                _ => false,
            };
        if claude_conflict || managed.contains(&name.as_str()) {
            return Err(Error::Unsupported(format!(
                "Kar manages the {name} argument for {}",
                runtime.id
            )));
        }
    }
    Ok(())
}
