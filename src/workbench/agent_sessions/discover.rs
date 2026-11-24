//! Local discovery within host-configured session homes. No native CLI or model is launched.
use super::Workbench;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};
pub(super) const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Source {
    Claude,
    Codex,
    Pi,
}
impl Source {
    pub fn runtime(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }
    fn home(self) -> Option<PathBuf> {
        let (names, default): (&[&str], &str) = match self {
            Self::Codex => (&["CODEX_HOME"], ".codex"),
            Self::Claude => (&["CLAUDE_CONFIG_DIR"], ".claude"),
            Self::Pi => (&["KAR_PI_AGENT_DIR", "PI_CODING_AGENT_DIR"], ".pi/agent"),
        };
        let home = names
            .iter()
            .find_map(|k| env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from))
            .or_else(|| env::home_dir().map(|p| p.join(default)))?;
        Some(home.join(if self == Self::Claude {
            "projects"
        } else {
            "sessions"
        }))
    }
}

#[derive(Clone)]
pub(super) struct Found {
    pub source: Source,
    base: Arc<Dir>,
    relative: PathBuf,
    pub path: PathBuf,
    pub stamp: String,
    pub size: u64,
    pub modified: u64,
    pub cwd: PathBuf,
    pub original_cwd: PathBuf,
    pub codex_title: Option<String>,
    pub project: PathBuf,
}
fn stamp(m: &Metadata) -> String {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", m.dev(), m.ino())
    };
    #[cfg(not(unix))]
    let identity = String::new();
    format!(
        "{}:{}:{}:{}",
        identity,
        m.created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos()),
        m.len(),
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    )
}

fn regular(dir: &Dir, name: &Path) -> io::Result<File> {
    if !dir.symlink_metadata(name)?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut opts = OpenOptions::new();
    opts.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        opts.custom_flags(nix::libc::O_NONBLOCK);
    }
    let file = dir.open_with(name, &opts)?.into_std();
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}
impl Found {
    pub fn file(&self) -> io::Result<File> {
        let mut dir = self.base.try_clone()?;
        for part in self.relative.parent().unwrap_or(Path::new("")).components() {
            dir = dir.open_dir_nofollow(part)?;
        }
        let f = regular(&dir, Path::new(self.relative.file_name().unwrap()))?;
        self.check(&f)?;
        Ok(f)
    }
    pub fn check(&self, file: &File) -> io::Result<()> {
        if stamp(&file.metadata()?) != self.stamp {
            return Err(io::Error::other("native transcript changed during import"));
        }
        Ok(())
    }
}
pub(super) fn cwd(row: &Value, source: Source) -> Result<Option<PathBuf>, ()> {
    if source == Source::Codex
        && row["type"] == "session_meta"
        && row["payload"]["source"]
            .as_object()
            .is_some_and(|s| s.contains_key("subagent") || s.contains_key("subAgent"))
    {
        return Err(());
    }
    Ok(row["cwd"]
        .as_str()
        .or_else(|| row["payload"]["cwd"].as_str())
        .and_then(|s| {
            let p = Path::new(s.trim());
            if let Ok(suffix) = p.strip_prefix("~") {
                std::env::home_dir().map(|h| h.join(suffix))
            } else {
                p.is_absolute().then(|| p.to_owned())
            }
        }))
}
pub(super) fn bounded_file(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let parent = Dir::open_ambient_dir(path.parent()?, ambient_authority()).ok()?;
    let file = regular(&parent, Path::new(path.file_name()?)).ok()?;
    let meta = file.metadata().ok()?;
    if meta.len() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    (&file).take(limit + 1).read_to_end(&mut bytes).ok()?;
    (stamp(&file.metadata().ok()?) == stamp(&meta) && bytes.len() as u64 <= limit).then_some(bytes)
}
pub(super) fn input<'a>(file: &'a File, path: &Path) -> io::Result<Box<dyn Read + 'a>> {
    if path.extension().is_some_and(|x| x == "zst") {
        let mut decoder = ruzstd::decoding::FrameDecoder::new();
        decoder.set_max_window_size(64 * MIB);
        Ok(Box::new(
            ruzstd::decoding::StreamingDecoder::new_with_decoder(file, decoder)
                .map_err(io::Error::other)?,
        ))
    } else {
        Ok(Box::new(file))
    }
}
fn excluded(p: &Path) -> bool {
    let home = env::home_dir();
    p == Path::new("/tmp")
        || p == Path::new("/private/tmp")
        || p == env::temp_dir()
        || home.is_some_and(|h| {
            p == h || p.starts_with(h.join("Downloads")) || p.starts_with(h.join("Documents/Codex"))
        })
}
#[derive(serde::Serialize)]
pub(super) struct Git {
    remote_key: Option<String>,
    repository: Option<String>,
}
pub(super) fn git(root: &Path) -> Option<Git> {
    if !root.join(".git").symlink_metadata().ok()?.is_dir() {
        return None;
    }
    let remote = (|| {
        let bytes = bounded_file(&root.join(".git/config"), MIB)?;
        let file = gix::config::File::from_bytes_no_includes(
            &bytes,
            gix::config::file::Metadata::default(),
            Default::default(),
        )
        .ok()?;
        let url = file.string("remote.origin.url")?;
        super::super::forge::remote_identity(std::str::from_utf8(url.as_ref()).ok()?).ok()
    })();
    Some(Git {
        remote_key: remote.as_ref().map(|(h, r)| format!("{h}/{r}")),
        repository: remote.map(|(_, r)| r),
    })
}
fn text_file(path: &Path) -> Option<String> {
    let parent = Dir::open_ambient_dir(path.parent()?, ambient_authority()).ok()?;
    let f = regular(&parent, Path::new(path.file_name()?)).ok()?;
    if f.metadata().ok()?.len() > MIB {
        return None;
    }
    let mut line = String::new();
    f.take(MIB + 1).read_to_string(&mut line).ok()?;
    Some(line)
}
pub(super) fn still_belongs(cwd: &Path, root: &Path) -> bool {
    cwd == root || project_root(cwd) == root
}
fn project_root(cwd: &Path) -> PathBuf {
    let dot = cwd.join(".git");
    let main = (|| {
        let gitfile = text_file(&dot)?;
        let admin = cwd
            .join(gitfile.trim().strip_prefix("gitdir:")?.trim())
            .canonicalize()
            .ok()?;
        let backlink = admin
            .join(text_file(&admin.join("gitdir"))?.trim())
            .canonicalize()
            .ok()?;
        if backlink != dot {
            return None;
        }
        let common = admin
            .join(text_file(&admin.join("commondir"))?.trim())
            .canonicalize()
            .ok()?;
        if admin.parent()? != common.join("worktrees") || common.file_name()? != ".git" {
            return None;
        }
        Some(common.parent()?.to_owned())
    })();
    main.unwrap_or_else(|| cwd.to_owned())
}

struct Budget {
    entries: usize,
    files: usize,
    bytes: u64,
    records: usize,
    truncated: bool,
}
impl Budget {
    fn names(&mut self, dir: &Dir) -> Vec<OsString> {
        let Ok(entries) = dir.entries() else {
            return vec![];
        };
        let mut names = vec![];
        for entry in entries.flatten() {
            if self.entries == 0 {
                self.truncated = true;
                break;
            }
            self.entries -= 1;
            names.push(entry.file_name());
        }
        names.sort_unstable_by(|a, b| b.cmp(a));
        names
    }
}
struct Home {
    source: Source,
    path: PathBuf,
    root: Arc<Dir>,
    codex_projects: BTreeMap<String, PathBuf>,
    codex_titles: BTreeMap<String, String>,
}
impl Home {
    fn walk(
        &self,
        dir: &Dir,
        rel: &Path,
        depth: usize,
        budget: &mut Budget,
        files: &mut Vec<Found>,
    ) {
        for name in budget.names(dir) {
            if budget.files == 0 || budget.bytes == 0 || budget.records == 0 {
                budget.truncated = true;
                break;
            }
            let part = Path::new(&name);
            let relative = rel.join(part);
            let max = match self.source {
                Source::Codex => 3,
                _ => 1,
            };
            if depth < max
                && let Ok(sub) = dir.open_dir_nofollow(part)
            {
                self.walk(&sub, &relative, depth + 1, budget, files);
                continue;
            }
            let filename = name.to_string_lossy();
            if !(filename.ends_with(".jsonl")
                || (self.source == Source::Codex && filename.ends_with(".jsonl.zst")))
                || (self.source != Source::Pi && depth != max)
                || (self.source == Source::Codex && !filename.starts_with("rollout-"))
            {
                continue;
            }
            let Ok(file) = regular(dir, part) else {
                continue;
            };
            let Ok(meta) = file.metadata() else { continue };
            budget.files -= 1;
            let size = meta.len();
            let Ok(stream) = input(&file, part) else {
                continue;
            };
            let allowance = MIB.min(budget.bytes);
            let mut reader = BufReader::new(stream.take(allowance));
            let mut line = vec![];
            let mut complete = false;
            for _ in 0..1000.min(budget.records) {
                line.clear();
                let Ok(n) = reader.read_until(b'\n', &mut line) else {
                    break;
                };
                budget.bytes -= n as u64;
                if n == 0 {
                    complete = reader.get_ref().limit() != 0
                        || (part.extension().is_none_or(|e| e != "zst") && size <= allowance);
                    break;
                }
                budget.records -= 1;
                let Ok(row) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let raw = match cwd(&row, self.source) {
                    Err(_) => {
                        complete = true;
                        break;
                    }
                    Ok(Some(c)) => c,
                    _ => continue,
                };
                let id = row
                    .pointer("/payload/id")
                    .or_else(|| row.pointer("/payload/session_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let workspace = if excluded(&raw) {
                    self.codex_projects.get(id).map(PathBuf::as_path)
                } else {
                    Some(raw.as_path())
                };
                let Some(cwd) = workspace
                    .and_then(|p| p.canonicalize().ok())
                    .filter(|p| !excluded(p))
                else {
                    complete = true;
                    break;
                };
                let original_cwd = raw.to_owned();
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as u64);
                files.push(Found {
                    source: self.source,
                    base: self.root.clone(),
                    relative: relative.clone(),
                    path: self.path.join(&relative),
                    stamp: stamp(&meta),
                    size,
                    modified,
                    project: PathBuf::new(),
                    original_cwd,
                    codex_title: self.codex_titles.get(id).cloned(),
                    cwd,
                });
                complete = true;
                break;
            }
            if !complete {
                budget.truncated = true;
            }
        }
    }
}
pub(super) fn discover(app: &Workbench) -> (Vec<Found>, bool) {
    let mut found = vec![];
    let mut truncated = false;
    for source in [Source::Claude, Source::Codex, Source::Pi] {
        let Some(path) = source.home().and_then(|p| p.canonicalize().ok()) else {
            continue;
        };
        let Ok(dir) = Dir::open_ambient_dir(&path, ambient_authority()) else {
            continue;
        };
        let mut codex_projects = BTreeMap::new();
        let mut codex_titles = BTreeMap::new();
        if source == Source::Codex
            && let Some(parent) = path.parent()
        {
            codex_projects.extend(super::codex::project_roots(parent).unwrap_or_default());
            codex_titles.extend(super::codex::titles(parent));
        }
        let home = Home {
            source,
            path,
            root: Arc::new(dir),
            codex_projects,
            codex_titles,
        };
        let mut budget = Budget {
            entries: 20000,
            files: 5000,
            bytes: 64 * MIB,
            records: 100000,
            truncated: false,
        };
        home.walk(&home.root, Path::new(""), 0, &mut budget, &mut found);
        truncated |= budget.truncated;
    }
    let mut roots = BTreeMap::<PathBuf, Option<PathBuf>>::new();
    found.retain_mut(|f| {
        let root = roots.entry(f.cwd.clone()).or_insert_with(|| {
            app.workspace_at(&f.cwd).ok()?;
            let main = project_root(&f.cwd);
            app.workspace_at(&main).ok()?;
            Some(main)
        });
        if let Some(root) = root {
            f.project = root.clone();
            true
        } else {
            false
        }
    });
    found.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    (found, truncated)
}
