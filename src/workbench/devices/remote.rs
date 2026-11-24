//! SSH supplies only the transport for Kar's fixed native device operations.
use super::*;
use std::future::Future;
use tokio::process::Command;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SshHost {
    pub id: String,
    pub label: String,
    target: String,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    kind: Option<String>,
}
impl SshHost {
    pub fn validate(&self) -> Result<()> {
        if self.kind.as_deref().is_some_and(|v| v != "ssh")
            || self.id == "local"
            || self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            || self.label.trim().is_empty()
            || self.port == Some(0)
            || self.target.is_empty()
            || self.target.starts_with('-')
            || self.target.len() > 512
            || !self
                .target
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"@._:[]%-".contains(&c))
        {
            return Err(Error::Refused("invalid SSH device host".into()));
        }
        if self
            .identity_file
            .as_ref()
            .is_some_and(|p| !p.is_absolute() || !p.is_file())
        {
            return Err(Error::Refused("invalid SSH identity file".into()));
        }
        Ok(())
    }
    fn command(&self, program: &str, args: &[&str]) -> Command {
        let mut cmd = native::local_command("ssh");
        cmd.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ClearAllForwardings=yes",
        ]);
        if let Some(p) = self.port {
            cmd.args(["-p", &p.to_string()]);
        }
        if let Some(file) = &self.identity_file {
            cmd.arg("-i").arg(file);
        }
        let remote = std::iter::once(program)
            .chain(args.iter().copied())
            .map(actions::quote)
            .collect::<Vec<_>>()
            .join(" ");
        cmd.arg("--").arg(&self.target).arg(remote);
        cmd
    }
    pub async fn probe(&self) -> Result<Value> {
        self.validate()?;
        let out=at(Some(self),native::text("sh",&["-c","uname -s; command -v xcrun >/dev/null && printf 'ios\\n'; command -v adb >/dev/null && printf 'android\\n'; true"])).await.map_err(|_|Error::Refused("could not complete the SSH device host probe".into()))?;
        Ok(
            json!({"id":self.id,"label":self.label,"kind":"ssh","platforms":[{"platform":"ios","available":out.lines().any(|s|s=="Darwin") && out.lines().any(|s|s=="ios")},{"platform":"android","available":out.lines().any(|s|s=="android")}]}),
        )
    }
}
// A discovery or native action may issue several nested commands. Pin the selected host for the
// lifetime of that future; other tasks and spawned processes cannot inherit this routing state.
tokio::task_local! {static HOST:SshHost;}
pub(super) async fn at<T>(host: Option<&SshHost>, f: impl Future<Output = T>) -> T {
    match host {
        Some(host) => HOST.scope(host.clone(), f).await,
        None => f.await,
    }
}
pub(super) fn active() -> bool {
    HOST.try_with(|_| ()).is_ok()
}
pub(super) fn command(program: &str, args: &[&str]) -> Command {
    HOST.try_with(|host| host.command(program, args))
        .unwrap_or_else(|_| {
            let mut cmd = native::local_command(program);
            cmd.args(args);
            cmd
        })
}
