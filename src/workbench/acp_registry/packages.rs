use super::{Result, bad};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

pub(super) fn identity<'a>(kind: &str, spec: &'a str) -> Result<(&'a str, &'a str)> {
    if spec.len() > 256 {
        return Err(bad("package spec too long"));
    }
    let (name, version) = if kind == "uvx" {
        spec.rsplit_once("==").or_else(|| spec.rsplit_once('@'))
    } else {
        spec.rsplit_once('@')
    }
    .ok_or_else(|| bad("package requires an exact version"))?;
    let part = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let valid = if kind == "npx"
        && let Some(scoped) = name.strip_prefix('@')
    {
        scoped
            .split_once('/')
            .is_some_and(|(scope, name)| part(scope) && part(name))
    } else {
        part(name)
    };
    let version = version.strip_prefix('v').unwrap_or(version);
    if !valid || semver::Version::parse(version).is_err() {
        return Err(bad("invalid package name or exact version"));
    }
    Ok((name, version))
}
fn executable(p: &Path) -> bool {
    p.metadata().is_ok_and(|m| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            m.is_file()
        }
    })
}
pub(super) fn manager(kind: &str) -> Option<PathBuf> {
    let name = if kind == "npx" { "npm" } else { "uv" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| executable(p))
}
async fn output(
    manager: &Path,
    root: &Path,
    kind: &str,
    args: &[&str],
    install: bool,
) -> Result<String> {
    let mut cmd = Command::new(manager);
    cmd.current_dir(root)
        .args(args)
        .env_remove("KAR_WORKBENCH_TOKEN")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if kind == "npx" {
        for (k, _) in
            std::env::vars_os().filter(|(k, _)| k.eq_ignore_ascii_case("npm_config_prefix"))
        {
            cmd.env_remove(k);
        }
        cmd.env("npm_config_prefix", root);
    } else {
        cmd.env("UV_TOOL_DIR", root)
            .env("UV_TOOL_BIN_DIR", root.join("bin"));
    }
    let seconds = if install { 1200 } else { 30 };
    tokio::time::timeout(Duration::from_secs(seconds), async {
        let mut child = cmd.spawn()?;
        async fn read(mut pipe: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
            let mut data = Vec::new();
            let mut buf = [0; 8192];
            loop {
                let n = pipe.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                if data.len() > 1024 * 1024 {
                    data.drain(..data.len() - 1024 * 1024);
                }
            }
            Ok(data)
        }
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (status, out, err) = tokio::try_join!(child.wait(), read(stdout), read(stderr))?;
        if !status.success() {
            let e = String::from_utf8_lossy(&err[err.len().saturating_sub(1000)..]);
            return Err(bad(&format!("ACP package manager failed ({status}): {e}")));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    })
    .await
    .map_err(|_| crate::Error::Timeout("ACP package manager".into()))?
}
fn npm_bin(root: &Path, name: &str, version: &str, agent: &str) -> Result<Option<PathBuf>> {
    let modules = if cfg!(windows) {
        root.join("node_modules")
    } else {
        root.join("lib/node_modules")
    };
    let manifest = modules.join(name).join("package.json");
    if !manifest.try_exists()? {
        return Ok(None);
    }
    if manifest.metadata()?.len() > 1024 * 1024 {
        return Err(bad("oversized npm package manifest"));
    }
    let value: serde_json::Value = serde_json::from_reader(std::fs::File::open(manifest)?)?;
    if value["name"] != name || value["version"] != version {
        return Ok(None);
    }
    let short = name.rsplit('/').next().unwrap();
    let bin = match &value["bin"] {
        serde_json::Value::String(_) => short,
        serde_json::Value::Object(map) if map.len() == 1 => map.keys().next().unwrap(),
        serde_json::Value::Object(map) if map.contains_key(short) => short,
        serde_json::Value::Object(map) if map.contains_key(agent) => agent,
        serde_json::Value::Object(map)
            if !map.is_empty() && map.values().all(|v| v == map.values().next().unwrap()) =>
        {
            map.keys().next().unwrap()
        }
        _ => return Err(bad("npm package does not identify a unique ACP command")),
    };
    if !super::catalog::version_ok(bin) || bin.len() > 128 {
        return Err(bad("invalid npm bin name"));
    }
    let bin = if cfg!(windows) {
        PathBuf::from(bin)
    } else {
        Path::new("bin").join(bin)
    };
    Ok(executable(&root.join(&bin)).then_some(bin))
}
async fn uv_bin(
    manager: &Path,
    root: &Path,
    name: &str,
    version: &str,
    agent: &str,
) -> Result<Option<PathBuf>> {
    let list = output(manager, root, "uvx", &["tool", "list"], false).await?;
    let normalize = |v: &str| {
        v.to_ascii_lowercase()
            .split(['-', '_', '.'])
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-")
    };
    let installed = list
        .lines()
        .filter_map(|l| l.split_once(" v"))
        .any(|(pkg, v)| {
            normalize(pkg) == normalize(name) && v.split_whitespace().next() == Some(version)
        });
    if !installed {
        return Ok(None);
    }
    Ok([agent, name, name.trim_end_matches("-acp")]
        .iter()
        .map(|c| Path::new("bin").join(c))
        .find(|p| executable(&root.join(p))))
}
pub(super) async fn install(root: &Path, agent: &str, kind: &str, spec: &str) -> Result<PathBuf> {
    let manager = manager(kind).ok_or_else(|| {
        bad(if kind == "npx" {
            "npm unavailable on host PATH"
        } else {
            "uv unavailable on host PATH"
        })
    })?;
    let (name, version) = identity(kind, spec)?;
    let payload = root.join("payload");
    std::fs::create_dir_all(&payload)?;
    let bin = if kind == "npx" {
        if let Some(bin) = npm_bin(&payload, name, version, agent)? {
            bin
        } else {
            output(
                &manager,
                &payload,
                kind,
                &["install", "--global", spec],
                true,
            )
            .await?;
            npm_bin(&payload, name, version, agent)?
                .ok_or_else(|| bad("installed npm command not found"))?
        }
    } else if let Some(bin) = uv_bin(&manager, &payload, name, version, agent).await? {
        bin
    } else {
        let spec = format!("{name}=={version}");
        output(
            &manager,
            &payload,
            kind,
            &["tool", "install", "--force", &spec],
            true,
        )
        .await?;
        uv_bin(&manager, &payload, name, version, agent)
            .await?
            .ok_or_else(|| bad("installed uv command not found"))?
    };
    Ok(Path::new("payload").join(bin))
}
