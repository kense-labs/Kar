use super::PiProviders;
pub(super) use crate::terminal::Terminal;
use crate::{Error, Result, providers::ProviderErrorKind as Kind};
use portable_pty::{CommandBuilder, PtySize};

impl Terminal {
    pub(super) fn start(pi: &PiProviders, args: &[&str]) -> Result<Self> {
        let mut cmd = login_command(pi)?;
        cmd.env_clear();
        for (k, v) in &pi.probe.env {
            cmd.env(k, v);
        }
        cmd.cwd(&pi.probe.cwd);
        cmd.args(args);
        Self::open(
            cmd,
            PtySize {
                rows: 40,
                cols: 240,
                ..Default::default()
            },
        )
        .map_err(|_| Kind::StartupFailed.error("Could not start the Pi login terminal"))
    }
    pub fn answer(&self, mut value: String) -> Result<()> {
        if value.chars().any(char::is_control) {
            return Err(Error::Refused(
                "Pi terminal prompts require a single printable line".into(),
            ));
        }
        value.push('\r');
        self.write(value)
    }
}
#[cfg(not(windows))]
fn login_command(pi: &PiProviders) -> Result<CommandBuilder> {
    Ok(CommandBuilder::new(&pi.command))
}

#[cfg(windows)]
fn login_command(pi: &PiProviders) -> Result<CommandBuilder> {
    use std::path::Path;
    if !pi
        .command
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
    {
        return Ok(CommandBuilder::new(&pi.command));
    }
    // npm global and project-local shims. ConPTY starts a Windows executable,
    // so pass Pi's own entry point to Node as an argv element.
    let dir = pi
        .command
        .parent()
        .ok_or_else(|| Kind::StartupFailed.error("Pi launcher has no directory"))?;
    let script = [
        dir.join("node_modules"),
        dir.parent().unwrap_or(dir).to_owned(),
    ]
    .into_iter()
    .map(|d| d.join("@earendil-works/pi-coding-agent/dist/bundle/cli.js"))
    .find(|p| p.is_file())
    .ok_or_else(|| {
        Kind::StartupFailed
            .error("Could not locate the installed Pi entry point for this Windows shim")
    })?;
    let node = std::iter::once(dir.to_owned())
        .chain(
            pi.probe
                .get("PATH")
                .into_iter()
                .flat_map(std::env::split_paths),
        )
        .map(|d| pi.probe.cwd.join(d).join(Path::new("node.exe")))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Kind::StartupFailed.error("Could not locate Node for the installed Windows Pi package")
        })?;
    let mut cmd = CommandBuilder::new(node);
    cmd.arg(script);
    Ok(cmd)
}
