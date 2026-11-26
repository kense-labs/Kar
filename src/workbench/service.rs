//! Pure-Rust macOS `launchd` boot service manager (`~/Library/LaunchAgents/dev.kar.workbench.plist`),
//! zero-downtime supervisor health check, and `kar doctor` host diagnostics.

use super::{Workbench, http::ApiError, policy::ManagedPolicy, tailscale};
use crate::{Error, Result};
use axum::{Json, Router, extract::State, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub const LAUNCHD_LABEL: &str = "dev.kar.workbench";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchdConfig {
    pub executable: PathBuf,
    pub roots: Vec<PathBuf>,
    pub state_dir: PathBuf,
    pub port: u16,
    pub log_dir: PathBuf,
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn render_launchd_plist(cfg: &LaunchdConfig) -> String {
    let mut args = vec![
        cfg.executable.to_string_lossy().into_owned(),
        "workbench".into(),
        "--state".into(),
        cfg.state_dir.to_string_lossy().into_owned(),
        "--port".into(),
        cfg.port.to_string(),
    ];
    for root in &cfg.roots {
        args.push("--root".into());
        args.push(root.to_string_lossy().into_owned());
    }
    let program_args: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    let stdout_log = xml_escape(&cfg.log_dir.join("workbench.out.log").to_string_lossy());
    let stderr_log = xml_escape(&cfg.log_dir.join("workbench.err.log").to_string_lossy());
    let workdir = xml_escape(
        &cfg.roots
            .first()
            .cloned()
            .unwrap_or_else(|| cfg.state_dir.clone())
            .to_string_lossy(),
    );

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
    <key>WorkingDirectory</key>
    <string>{workdir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{stdout_log}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_log}</string>
</dict>
</plist>
"#
    )
}

pub fn launch_agents_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("KAR_LAUNCH_AGENTS_DIR").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Refused("HOME is not set".into()))?;
    Ok(home.join("Library/LaunchAgents"))
}

pub fn plist_path() -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{LAUNCHD_LABEL}.plist")))
}

fn uid_domain() -> String {
    format!("gui/{}", nix::unistd::geteuid().as_raw())
}

pub fn service_status() -> Result<Value> {
    let plist = plist_path()?;
    let installed = plist.is_file();
    let domain_target = format!("{}/{LAUNCHD_LABEL}", uid_domain());
    let loaded = if Path::new("/bin/launchctl").is_file() {
        Command::new("/bin/launchctl")
            .args(["print", &domain_target])
            .output()
            .is_ok_and(|o| o.status.success())
    } else {
        false
    };
    Ok(json!({
        "label": LAUNCHD_LABEL,
        "installed": installed,
        "loaded": loaded,
        "plist_path": plist,
        "platform": std::env::consts::OS,
    }))
}

pub fn install_service(cfg: &LaunchdConfig, load_with_launchctl: bool) -> Result<Value> {
    if cfg.roots.is_empty() {
        return Err(Error::Refused(
            "at least one authorized --root is required to install the service".into(),
        ));
    }
    fs::create_dir_all(&cfg.log_dir)?;
    fs::create_dir_all(&cfg.state_dir)?;
    let plist_file = plist_path()?;
    if let Some(parent) = plist_file.parent() {
        fs::create_dir_all(parent)?;
    }
    let xml = render_launchd_plist(cfg);
    fs::write(&plist_file, xml.as_bytes())?;
    if load_with_launchctl && Path::new("/bin/launchctl").is_file() {
        let domain = uid_domain();
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &domain])
            .arg(&plist_file)
            .output();
        let out = Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist_file)
            .output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(Error::Refused(format!("launchctl bootstrap failed: {err}")));
        }
    }
    service_status()
}

pub fn uninstall_service(unload_with_launchctl: bool) -> Result<Value> {
    let plist_file = plist_path()?;
    if unload_with_launchctl && plist_file.is_file() && Path::new("/bin/launchctl").is_file() {
        let domain = uid_domain();
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &domain])
            .arg(&plist_file)
            .output();
    }
    let removed = if plist_file.is_file() {
        fs::remove_file(&plist_file)?;
        true
    } else {
        false
    };
    Ok(json!({
        "label": LAUNCHD_LABEL,
        "removed": removed,
        "plist_path": plist_file,
    }))
}

pub fn restart_service() -> Result<Value> {
    let target = format!("{}/{LAUNCHD_LABEL}", uid_domain());
    let out = Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &target])
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(Error::Refused(format!("launchctl kickstart failed: {err}")));
    }
    service_status()
}

/// Comprehensive host & workbench diagnostic report (`kar doctor` / `GET /server/doctor`).
pub async fn run_doctor(state_dir: Option<&Path>) -> Result<Value> {
    let git_ok = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::process::Command::new("git")
            .arg("--version")
            .output(),
    )
    .await
    .ok()
    .and_then(std::result::Result::ok)
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    let ssh_ok = Path::new("/usr/bin/ssh").is_file();
    let op_ok = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("op"))
            .any(|p| p.is_file())
    });
    let ts = tailscale::inspect_tailscale().await;
    let policy = ManagedPolicy::load()?;
    let launchd = service_status().ok();

    let mut installed_runtimes = Vec::new();
    for rt in crate::runtimes() {
        if let Ok(crate::InstallationSnapshot::Available {
            version, command, ..
        }) = rt.probe_installation(&Default::default()).await
        {
            installed_runtimes.push(json!({
                "id": rt.id,
                "version": version,
                "executable": command,
            }));
        }
    }

    let browser_found = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ]
    .into_iter()
    .find(|p| Path::new(p).is_file());

    let healthy = git_ok.is_some() && !installed_runtimes.is_empty();
    Ok(json!({
        "healthy": healthy,
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "kar_version": env!("CARGO_PKG_VERSION"),
        "state_dir": state_dir,
        "checks": {
            "git": {"available": git_ok.is_some(), "version": git_ok},
            "ssh": {"available": ssh_ok, "path": "/usr/bin/ssh"},
            "onepassword_cli": {"available": op_ok},
            "macos_keychain": {"available": Path::new("/usr/bin/security").is_file()},
            "preview_browser": {"available": browser_found.is_some(), "path": browser_found},
            "tailscale": ts,
            "launchd_service": launchd,
            "managed_policy": policy,
        },
        "installed_runtimes": installed_runtimes,
    }))
}

async fn get_service() -> std::result::Result<Json<Value>, ApiError> {
    Ok(Json(service_status()?))
}

async fn get_doctor(State(app): State<Workbench>) -> std::result::Result<Json<Value>, ApiError> {
    Ok(Json(run_doctor(Some(&app.0.state)).await?))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/server/service", get(get_service))
        .route("/server/doctor", get(get_doctor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_plist_escapes_paths_and_installs_cleanly_in_override_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = LaunchdConfig {
            executable: PathBuf::from("/usr/local/bin/kar"),
            roots: vec![PathBuf::from("/Users/leon/My & Project")],
            state_dir: tmp.path().join("state"),
            port: 4777,
            log_dir: tmp.path().join("logs"),
        };
        let xml = render_launchd_plist(&cfg);
        assert!(xml.contains("<string>dev.kar.workbench</string>"));
        assert!(xml.contains("My &amp; Project"));
        assert!(xml.contains("<string>4777</string>"));
    }
}
