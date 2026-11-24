use crate::{
    Error, Result, Runtime, SessionOptions,
    account::{profile_home, qwen},
    installation::Probe,
    private_dir::PrivateDir,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

/// Lower the Qwen system defaults. Higher-priority native settings still apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NativeMemoryDefault {
    Off,
}

/// Effective background memory for the native session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeMemory {
    On,
    Off,
}

pub(crate) struct Setup {
    pub directory: Option<PrivateDir>,
    pub effective: Option<NativeMemory>,
}

const PREFIX: &str = "kar-rs-qwen-defaults-";
const SIGNATURE: &[u8] = b"kar-rs Qwen system defaults v1\n";
const KEYS: [(&str, bool); 3] = [
    ("enableManagedAutoMemory", true),
    ("enableManagedAutoDream", true),
    ("enableAutoSkill", false),
];

fn normalized(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

// Pinned launchers may have no discovery record. Only use a package version
// if its declared entry point or official standalone layout owns the launcher.
fn installed_version(program: &Path, probe: &Probe) -> Option<String> {
    let executable = probe.cwd.join(program).canonicalize().ok()?;
    for root in executable.ancestors().skip(1) {
        let package = root.join("package.json");
        let Ok(Some(info)) = qwen::settings(&package) else {
            continue;
        };
        if info["name"] != "@qwen-code/qwen-code" {
            continue;
        }
        let version = info["version"].as_str()?;
        if info["bin"]["qwen"]
            .as_str()
            .is_some_and(|p| root.join(p).canonicalize().ok().as_ref() == Some(&executable))
        {
            return Some(version.to_owned());
        }
        if executable
            .strip_prefix(root)
            .is_ok_and(|p| p == Path::new("bin/qwen") || p == Path::new("bin/qwen.cmd"))
            && root.join("lib/cli-entry.js").is_file()
            && qwen::settings(&root.join("manifest.json"))
                .ok()
                .flatten()
                .is_some_and(|m| m["name"] == info["name"] && m["version"] == info["version"])
        {
            return Some(version.to_owned());
        }
    }
    None
}

fn gated(probe: &Probe, args: &[OsString]) -> bool {
    let launch_flag = args.iter().take_while(|a| *a != "--").any(|a| {
        let arg = a.to_string_lossy();
        let name = arg
            .split_once('=')
            .filter(|_| arg.starts_with("--"))
            .map_or(arg.as_ref(), |(name, _)| name);
        matches!(
            name,
            "--bare"
                | "--no-bare"
                | "--safe-mode"
                | "--no-safe-mode"
                | "--safeMode"
                | "--no-safeMode"
                | "--sandbox"
                | "--no-sandbox"
        ) || arg.starts_with("-s")
    });
    launch_flag
        || [
            "QWEN_CODE_SIMPLE",
            "QWEN_CODE_SAFE_MODE",
            "QWEN_SANDBOX",
            "SANDBOX",
            "QWEN_CODE_MEMORY_TEAM",
            "QWEN_CODE_MEMORY_TEAM_SYNC",
            "QWEN_CODE_MEMORY_STRUCTURED_RECALL",
        ]
        .iter()
        .any(|name| {
            probe
                .get(name)
                .is_some_and(|v| !v.to_string_lossy().trim().is_empty())
        })
}

fn effective(
    probe: &Probe,
    args: &[OsString],
    defaults: &Value,
    system_path: &Path,
) -> Result<Option<NativeMemory>> {
    if gated(probe, args) {
        return Ok(None);
    }
    let Some(home) = profile_home(probe) else {
        return Ok(None);
    };
    let dir = qwen::config_home(probe, &home, false)?;
    let user = qwen::settings(&dir.join("settings.json"))?.unwrap_or_default();
    let system = qwen::settings(system_path)?.unwrap_or_default();
    let workspace = if normalized(&probe.cwd) == normalized(&home) {
        Value::Null
    } else {
        qwen::settings(&probe.cwd.join(".qwen/settings.json"))?.unwrap_or_default()
    };
    let trust = [&system, &user, defaults]
        .iter()
        .find_map(|s| {
            s.pointer("/security/folderTrust/enabled")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    if trust
        && KEYS
            .iter()
            .any(|(key, _)| workspace["memory"][key].is_boolean())
    {
        return Ok(None);
    }
    let layers = [&system, &workspace, &user, defaults];
    if layers.iter().any(|s| {
        ["executionSandbox", "sandbox"]
            .iter()
            .any(|key| s["tools"].get(key).is_some_and(|v| v != false))
    }) {
        return Ok(None);
    }
    let active = KEYS.iter().any(|(key, fallback)| {
        layers
            .iter()
            .find_map(|s| s["memory"][key].as_bool())
            .unwrap_or(*fallback)
    });
    Ok(Some(if active {
        NativeMemory::On
    } else {
        NativeMemory::Off
    }))
}

fn setup(
    probe: &Probe,
    args: &[OsString],
    requested: bool,
    version: Option<&str>,
    root: &Path,
) -> Result<Setup> {
    if !requested {
        let _ = PrivateDir::sweep(root, PREFIX, SIGNATURE);
    }
    let supported = version
        .and_then(|s| Version::parse(s.trim().trim_start_matches('v')).ok())
        .is_some_and(|v| v >= Version::new(0, 26, 0));
    if !supported {
        return if requested {
            Err(Error::Unsupported(
                "native_memory_default requires verified Qwen Code 0.26.0 or later".into(),
            ))
        } else {
            Ok(Setup {
                directory: None,
                effective: None,
            })
        };
    }
    let (system, defaults_path) = qwen::system_paths(probe);
    let mut defaults = match qwen::settings(&defaults_path) {
        Ok(Some(object)) => object,
        Ok(None) => json!({}),
        Err(err) if requested => return Err(err),
        Err(_) => {
            return Ok(Setup {
                directory: None,
                effective: None,
            });
        }
    };
    let directory = if requested {
        if !defaults["memory"].is_object() {
            defaults["memory"] = json!({});
        }
        for (key, _) in KEYS {
            defaults["memory"][key] = Value::Bool(false);
        }
        let dir = PrivateDir::create(root, PREFIX, SIGNATURE)?;
        let mut open = fs::OpenOptions::new();
        open.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let file = open.open(dir.path().join("system-defaults.json"))?;
        serde_json::to_writer(file, &defaults)?;
        Some(dir)
    } else {
        None
    };
    Ok(Setup {
        directory,
        effective: effective(probe, args, &defaults, &system).ok().flatten(),
    })
}

pub(crate) async fn prepare(
    runtime: &Runtime,
    options: &SessionOptions,
    version: Option<&str>,
) -> Result<(Setup, Option<String>)> {
    let requested = options.settings.native_memory_default.is_some();
    let version = if requested && version.is_none() {
        runtime.launch_version(options).await.ok()
    } else {
        version.map(String::from)
    };
    let probe = Probe::for_session(options)?;
    let program = runtime.program.clone();
    let mut args = runtime.args.clone();
    args.extend(options.launch_args.iter().map(OsString::from));
    tokio::task::spawn_blocking(move || {
        let version = version.or_else(|| {
            if requested {
                None
            } else {
                installed_version(&program, &probe)
            }
        });
        let setup = setup(
            &probe,
            &args,
            requested,
            version.as_deref(),
            &std::env::temp_dir(),
        )?;
        Ok((setup, version))
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fixture {
        root: tempfile::TempDir,
        probe: Probe,
        home: PathBuf,
        system: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().join("workspace");
            let home = root.path().join("home");
            let system = root.path().join("system.json");
            for dir in [&cwd, &home] {
                fs::create_dir_all(dir.join(".qwen")).unwrap();
            }
            let env = BTreeMap::from([
                ("HOME".into(), home.clone().into()),
                ("USERPROFILE".into(), home.clone().into()),
                (
                    "QWEN_CODE_SYSTEM_SETTINGS_PATH".into(),
                    system.clone().into(),
                ),
            ]);
            Self {
                root,
                probe: Probe { cwd, env },
                home,
                system,
            }
        }
        fn prepare(&self, requested: bool) -> Result<Setup> {
            setup(
                &self.probe,
                &[],
                requested,
                Some("0.26.0"),
                self.root.path(),
            )
        }
        fn state(&self, requested: bool) -> Option<NativeMemory> {
            self.prepare(requested).unwrap().effective
        }
    }

    #[test]
    fn merges_private_defaults_and_reclaims_only_abandoned_copies() {
        let f = Fixture::new();
        let original = "\u{feff}{/*keep*/ \"ui\":{\"theme\":\"dark\"}, \"memory\":{\"path\":\"elsewhere\",\"enableAutoSkill\":true}}";
        let defaults = f.root.path().join("system-defaults.json");
        fs::write(&defaults, original).unwrap();
        let prepared = f.prepare(true).unwrap();
        assert_eq!(prepared.effective, Some(NativeMemory::Off));
        let dir = prepared.directory.unwrap();
        let file = dir.path().join("system-defaults.json");
        let contents = qwen::settings(&file).unwrap().unwrap();
        assert_eq!(contents["ui"]["theme"], "dark");
        assert_eq!(contents["memory"]["path"], "elsewhere");
        for (key, _) in KEYS {
            assert_eq!(contents["memory"][key], false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let abandoned = f.root.path().join(format!("{PREFIX}orphan"));
        fs::create_dir(&abandoned).unwrap();
        fs::write(abandoned.join(".owner"), SIGNATURE).unwrap();
        let unrelated = f.root.path().join(format!("{PREFIX}unrelated"));
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join(".owner"), b"unrelated").unwrap();
        assert_eq!(f.state(false), Some(NativeMemory::On));
        assert!(file.is_file() && unrelated.is_dir() && !abandoned.exists());
        drop(dir);
        assert!(!file.exists());
        assert_eq!(fs::read_to_string(defaults).unwrap(), original);
    }

    #[test]
    fn merges_each_boolean_by_layer_and_respects_native_trust() {
        let f = Fixture::new();
        let user = f.home.join(".qwen/settings.json");
        let work = f.probe.cwd.join(".qwen/settings.json");
        fs::write(&user, r#"{"memory":{"enableManagedAutoMemory":true}}"#).unwrap();
        assert_eq!(f.state(true), Some(NativeMemory::On));
        fs::write(
            &work,
            r#"{"memory":{"enableManagedAutoMemory":false,"enableAutoSkill":true}}"#,
        )
        .unwrap();
        fs::write(&f.system, r#"{"memory":{"enableAutoSkill":false}}"#).unwrap();
        assert_eq!(f.state(true), Some(NativeMemory::Off));
        fs::write(
            &f.system,
            r#"{"security":{"folderTrust":{"enabled":true}}}"#,
        )
        .unwrap();
        assert_eq!(f.state(true), None);
        fs::write(&work, r#"{"memory":{"enableAutoSkill":"true"}}"#).unwrap();
        assert_eq!(f.state(true), Some(NativeMemory::On));
        fs::write(&user, r#"{"tools":{"executionSandbox":null}}"#).unwrap();
        assert_eq!(f.state(true), None);
        fs::write(&user, "[]").unwrap();
        assert_eq!(f.state(true), None);
        let defaults = f.root.path().join("system-defaults.json");
        fs::write(defaults, "{").unwrap();
        assert!(f.prepare(true).is_err());
        assert_eq!(f.state(false), None);
    }

    #[test]
    fn handles_dotenv_home_workspace_identity_and_runtime_gates() {
        let mut f = Fixture::new();
        let config = f.home.join("qwen account");
        fs::create_dir(&config).unwrap();
        fs::write(f.home.join(".qwen/.env"), "QWEN_HOME='~/qwen account'").unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{"memory":{"enableAutoSkill":true}}"#,
        )
        .unwrap();
        fs::write(
            f.home.join(".qwen/settings.json"),
            r#"{"memory":{"enableAutoSkill":false}}"#,
        )
        .unwrap();
        f.probe.cwd = f.home.clone();
        assert_eq!(f.state(true), Some(NativeMemory::On));
        for arg in ["--bare", "--no-safeMode", "--sandbox=false", "-sany"] {
            assert!(gated(&f.probe, &[arg.into()]));
            assert!(!gated(&f.probe, &["--".into(), arg.into()]));
        }
        f.probe.env.insert("QWEN_SANDBOX".into(), "  ".into());
        assert_eq!(f.state(true), Some(NativeMemory::On));
        f.probe.env.insert("QWEN_SANDBOX".into(), "false".into());
        assert_eq!(f.state(true), None);
        for version in [Some("0.25.9"), Some("0.26.0-rc.1"), Some("unknown"), None] {
            assert!(matches!(
                setup(&f.probe, &[], true, version, f.root.path()),
                Err(Error::Unsupported(_))
            ));
            assert!(
                setup(&f.probe, &[], false, version, f.root.path())
                    .unwrap()
                    .effective
                    .is_none()
            );
        }
    }
}
