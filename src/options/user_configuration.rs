//! Select Claude's native configuration sources without relocating credentials or history.
use super::env_path;
use crate::{Error, Result, SessionOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum UserConfigurationCategory {
    Settings,
    Mcp,
    Instructions,
}

fn canonical(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_owned()),
        Err(e) => Err(e.into()),
    }
}
fn comparable(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase()
            .into()
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}
fn claude_paths(opts: &SessionOptions, cwd: &Path) -> Result<Vec<String>> {
    let home = opts
        .session_home()
        .ok_or_else(|| Error::Protocol("Claude home is unknown".into()))?;
    let home = cwd.join(home);
    let config =
        env_path(opts, "CLAUDE_CONFIG_DIR").map_or_else(|| home.join(".claude"), |v| cwd.join(v));
    let config_real = comparable(&canonical(&config)?);
    let cwd_real = comparable(&canonical(cwd)?);
    for directory in [&home, &config] {
        if cwd_real.starts_with(comparable(&canonical(directory)?))
            && !comparable(cwd).starts_with(comparable(directory))
        {
            return Err(Error::Unsupported(
                "Claude can load this aliased user directory as project configuration".into(),
            ));
        }
    }
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join(".claude");
        if comparable(&candidate) != comparable(&config)
            && comparable(&canonical(&candidate)?) == config_real
        {
            return Err(Error::Unsupported(
                "Claude can load aliased user configuration through a project ancestor".into(),
            ));
        }
    }
    let mut paths = Vec::new();
    for path in [
        home.join(".claude/**"),
        config.join("**"),
        home.join("CLAUDE.md"),
        home.join("CLAUDE.local.md"),
    ] {
        let value = path.to_str().ok_or_else(|| {
            Error::Unsupported("Claude instruction exclusions require UTF-8 paths".into())
        })?;
        let value = if cfg!(windows) {
            value.replace('\\', "/")
        } else {
            value.to_owned()
        };
        if !paths.contains(&value) {
            paths.push(value);
        }
    }
    Ok(paths)
}
pub(crate) fn claude(
    cmd: &mut Command,
    opts: &SessionOptions,
    cwd: &Path,
    disable_hooks: bool,
    capture: Option<&str>,
) -> Result<()> {
    use UserConfigurationCategory::*;
    let excluded = &opts.settings.exclude_user_configuration;
    let mut overrides = serde_json::Map::new();
    if let Some(setting) = capture {
        overrides.insert("env".into(), json!({"OTEL_LOG_RAW_API_BODIES":setting}));
    }
    if disable_hooks {
        overrides.insert("disableAllHooks".into(), true.into());
    }
    if let Some(tier) = &opts.settings.service_tier {
        overrides.insert("fastMode".into(), (tier == "fast").into());
    }
    if excluded.contains(&Settings) || excluded.contains(&Instructions) {
        let paths = claude_paths(opts, cwd)?;
        if excluded.contains(&Settings) {
            cmd.args(["--setting-sources", "project,local"]);
        }
        if excluded.contains(&Instructions) {
            overrides.insert("claudeMdExcludes".into(), json!(paths));
            cmd.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
        }
    }
    if excluded.contains(&Mcp) {
        cmd.arg("--strict-mcp-config")
            .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false");
    }
    if !overrides.is_empty() {
        cmd.arg("--settings")
            .arg(Value::Object(overrides).to_string());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn claude_sources_preserve_project_rules_and_reject_a_user_directory_alias() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let config = home.join("custom");
        let cwd = root.join("project");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        let mut opts = SessionOptions::new(&cwd);
        opts.env.insert("HOME".into(), home.clone().into());
        opts.env
            .insert("CLAUDE_CONFIG_DIR".into(), config.clone().into());
        opts.settings
            .exclude_user_configuration
            .insert(UserConfigurationCategory::Instructions);
        opts.settings.service_tier = Some("fast".into());
        let mut cmd = Command::new("claude");
        claude(&mut cmd, &opts, &cwd, true, None).unwrap();
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--settings");
        let settings: Value = serde_json::from_str(args[1].to_str().unwrap()).unwrap();
        assert_eq!(settings["disableAllHooks"], true);
        assert_eq!(settings["fastMode"], true);
        assert!(
            settings["claudeMdExcludes"]
                .as_array()
                .unwrap()
                .contains(&json!(config.join("**")))
        );
        std::os::unix::fs::symlink(&config, cwd.join(".claude")).unwrap();
        assert!(matches!(
            claude_paths(&opts, &cwd),
            Err(Error::Unsupported(_))
        ));
    }
}
