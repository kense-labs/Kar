//! Locate the SDK belonging to the selected Pi executable.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
const PACKAGE: &str = "@earendil-works/pi-coding-agent";

pub(crate) fn standalone(program: &Path) -> bool {
    let Ok(entry) = program.canonicalize() else {
        return false;
    };
    let Some(root) = entry.parent() else {
        return false;
    };
    matches!(
        entry.file_name().and_then(|n| n.to_str()),
        Some("pi" | "pi.exe")
    ) && manifest(root).is_some()
        && !root.join("dist/index.js").is_file()
}

fn manifest(root: &Path) -> Option<Value> {
    let p: Value = serde_json::from_reader(fs::File::open(root.join("package.json")).ok()?).ok()?;
    (p["name"] == PACKAGE).then_some(p)
}

pub(crate) struct Companion {
    pub entry: PathBuf,
    pub dist: PathBuf,
}
pub(crate) enum CompanionError {
    Installation,
    Version {
        installed: String,
        required: Option<String>,
    },
}
impl CompanionError {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Installation => {
                "KAR_PI_SDK_DIR must point to an installed @earendil-works/pi-coding-agent SDK"
            }
            Self::Version { .. } => "The Pi SDK must match the selected executable's exact version",
        }
    }
}

/// Resolve only the named official installation, verified against the selected CLI.
pub(crate) fn companion(root: &Path, version: Option<&str>) -> Result<Companion, CompanionError> {
    fn read(root: &Path) -> Option<(Companion, semver::Version)> {
        let root = root.canonicalize().ok()?;
        let package = manifest(&root)?;
        let version = semver::Version::parse(package["version"].as_str()?).ok()?;
        let entry = root
            .join(package["bin"]["pi"].as_str()?)
            .canonicalize()
            .ok()?;
        let dist = sdk(&entry)?;
        (entry.starts_with(&root)
            && entry.is_file()
            && dist == root.join("dist")
            && dist.join("index.js").is_file())
        .then_some((Companion { entry, dist }, version))
    }
    let (sdk, installed) = read(root).ok_or(CompanionError::Installation)?;
    let required = version.and_then(|v| crate::installation::read_version("pi", v));
    if required.as_ref() != Some(&installed) {
        return Err(CompanionError::Version {
            installed: installed.to_string(),
            required: required.map(|v| v.to_string()),
        });
    }
    Ok(sdk)
}

pub(crate) fn node_for(_program: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(node) = _program
        .parent()
        .map(|dir| dir.join("node.exe"))
        .filter(|p| p.is_file())
    {
        return node;
    }
    PathBuf::from("node")
}

pub(crate) fn sdk(program: &Path) -> Option<PathBuf> {
    let entry = program.canonicalize().ok()?;
    let matches = |root: &Path, selected: &Path| -> Option<PathBuf> {
        let package = manifest(root)?;
        let binary = root
            .join(package["bin"]["pi"].as_str()?)
            .canonicalize()
            .ok()?;
        let dist = root.join("dist");
        (binary == selected
            && dist.join("cli.js").is_file()
            && dist.join("core/agent-session.js").is_file())
        .then_some(dist)
    };
    // Resolve the exact selected installation, including a symlink in node_modules/.bin.
    for root in entry.ancestors().skip(1).take(4) {
        if let Some(dist) = matches(root, &entry) {
            return Some(dist);
        }
    }
    #[cfg(windows)]
    if entry
        .extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("cmd"))
        && let Ok(shim) = fs::read_to_string(&entry)
        && let Some(parent) = entry.parent()
    {
        let shim = shim.replace('\\', "/");
        for relative in [
            "node_modules/@earendil-works/pi-coding-agent",
            "../@earendil-works/pi-coding-agent",
        ] {
            let root = parent.join(relative);
            let declared: Option<Value> = fs::File::open(root.join("package.json"))
                .ok()
                .and_then(|f| serde_json::from_reader(f).ok());
            if let Some(bin) = declared.as_ref().and_then(|v| v["bin"]["pi"].as_str())
                && shim.contains(&format!("/{relative}/{bin}\""))
                && let Ok(selected) = root.join(bin).canonicalize()
                && let Some(dist) = matches(&root, &selected)
            {
                return Some(dist);
            }
        }
    }
    None
}

/// One native Node host shared by the Pi extensions requested for this session.
pub(crate) struct Launch {
    pub(crate) dist: PathBuf,
    node: PathBuf,
}
impl Launch {
    pub(crate) async fn resolve(
        rt: &crate::Runtime,
        opts: &crate::SessionOptions,
        version: Option<&str>,
    ) -> crate::Result<Self> {
        let dist = match sdk(&rt.program) {
            Some(dist) => dist,
            None => {
                let Some(dir) = crate::options::env_path(opts, "KAR_PI_SDK_DIR") else {
                    return Err(crate::Error::Unsupported(
                        "This Pi operation requires the official npm SDK or a matching SDK in KAR_PI_SDK_DIR".into(),
                    ));
                };
                let checked;
                let actual = match version {
                    Some(v) => v,
                    None => {
                        checked = rt.launch_version(opts).await?;
                        &checked
                    }
                };
                companion(&opts.cwd.join(dir), Some(actual))
                    .map_err(|e| crate::Error::Unsupported(e.message().into()))?
                    .dist
            }
        };
        Ok(Self {
            dist,
            node: node_for(&rt.program),
        })
    }
    pub(crate) fn command(
        &self,
        opts: &crate::SessionOptions,
    ) -> crate::Result<tokio::process::Command> {
        let mut cmd = opts.command(&self.node)?;
        cmd.arg("--experimental-addon-modules")
            .arg(self.dist.join("cli.js"));
        Ok(cmd)
    }
    pub(crate) async fn check_plugin(
        &self,
        path: &Path,
        opts: &crate::SessionOptions,
    ) -> crate::Result<()> {
        use std::process::Stdio;
        let mut cmd = opts.command(&self.node)?;
        cmd.args(["--experimental-addon-modules", "--require"])
            .arg(path)
            .args(["-e", ""])
            .current_dir(&opts.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if tokio::time::timeout(opts.handshake_timeout, cmd.status())
            .await
            .map_err(|_| crate::Error::Timeout("Pi SDK host probe".into()))?
            .is_ok_and(|s| s.success())
        {
            Ok(())
        } else {
            Err(crate::Error::Unsupported(
                "The selected Node host cannot load the Pi native extension".into(),
            ))
        }
    }
}
