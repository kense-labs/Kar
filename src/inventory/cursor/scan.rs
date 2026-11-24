use super::*;
use crate::process::OwnedProcess;
use std::{io::ErrorKind, process::Stdio};
use tokio::io::AsyncReadExt;

fn missing(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

pub(super) async fn direct(root: PathBuf, partial: &mut bool) -> Result<Vec<PathBuf>> {
    let mut pending = vec![(root, 0)];
    let mut visited = HashSet::new();
    let mut files = Vec::new();
    while let Some((dir, depth)) = pending.pop() {
        let contents = async {
            if !visited.insert(fs::canonicalize(&dir).await?) {
                return Ok(());
            }
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                let native = entry.file_type().await?;
                let kind = if native.is_symlink() {
                    let Ok(meta) = fs::metadata(entry.path()).await else {
                        continue;
                    };
                    meta.file_type()
                } else {
                    native
                };
                if kind.is_dir()
                    && depth < 10
                    && !name.to_string_lossy().starts_with('.')
                    && !matches!(
                        name.to_str(),
                        Some("node_modules" | "__pycache__" | "dist" | "build")
                    )
                {
                    pending.push((entry.path(), depth + 1));
                }
                if kind.is_file() && name == "SKILL.md" {
                    files.push(entry.path());
                }
            }
            Ok::<_, std::io::Error>(())
        }
        .await;
        if contents.as_ref().is_err_and(|e| !missing(e)) {
            *partial = true;
        }
    }
    let order = icu_collator::Collator::try_new(Default::default(), Default::default())
        .map_err(|_| invalid())?;
    files.sort_by(|a, b| order.compare(&a.to_string_lossy(), &b.to_string_lossy()));
    Ok(files)
}
fn nested_allowed(cwd: &Path, home: &Path) -> bool {
    if cwd == home {
        return false;
    }
    if cfg!(target_os = "macos") {
        let cwd = PathBuf::from(cwd.to_string_lossy().to_lowercase());
        return ![
            "Desktop",
            "Documents",
            "Downloads",
            "Music",
            "Movies",
            "Pictures",
            "Library",
            ".Trash",
        ]
        .iter()
        .map(|n| home.join(n))
        .chain(std::iter::once(PathBuf::from("/Volumes")))
        .map(|p| PathBuf::from(p.to_string_lossy().to_lowercase()))
        .any(|p| p != cwd && p.starts_with(&cwd));
    }
    true
}

async fn git_workspace(probe: &Probe) -> bool {
    let mut git = probe.command(Path::new("git"));
    git.args(["-c", "core.fsmonitor=false", "rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = OwnedProcess::spawn(git) else {
        return false;
    };
    child.wait().await.is_ok_and(|s| s.success())
}

pub(super) async fn nested(
    rt: &Runtime,
    probe: &Probe,
    home: &Path,
    partial: &mut bool,
) -> Result<Vec<PathBuf>> {
    if !nested_allowed(&probe.cwd, home) || !git_workspace(probe).await {
        return Ok(vec![]);
    }
    let companion = probe
        .get("CURSOR_RIPGREP_PATH")
        .map(|p| probe.cwd.join(p))
        .or_else(|| probe.companion(rt, if cfg!(windows) { "rg.exe" } else { "rg" }));
    let Some(companion) = companion else {
        *partial = true;
        return Ok(vec![]);
    };
    let mut cmd = probe.command(&companion);
    cmd.args([
        "--files",
        "--hidden",
        "--no-require-git",
        "--no-config",
        "--color=never",
        "--threads",
        "2",
        "--follow",
    ]);
    for brand in BRANDS {
        cmd.arg("--iglob").arg(format!("*/**/{brand}/**"));
    }
    for name in [
        "node_modules",
        ".git",
        ".turbo",
        ".next",
        ".cache",
        ".pnpm",
        ".yarn",
        "dist",
        "build",
        "out",
        "target",
        ".vscode",
        ".idea",
        "venv",
        "__pycache__",
        "logs",
        "tmp",
        "temp",
    ] {
        cmd.arg("--iglob").arg(format!("!**/{name}/**"));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let read = async {
        let mut child = OwnedProcess::spawn(cmd)?;
        let mut output = child.inner().stdout.take().expect("piped stdout");
        let mut buf = Vec::new();
        const MAX: u64 = 2 * 1024 * 1024;
        (&mut output).take(MAX).read_to_end(&mut buf).await?;
        if buf.len() as u64 == MAX {
            *partial = true;
            let line = buf.iter().rposition(|c| *c == b'\n').map_or(0, |i| i + 1);
            buf.truncate(line);
            tokio::io::copy(&mut output, &mut tokio::io::sink()).await?;
        }
        if !matches!(child.wait().await?.code(), Some(0 | 1)) {
            *partial = true;
        }
        Ok::<_, std::io::Error>(buf)
    }
    .await;
    let Ok(bytes) = read else {
        *partial = true;
        return Ok(vec![]);
    };
    static SKILL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)[/\\]\.(?:cursor[/\\]skills(?:-cursor)?|(?:claude|codex|grok|agents)[/\\]skills)[/\\](?:.*[/\\])?SKILL\.md$").unwrap()
    });
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| SKILL.is_match(line))
        .map(|s| probe.cwd.join(s))
        .collect())
}
