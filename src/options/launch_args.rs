use crate::{Error, Protocol, Result, Runtime};

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

pub(crate) fn validate(runtime: &Runtime, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    if runtime.protocol != Protocol::Qwen {
        return Err(Error::Unsupported(format!(
            "Kar does not accept session launch arguments for {}",
            runtime.id
        )));
    }
    for arg in args {
        let name = flag(arg, true);
        if QWEN_MANAGED.contains(&name.as_str()) {
            return Err(Error::Unsupported(format!(
                "Kar manages Qwen's {name} argument"
            )));
        }
    }
    Ok(())
}
