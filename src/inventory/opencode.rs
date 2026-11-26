mod diagnostic;
use super::*;
use crate::process::OwnedProcess;
use semver::Version;
use std::process::Stdio;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
};

async fn add_root(roots: &mut Vec<(PathBuf, SkillScope)>, path: PathBuf, scope: SkillScope) {
    if let Ok(path) = fs::canonicalize(path).await
        && !roots.iter().any(|(root, _)| *root == path)
    {
        roots.push((path, scope));
    }
}

async fn roots(probe: &Probe, major: u64) -> Vec<(PathBuf, SkillScope)> {
    let mut roots = Vec::new();
    let home = crate::account::profile_home(probe);
    let from_env = |key| {
        probe
            .get(key)
            .filter(|p| !p.is_empty())
            .map(|p| probe.cwd.join(p))
    };
    if let Some(path) = from_env("OPENCODE_TEST_HOME").or_else(|| home.clone()) {
        for brand in [".claude", ".agents"] {
            add_root(
                &mut roots,
                path.join(brand).join("skills"),
                SkillScope::User,
            )
            .await;
        }
    }
    let config = (major == 2)
        .then(|| from_env("OPENCODE_CONFIG_DIR"))
        .flatten()
        .or_else(|| {
            from_env("XDG_CONFIG_HOME")
                .or_else(|| home.map(|p| p.join(".config")))
                .map(|p| p.join("opencode"))
        });
    if let Some(config) = config {
        for dir in ["skills", "skill"] {
            add_root(&mut roots, config.join(dir), SkillScope::User).await;
        }
    }
    for parent in probe.cwd.ancestors() {
        for dir in [
            ".claude/skills",
            ".agents/skills",
            ".opencode/skills",
            ".opencode/skill",
        ] {
            add_root(&mut roots, parent.join(dir), SkillScope::Project).await;
        }
        if major == 1 && fs::metadata(parent.join(".git")).await.is_ok() {
            break;
        }
    }
    roots
}

async fn scope(
    path: &str,
    roots: &[(PathBuf, SkillScope)],
    version: &Version,
) -> Option<SkillScope> {
    if path == "<built-in>"
        || (matches!(
            path,
            "skill://opencode/SKILL.md" | "skill://report/SKILL.md"
        ) && [Version::new(2, 2, 7), Version::new(2, 3, 0)].contains(version))
    {
        return Some(SkillScope::Builtin);
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let path = fs::canonicalize(path).await.ok()?;
    let mut matches = roots
        .iter()
        .filter(|(root, _)| path != *root && path.starts_with(root))
        .map(|(_, s)| *s);
    let first = matches.next()?;
    matches.all(|s| s == first).then_some(first)
}

async fn project(raw: &Value, probe: &Probe, version: &Version) -> Result<Vec<SkillEntry>> {
    let roots = roots(probe, version.major).await;
    let mut items = Vec::new();
    let location = if version.major == 1 {
        "location"
    } else {
        "path"
    };
    for row in rows(raw)? {
        let path = row[location]
            .as_str()
            .filter(|p| !p.is_empty())
            .ok_or_else(invalid)?;
        items.push(SkillEntry {
            name: name(row)?,
            description: field(row, "description"),
            source: (path == "<built-in>").then(|| "builtin".into()),
            path: (path != "<built-in>").then(|| path.to_owned()),
            scope: scope(path, &roots, version).await,
            disable_model_invocation: row["autoinvoke"].as_bool().map(|v| !v),
            ..Default::default()
        });
    }
    Ok(items)
}

async fn legacy(
    probe: &Probe,
    path: &Path,
    version: &Version,
) -> Result<InventoryResult<SkillEntry>> {
    // OpenCode 1 can exit before large writes to a pipe drain. Its catalog also contains skill bodies.
    let file = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let mut cmd = probe.command(path);
    cmd.args(["debug", "skill"])
        .stdin(Stdio::null())
        .stderr(stderr.try_clone()?)
        .stdout(file.try_clone()?);
    let status = OwnedProcess::spawn(cmd)?.wait().await?;
    let mut file = fs::File::from_std(file);
    file.rewind().await?;
    let size = file.metadata().await?.len();
    let failed = async |issue: &str| {
        let status = status
            .code()
            .map(|code| format!("exit {code}"))
            .unwrap_or_else(|| "no exit code".into());
        let mut reason =
            format!("OpenCode skill discovery failed ({status}, {size} output bytes, {issue})");
        if let Some(line) = diagnostic::excerpt(&stderr, probe).await {
            reason.push_str(": ");
            reason.push_str(&line);
        }
        InventoryResult::unavailable(UnavailableCode::QueryFailed, &reason)
    };
    if !status.success() {
        return Ok(failed("native command failed").await);
    }
    const LIMIT: u64 = 64 * 1024 * 1024;
    if size > LIMIT {
        return Ok(failed("catalog exceeded size limit").await);
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > LIMIT {
        return Ok(failed("catalog exceeded size limit").await);
    }
    let Ok(raw) = serde_json::from_slice(&bytes) else {
        return Ok(failed("invalid JSON").await);
    };
    let Ok(items) = project(&raw, probe, version).await else {
        return Ok(failed("invalid catalog").await);
    };
    Ok(InventoryResult::observed(
        &probe.cwd,
        items,
        false,
        View::Discovered,
    ))
}

fn unsupported_version(
    version: &Version,
    installed: Option<&str>,
) -> Option<InventoryResult<SkillEntry>> {
    if version.major == 1 {
        return None;
    }
    let tuple = (version.major, version.minor, version.patch);
    if ((2, 2, 7)..(2, 4, 0)).contains(&tuple) {
        return None;
    }
    let reason = if tuple >= (2, 4, 0) {
        "This OpenCode release has no verified way to wait for skill and plugin discovery to finish"
    } else {
        "OpenCode 2 workspace skills require the plugin activation barrier"
    };
    Some(InventoryResult::Unsupported {
        code: UnsupportedCode::VersionUnsupported,
        reason: reason.into(),
        installed_version: installed.map(str::to_owned),
        required_version: Some(">=2.2.7 <2.4.0".into()),
    })
}

pub(super) async fn skills(
    probe: &Probe,
    path: &Path,
    installed: Option<&str>,
    end: Instant,
) -> Result<InventoryResult<SkillEntry>> {
    let version = installed
        .and_then(|s| crate::installation::read_version("opencode", s))
        .ok_or_else(invalid)?;
    if version.major == 1 {
        return legacy(probe, path, &version).await;
    }
    if let Some(unsupported) = unsupported_version(&version, installed) {
        return Ok(unsupported);
    }
    let (raw, partial) = crate::providers::opencode_workspace_skills(
        probe,
        path,
        end.saturating_duration_since(Instant::now()),
    )
    .await?;
    Ok(InventoryResult::observed(
        &probe.cwd,
        project(&raw, probe, &version).await?,
        partial,
        View::Discovered,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_discovery_version_gate_enforces_activation_barrier_window() {
        for ok in ["1.19.6", "2.2.7", "2.2.7-rc.1", "2.3.0+custom", "2.3.9"] {
            let v = crate::installation::read_version("opencode", ok).unwrap();
            assert!(
                unsupported_version(&v, Some(ok)).is_none(),
                "expected {ok} to pass version gate"
            );
        }
        for bad in [
            "0.9.9", "2.0.0", "2.1.9", "2.2.6", "2.4.0", "2.4.1", "3.0.0",
        ] {
            let v = crate::installation::read_version("opencode", bad).unwrap();
            let Some(InventoryResult::Unsupported {
                code,
                installed_version,
                required_version,
                ..
            }) = unsupported_version(&v, Some(bad))
            else {
                panic!("expected {bad} to be rejected by version gate");
            };
            assert_eq!(code, UnsupportedCode::VersionUnsupported);
            assert_eq!(installed_version.as_deref(), Some(bad));
            assert_eq!(required_version.as_deref(), Some(">=2.2.7 <2.4.0"));
        }
    }
}
