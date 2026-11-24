use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
};
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct FileUsage {
    pub root: PathBuf,
    pub format: String,
    pub entries: Vec<Entry>,
    parser: records::Parser,
    signature: String,
    identity: String,
    offset: u64,
    guard: Vec<u8>,
    malformed: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Source {
    pub root: PathBuf,
    pub provider: String,
    fingerprint: Value,
    status: String,
    scanned_files: u64,
    skipped_files: u64,
    malformed_records: u64,
    message: Option<String>,
}
#[derive(Default)]
pub(super) struct Snapshot {
    pub sources: Vec<Source>,
    pub files: BTreeMap<PathBuf, Arc<FileUsage>>,
    pub at: i64,
    pub duration: u64,
}
impl Snapshot {
    pub fn source_id(&self, root: &Path, provider: &str) -> Option<usize> {
        self.sources
            .iter()
            .position(|s| s.root == root && s.provider == provider)
    }
}
fn home_dir(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default)
}
fn roots(chat: PathBuf) -> Vec<(&'static str, PathBuf)> {
    let mut dirs = vec![("kar", chat)];
    if let Some(home) = std::env::home_dir() {
        let codex = home_dir("CODEX_HOME", home.join(".codex"));
        dirs.extend([
            (
                "claude",
                home_dir("CLAUDE_CONFIG_DIR", home.join(".claude")).join("projects"),
            ),
            ("codex", codex.join("sessions")),
            ("codex", codex.join("archived_sessions")),
            (
                "pi",
                home_dir("PI_CODING_AGENT_DIR", home.join(".pi/agent")).join("sessions"),
            ),
            (
                "grok",
                home_dir("GROK_HOME", home.join(".grok")).join("sessions"),
            ),
        ]);
        if let Some(home) = std::env::var_os("MUSE_HOME") {
            dirs.push(("muse", PathBuf::from(home).join("sessions")));
        }
    }
    dirs
}
#[cfg(unix)]
fn identity(m: &Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", m.dev(), m.ino())
}
#[cfg(not(unix))]
fn identity(m: &Metadata) -> String {
    format!("{:?}", m.created().ok())
}
fn signature(m: &Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}",
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec()
        )
    }
    #[cfg(not(unix))]
    {
        format!("{}:{:?}", m.len(), m.modified().ok())
    }
}
fn host() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(n) = sysinfo::System::host_name() {
            return n;
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into())
}
fn source(root: &Path, provider: &str) -> Source {
    let volume = fs::metadata(root)
        .ok()
        .map(|m| identity(&m))
        .unwrap_or_default();
    Source {
        root: root.into(),
        provider: provider.into(),
        status: "ok".into(),
        fingerprint: json!({"host_id":host(),"provider":provider,"resolved_home_path":root,"volume_id":volume}),
        ..Default::default()
    }
}
fn guard(file: &mut File, offset: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    if io::copy(&mut file.take(offset), &mut digest)? != offset {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "usage journal changed during scan",
        ));
    }
    Ok(digest.finalize().to_vec())
}
const MAX_LINE: usize = 32 << 20;
fn read_file(
    path: &Path,
    root: &Path,
    format: &str,
    old: Option<&Arc<FileUsage>>,
) -> io::Result<(Arc<FileUsage>, bool)> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let id = identity(&meta);
    let sig = signature(&meta);
    let old = old.filter(|o| o.identity == id && o.format == format && o.root == root);
    if let Some(o) = old.filter(|x| x.signature == sig) {
        return Ok((o.clone(), true));
    }
    let previous = match old {
        Some(o)
            if o.offset > 0 && meta.len() > o.offset && guard(&mut file, o.offset)? == o.guard =>
        {
            Some(o.as_ref().clone())
        }
        _ => None,
    };
    let mut result = previous.unwrap_or_else(|| FileUsage {
        root: root.into(),
        format: format.into(),
        entries: vec![],
        parser: Default::default(),
        signature: String::new(),
        identity: id,
        offset: 0,
        guard: vec![],
        malformed: 0,
    });
    file.seek(SeekFrom::Start(result.offset))?;
    let mut reader = BufReader::new(&mut file);
    let mut line = Vec::new();
    let mut bytes = 0u64;
    let mut oversize = false;
    loop {
        let block = reader.fill_buf()?;
        if block.is_empty() {
            break;
        }
        let newline = block.iter().position(|b| *b == b'\n');
        let count = newline.map_or(block.len(), |i| i + 1);
        if line.len().saturating_add(count) > MAX_LINE {
            oversize = true;
        }
        if !oversize {
            line.extend_from_slice(&block[..count]);
        }
        reader.consume(count);
        bytes += count as u64;
        if newline.is_none() {
            continue;
        }
        if oversize {
            result.malformed += 1;
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            match serde_json::from_slice::<Value>(&line) {
                Ok(v) => result.entries.extend(result.parser.parse(format, &v)),
                Err(_) => result.malformed += 1,
            }
        }
        result.offset += bytes;
        bytes = 0;
        oversize = false;
        line.clear();
    }
    result.guard = guard(&mut file, result.offset)?;
    result.signature = sig;
    Ok((Arc::new(result), false))
}
fn scan(root: &Path, format: &str, files: &mut BTreeMap<PathBuf, Arc<FileUsage>>) -> Source {
    let mut report = source(root, format);
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(e) => {
                if dir == root {
                    report.status = if e.kind() == io::ErrorKind::NotFound {
                        "missing"
                    } else {
                        "failed"
                    }
                    .into();
                } else {
                    report.status = "partial".into();
                }
                if e.kind() != io::ErrorKind::NotFound {
                    report.message = Some(e.to_string());
                }
                continue;
            }
        };
        for item in entries {
            let Ok(e) = item else {
                report.status = "partial".into();
                continue;
            };
            let Ok(kind) = e.file_type() else {
                report.status = "partial".into();
                continue;
            };
            let path = e.path();
            if kind.is_dir() {
                if format != "kar" {
                    pending.push(path);
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let matched = match format {
                "kar" => path.extension().is_some_and(|e| e == "karchat"),
                "grok" => e.file_name() == "updates.jsonl",
                "muse" => e.file_name() == "session.jsonl",
                _ => path.extension().is_some_and(|e| e == "jsonl"),
            };
            if !matched {
                continue;
            }
            match read_file(&path, root, format, files.get(&path)) {
                Ok((f, cached)) => {
                    if cached {
                        report.skipped_files += 1;
                    } else {
                        report.scanned_files += 1;
                    }
                    report.malformed_records += f.malformed + f.parser.malformed();
                    files.insert(path, f);
                }
                Err(e) => {
                    report.status = "partial".into();
                    report.message = Some(e.to_string());
                }
            }
        }
    }
    if report.malformed_records > 0 && report.status == "ok" {
        report.status = "partial".into();
        report.message = Some("some usage records were malformed".into());
    }
    report
}
pub(super) fn read(
    file: &Path,
    previous: Option<Arc<Snapshot>>,
    chat: PathBuf,
) -> Result<Snapshot> {
    let start = Instant::now();
    let mut files = previous.map(|p| p.files.clone()).unwrap_or_else(|| {
        File::open(file)
            .ok()
            .and_then(|f| {
                serde_json::from_reader::<_, BTreeMap<PathBuf, FileUsage>>(BufReader::new(f)).ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, Arc::new(v)))
            .collect()
    });
    let mut sources = vec![];
    let mut changed = false;
    let roots: BTreeSet<_> = roots(chat)
        .into_iter()
        .map(|(f, p)| (f, fs::canonicalize(&p).unwrap_or(p)))
        .collect();
    // Changing the configured provider home must not surface usage from a disconnected account.
    files.retain(|_, f| roots.contains(&(f.format.as_str(), f.root.clone())));
    for (format, root) in roots {
        let report = scan(&root, format, &mut files);
        changed |= report.scanned_files > 0;
        if format == "kar" {
            let providers: BTreeSet<_> = files
                .values()
                .filter(|f| f.root == root && f.format == "kar")
                .flat_map(|f| f.entries.iter().map(|e| e.provider.as_str()))
                .collect();
            for provider in providers {
                let mut s = report.clone();
                s.provider = provider.into();
                s.fingerprint["provider"] = json!(provider);
                sources.push(s);
            }
        } else {
            sources.push(report);
        }
    }
    if changed {
        let save: BTreeMap<_, _> = files.iter().map(|(p, f)| (p, f.as_ref())).collect();
        super::super::triage::atomic(file, &serde_json::to_vec(&save)?)?;
    }
    Ok(Snapshot {
        files,
        sources,
        at: now(),
        duration: start.elapsed().as_millis() as u64,
    })
}
