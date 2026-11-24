use super::{Candidate, Probe};
use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.is_file() && nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
#[cfg(unix)]
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = path.metadata().ok()?;
    Some((m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn identity(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

impl Probe {
    pub(super) fn locate(&self, name: &Path, pinned: bool) -> Option<Candidate> {
        if name.is_absolute() || name.components().count() > 1 {
            let path = self.cwd.join(name);
            return path.is_file().then(|| Candidate::new(path));
        }
        let path = self.get("PATH").map(OsString::from).unwrap_or_else(|| {
            if cfg!(windows) {
                "".into()
            } else {
                "/usr/bin:/bin".into()
            }
        });
        if path.is_empty() {
            return None;
        }
        let names = self.names(name);
        let mut files = HashSet::new();
        let mut matches = Vec::new();
        for dir in std::env::split_paths(&path) {
            let base = self.cwd.join(dir);
            if let Some(found) = names.iter().map(|n| base.join(n)).find(|p| executable(p))
                && let Some(id) = identity(&found)
                && files.insert(id)
            {
                matches.push(found);
                if pinned {
                    break;
                }
            }
        }
        let mut matches = matches.into_iter();
        Some(Candidate {
            command: matches.next()?,
            shadowed: matches.collect(),
        })
    }
    fn names(&self, name: &Path) -> Vec<PathBuf> {
        #[cfg(windows)]
        {
            let ext = self
                .get("PATHEXT")
                .unwrap_or(std::ffi::OsStr::new(".EXE;.CMD;.BAT;.COM"))
                .to_string_lossy();
            if ext.is_empty()
                || name.extension().is_some_and(|x| {
                    ext.split(';').any(|e| {
                        e.trim_start_matches('.')
                            .eq_ignore_ascii_case(&x.to_string_lossy())
                    })
                })
            {
                return vec![name.into()];
            }
            ext.split(';')
                .filter(|e| !e.is_empty())
                .map(|e| {
                    let mut s = name.as_os_str().to_owned();
                    s.push(e);
                    PathBuf::from(s)
                })
                .collect()
        }
        #[cfg(not(windows))]
        {
            vec![name.into()]
        }
    }
    pub(super) fn fallbacks(&self, id: &str) -> Vec<PathBuf> {
        let folder = |k| self.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = folder(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).or_else(account_home);
        let mut paths = vec![];
        let name = |n: &str| {
            if cfg!(windows) {
                format!("{n}.exe")
            } else {
                n.to_owned()
            }
        };
        match id {
            "codex" => {
                #[cfg(windows)]
                {
                    let roaming =
                        folder("APPDATA").or_else(|| Some(home.as_ref()?.join("AppData/Roaming")));
                    paths.extend(roaming.map(|d| d.join("npm/codex.cmd")));
                    let mut seen = HashSet::new();
                    for local in [
                        folder("LOCALAPPDATA"),
                        folder("USERPROFILE").map(|p| p.join("AppData/Local")),
                        home.as_ref().map(|p| p.join("AppData/Local")),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if !seen.insert(local.to_string_lossy().to_lowercase()) {
                            continue;
                        }
                        for base in ["Programs/OpenAI/Codex/bin", "OpenAI/Codex/bin"] {
                            let dir = local.join(base);
                            paths.push(dir.join("codex.exe"));
                            paths.extend(builds(&dir));
                        }
                    }
                }
                #[cfg(not(windows))]
                {
                    #[cfg(target_os = "macos")]
                    for base in [
                        Some(PathBuf::from("/Applications")),
                        home.as_ref().map(|h| h.join("Applications")),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        for app in ["ChatGPT.app", "Codex.app"] {
                            let resources = base.join(app).join("Contents/Resources");
                            paths.push(resources.join("codex"));
                            paths.push(
                                resources.join("codex-cli/CodexCLI.app/Contents/MacOS/codex"),
                            );
                        }
                    }
                    paths.extend(folder("CODEX_INSTALL_DIR").map(|dir| dir.join("codex")));
                    paths.extend(home.map(|h| h.join(".local/bin/codex")));
                }
            }
            "claude" | "factory" | "codebuddy" | "qoder" => {
                let bin = match id {
                    "factory" => "droid",
                    "qoder" => "qodercli",
                    _ => id,
                };
                paths.extend(home.map(|h| h.join(".local/bin").join(name(bin))));
            }
            "devin" | "antigravity" => {
                let bin = if id == "devin" { "devin" } else { "agy" };
                if cfg!(windows) {
                    let local = folder("LOCALAPPDATA")
                        .or_else(|| Some(home.as_ref()?.join("AppData/Local")));
                    paths.extend(local.map(|p| {
                        p.join(if id == "devin" {
                            "devin/cli/bin"
                        } else {
                            "agy/bin"
                        })
                        .join(name(bin))
                    }));
                } else {
                    paths.extend(home.map(|h| h.join(".local/bin").join(bin)));
                }
            }
            "kimi" => {
                paths.extend(folder("KIMI_INSTALL_DIR").map(|d| d.join("bin").join(name("kimi"))));
                paths.extend(home.map(|h| h.join(".kimi-code/bin").join(name("kimi"))));
                paths.push("kimi-code".into());
            }
            "grok" => {
                paths.extend(folder("GROK_BIN_DIR").map(|p| p.join(name("grok"))));
                paths.extend(folder("GROK_HOME").map(|p| p.join("bin").join(name("grok"))));
                paths.extend(home.map(|p| p.join(".grok/bin").join(name("grok"))));
            }
            "opencode" => {
                paths.extend(home.map(|p| p.join(".opencode/bin").join(name("opencode"))));
            }
            "qwen" if !cfg!(windows) => {
                let root = folder("QWEN_INSTALL_ROOT").or_else(|| home.map(|p| p.join(".local")));
                paths.extend(
                    folder("QWEN_INSTALL_BIN_DIR")
                        .or_else(|| root.map(|p| p.join("bin")))
                        .map(|p| p.join("qwen")),
                );
            }
            _ => {}
        }
        paths
    }
}
#[cfg(any(windows, test))]
pub(super) fn builds(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            if e.file_name().to_string_lossy().starts_with('.') || !e.file_type().ok()?.is_dir() {
                return None;
            }
            let path = e.path().join("codex.exe");
            let m = path.metadata().ok()?;
            m.is_file().then_some((m.modified().ok()?, path))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    entries.into_iter().take(3).map(|(_, p)| p).collect()
}

#[cfg(unix)]
pub(crate) fn account_home() -> Option<PathBuf> {
    nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|user| user.dir)
}
#[cfg(not(unix))]
pub(crate) fn account_home() -> Option<PathBuf> {
    // std reads the parent USERPROFILE first. A removed override must not
    // bring that path back when computing a native session's default.
    if std::env::var_os("USERPROFILE").is_some() {
        None
    } else {
        std::env::home_dir()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn newest_three_desktop_builds_exclude_helpers() {
        use std::{
            fs,
            time::{Duration, UNIX_EPOCH},
        };
        let dir = tempfile::tempdir().unwrap();
        for (i, build) in ["a", "b", "c", "d", ".sandbox-bin"].into_iter().enumerate() {
            let path = dir.path().join(build);
            fs::create_dir(&path).unwrap();
            fs::File::create(path.join("codex.exe"))
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000 + i as u64))
                .unwrap();
        }
        let found = super::builds(dir.path());
        assert_eq!(
            found,
            ["d", "c", "b"].map(|s| dir.path().join(s).join("codex.exe"))
        );
    }
}
