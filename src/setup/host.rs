//! Read-only, current-user validation of the installer's tools and actual destinations.
use super::{InstallPlan, InstallPreview, update};
use crate::{InstallationSnapshot, Runtime, installation::Probe};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallReason {
    Platform,
    RequiresPrivileges,
    RequiresGui,
    MissingTool,
    LocationUnknown,
    Configuration,
    Bundled,
    LineRequired,
    UnknownLine,
    NoInstaller,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallRefusal {
    pub reason: InstallReason,
    pub detail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<InstallStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}
impl InstallRefusal {
    fn new(reason: InstallReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
            steps: vec![],
            source: None,
        }
    }
    fn steps(mut self, p: &InstallPreview) -> Self {
        self.steps = steps(p);
        self.source = Some(p.source.clone());
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallStep {
    pub command: Vec<String>,
    pub display: String,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub environment: std::collections::BTreeMap<String, String>,
}
fn steps(preview: &InstallPreview) -> Vec<InstallStep> {
    preview.steps.iter().map(|step|{
        let mut environment=std::collections::BTreeMap::new();
        let cmd=std::iter::once(step.program.to_owned()).chain(step.arguments.iter().cloned()).collect::<Vec<_>>();
        let mut display=cmd.join(" ");
        if matches!(preview.plan,InstallPlan::VendorScript{url:"https://chatgpt.com/codex/install.sh",interpreter,..} if step.program==interpreter) {
            environment.insert("CODEX_NON_INTERACTIVE".into(),"1".into());
            display=format!("CODEX_NON_INTERACTIVE=1 {display}");
        }
        InstallStep{command:cmd,display,environment}
    }).collect()
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostInstallPlan {
    Plan {
        steps: Vec<InstallStep>,
        locations: Vec<PathBuf>,
        source: String,
        privileges: bool,
        network: bool,
    },
    Unsupported(InstallRefusal),
}
impl HostInstallPlan {
    pub(super) fn checked(preview: InstallPreview, locations: Vec<PathBuf>) -> Self {
        Self::Plan {
            steps: steps(&preview),
            locations,
            source: preview.source,
            privileges: false,
            network: true,
        }
    }
}

pub(super) fn documentation(rt: &str, plan: &InstallPlan) -> String {
    match rt {
        "codex" => "https://github.com/openai/codex#installing-and-running-codex-cli",
        "claude" => "https://code.claude.com/docs/en/setup",
        "grok" => "https://docs.x.ai/build/overview",
        "kimi" => "https://github.com/MoonshotAI/kimi-code#install",
        "devin" => "https://docs.devin.ai/cli",
        "antigravity" => "https://antigravity.google/docs/cli",
        "qwen" => "https://qwenlm.github.io/qwen-code-docs/en/users/quickstart/",
        "qoder" => "https://docs.qoder.com/cli/troubleshoot-install",
        "factory" => "https://docs.factory.ai/cli/getting-started/quickstart",
        "codebuddy" => "https://www.codebuddy.ai/docs/cli/installation",
        "opencode" if matches!(plan,InstallPlan::VendorScript{url,..} if url.contains("/v2/")) => {
            "https://opencode.ai/v2/docs/"
        }
        "opencode" => "https://opencode.ai/docs/#install",
        _ => return plan.preview().source,
    }
    .into()
}

fn folder(probe: &Probe, key: &str, fallback: &Path) -> PathBuf {
    probe
        .get(key)
        .filter(|v| !v.is_empty())
        .map_or_else(|| fallback.to_owned(), PathBuf::from)
}

fn script_locations(id: &str, probe: &Probe) -> Result<Vec<PathBuf>, String> {
    let home = probe
        .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .ok_or("home directory is unknown")?;
    let bin = home.join(".local/bin");
    let dirs = match id {
        "codex" => vec![
            folder(probe, "CODEX_INSTALL_DIR", &bin),
            folder(probe, "CODEX_HOME", &home.join(".codex")),
        ],
        "claude" => vec![bin, home.join(".local/share/claude"), home.join(".claude")],
        "grok" => {
            let dir = home.join(".grok");
            vec![folder(probe, "GROK_BIN_DIR", &dir.join("bin")), dir]
        }
        "kimi" => {
            let dir = home.join(".kimi-code");
            vec![
                folder(probe, "KIMI_INSTALL_DIR", &dir),
                folder(probe, "KIMI_CODE_HOME", &dir),
            ]
        }
        "devin" => {
            let data = folder(probe, "XDG_DATA_HOME", &home.join(".local/share"));
            vec![bin, data.join("devin"), data.join("man/man1")]
        }
        "antigravity" => vec![bin, home.join(".cache/antigravity")],
        "qwen" => {
            let root = folder(probe, "QWEN_INSTALL_ROOT", &home.join(".local"));
            let parent = folder(probe, "QWEN_INSTALL_LIB_PARENT", &root.join("lib"));
            let library = folder(probe, "QWEN_INSTALL_LIB_DIR", &parent.join("qwen-code"));
            vec![
                folder(probe, "QWEN_INSTALL_BIN_DIR", &root.join("bin")),
                library
                    .parent()
                    .ok_or("invalid Qwen library path")?
                    .to_owned(),
            ]
        }
        "qoder" => vec![bin, home.join(".qoder")],
        "opencode" => vec![home.join(".opencode/bin")],
        "cursor" => vec![bin, home.join(".local/share/cursor-agent")],
        _ => vec![bin],
    };
    Ok(dirs.into_iter().map(|p| probe.cwd.join(p)).collect())
}

pub(crate) async fn npm_prefix(probe: &Probe, npm: &Path) -> Result<PathBuf, String> {
    let cache = tempfile::Builder::new()
        .prefix("kar-npm-prefix-")
        .tempdir()
        .map_err(|_| "cannot create a private cache")?;
    let mut cmd = update::setup_command(probe, npm);
    cmd.args(["prefix", "-g", "--cache"])
        .arg(cache.path())
        .args([
            "--loglevel=error",
            "--logs-max=0",
            "--timing=false",
            "--update-notifier=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        crate::process::OwnedProcess::spawn(cmd).map_err(|_| "prefix query could not start")?;
    let out = tokio::time::timeout(Duration::from_secs(15), child.output())
        .await
        .map_err(|_| "prefix query timed out")?
        .map_err(|_| "prefix query failed")?;
    if !out.status.success() {
        return Err(match out.status.code() {
            Some(code) => format!("prefix query exited with code {code}"),
            None => "prefix query terminated without an exit code".into(),
        });
    }
    let raw = std::str::from_utf8(&out.stdout).map_err(|_| "prefix is not valid text")?;
    let value = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw);
    if value.contains(['\n', '\r', '\0']) || !Path::new(value).is_absolute() {
        return Err("prefix query did not return one absolute path".into());
    }
    Ok(PathBuf::from(value))
}

fn check_location(path: &Path) -> Result<(), InstallRefusal> {
    let mut existing = path;
    loop {
        match existing.metadata() {
            Ok(m) if !m.is_dir() => {
                return Err(InstallRefusal::new(
                    InstallReason::LocationUnknown,
                    format!("{} is not a directory", existing.display()),
                ));
            }
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if existing.symlink_metadata().is_ok() {
                    return Err(InstallRefusal::new(
                        InstallReason::LocationUnknown,
                        format!("{} is a broken link", existing.display()),
                    ));
                }
                existing = existing.parent().ok_or_else(|| {
                    InstallRefusal::new(
                        InstallReason::LocationUnknown,
                        "no existing destination ancestor",
                    )
                })?;
            }
            Err(e) => {
                return Err(InstallRefusal::new(
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        InstallReason::RequiresPrivileges
                    } else {
                        InstallReason::LocationUnknown
                    },
                    format!("cannot inspect {}", existing.display()),
                ));
            }
        }
    }
    #[cfg(unix)]
    let writable = nix::unistd::access(
        existing,
        nix::unistd::AccessFlags::W_OK | nix::unistd::AccessFlags::X_OK,
    )
    .is_ok();
    #[cfg(not(unix))]
    let writable = existing
        .metadata()
        .is_ok_and(|m| !m.permissions().readonly());
    if !writable {
        return Err(InstallRefusal::new(
            InstallReason::RequiresPrivileges,
            format!(
                "{} is not writable for {}",
                existing.display(),
                path.display()
            ),
        ));
    }
    Ok(())
}

pub(super) async fn inspect(
    rt: &Runtime,
    probe: &Probe,
    line: Option<&str>,
    existing: Option<&InstallationSnapshot>,
) -> Result<(InstallPreview, Vec<PathBuf>), InstallRefusal> {
    let method = rt.install_plan_for(line).map_err(|e| {
        let reason = if line.is_some_and(|l| !rt.install_lines().contains(&l)) {
            InstallReason::UnknownLine
        } else if line.is_none() && !rt.install_lines().is_empty() {
            InstallReason::LineRequired
        } else if rt.protocol == crate::Protocol::Cybion {
            InstallReason::Bundled
        } else if matches!(
            rt.id.as_str(),
            "codex"
                | "claude"
                | "opencode"
                | "grok"
                | "kimi"
                | "devin"
                | "antigravity"
                | "qwen"
                | "qoder"
                | "factory"
                | "cursor"
        ) {
            InstallReason::Platform
        } else {
            InstallReason::NoInstaller
        };
        InstallRefusal::new(reason, e.to_string())
    })?;
    let mut preview = method.preview();
    preview.source = documentation(&rt.id, &method);
    let dirs = match method {
        InstallPlan::VendorScript { interpreter, .. } => {
            if !cfg!(all(
                any(target_os = "macos", target_os = "linux"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            )) {
                return Err(InstallRefusal::new(
                    InstallReason::Platform,
                    "vendor script has no verified build on this platform or architecture",
                ));
            }
            for tool in ["curl", interpreter] {
                if probe.tool(tool).is_none() {
                    return Err(
                        InstallRefusal::new(InstallReason::MissingTool, tool).steps(&preview)
                    );
                }
            }
            script_locations(&rt.id, probe).map_err(|e| {
                InstallRefusal::new(InstallReason::LocationUnknown, e).steps(&preview)
            })?
        }
        InstallPlan::Npm { package } => {
            if !cfg!(any(target_os = "linux", target_os = "macos", windows)) {
                return Err(InstallRefusal::new(
                    InstallReason::Platform,
                    "npm installation is supported on Linux, macOS and Windows",
                ));
            }
            let npm = probe.tool(super::plan::npm_program()).ok_or_else(|| {
                InstallRefusal::new(InstallReason::MissingTool, "npm").steps(&preview)
            })?;
            let prefix = npm_prefix(probe, &npm).await.map_err(|e| {
                InstallRefusal::new(InstallReason::LocationUnknown, e).steps(&preview)
            })?;
            let (bin, library) = if cfg!(windows) {
                (prefix.clone(), prefix.join("node_modules"))
            } else {
                (prefix.join("bin"), prefix.join("lib/node_modules"))
            };
            vec![prefix, bin, library.join(package)]
        }
    };
    for dir in &dirs {
        check_location(dir).map_err(|e| e.steps(&preview))?;
    }
    if rt.id == "devin"
        && !probe.entry(rt).1
        && dirs[0]
            .join("devin")
            .symlink_metadata()
            .is_ok_and(|m| m.is_file())
    {
        return Err(InstallRefusal::new(
            InstallReason::Configuration,
            "Devin's installer refuses to replace an existing regular launcher file",
        ));
    }
    if probe.entry(rt).1 || matches!(rt.id.as_str(), "antigravity" | "devin") {
        let snapshot;
        let selected = match existing {
            Some(s) => s,
            None => {
                snapshot = probe.discover(rt).await;
                &snapshot
            }
        };
        let pinned = probe.entry(rt).1;
        let cannot_replace = if !pinned && matches!(selected, InstallationSnapshot::Failed { .. }) {
            // A broken PATH copy need not block installation at the official destination.
            let mut official = rt.clone();
            official.program = dirs[0].join(if rt.id == "devin" { "devin" } else { "agy" });
            matches!(
                probe.discover(&official).await,
                InstallationSnapshot::Failed { .. }
            )
        } else {
            pinned && !matches!(selected, InstallationSnapshot::Available { .. })
        };
        if cannot_replace {
            return Err(InstallRefusal::new(
                InstallReason::Configuration,
                format!(
                    "the installer cannot repair the selected {} executable; correct its selection or upgrade it",
                    rt.id
                ),
            ));
        }
    }
    Ok((preview, dirs))
}
