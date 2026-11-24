//! Materialize regular files from the exact Git object. Archive extraction does not execute
//! checkout filters and never materializes symlinks, submodules or provider configuration.
use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
};
pub(super) struct Checkout {
    _dir: tempfile::TempDir,
    pub cwd: PathBuf,
    pub diff: String,
    pub lines: BTreeMap<String, (usize, usize)>,
}
fn sha(s: &str) -> Result<(), ForgeError> {
    if matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ForgeError::bad(
            "review guide requires immutable head and base commit hashes",
        ))
    }
}
fn safe(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c|matches!(c,Component::Normal(n) if !matches!(n.to_str().map(str::to_ascii_lowercase).as_deref(),Some(".git"|".codex"|".claude"|".mcp.json"|"agents.md"|"claude.md"))))
}
fn bounded(cwd: &Path, args: &[&str], max: usize) -> Result<Vec<u8>, ForgeError> {
    let mut cmd = crate::workbench_git::command(cwd);
    let mut p = cmd
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::from)?;
    let mut bytes = vec![];
    let result = p
        .stdout
        .take()
        .unwrap()
        .take(max as u64 + 1)
        .read_to_end(&mut bytes);
    if result.is_err() || bytes.len() > max {
        let _ = p.kill();
    }
    let status = p.wait().map_err(Error::from)?;
    result.map_err(Error::from)?;
    if !status.success() || bytes.len() > max {
        return Err(ForgeError::bad(
            "review checkout Git operation failed or exceeded its size budget",
        ));
    }
    Ok(bytes)
}
pub(super) fn prepare(
    root: &Path,
    repo: &str,
    host: &str,
    head: &str,
    base: &str,
) -> Result<Checkout, ForgeError> {
    sha(head)?;
    sha(base)?;
    let (remote, selected_host, selected_repo) = primary_remote(root)?;
    if selected_host != host || !selected_repo.eq_ignore_ascii_case(repo) {
        return Err(ForgeError::bad("guide repository identity changed"));
    }
    for sha in [head, base] {
        let oid = format!("{sha}^{{commit}}");
        if bounded(root, &["cat-file", "-e", &oid], 1024).is_err() {
            bounded(
                root,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--",
                    &remote,
                    sha,
                ],
                1024 * 1024,
            )?;
        }
        let got = bounded(root, &["rev-parse", "--verify", &oid], 256)?;
        if String::from_utf8_lossy(&got).trim() != sha {
            return Err(ForgeError::bad("guide commit could not be verified"));
        }
    }
    let dir = tempfile::tempdir().map_err(Error::from)?;
    let cwd = dir.path().join("tree");
    std::fs::create_dir(&cwd).map_err(Error::from)?;
    let mut child = crate::workbench_git::command(root)
        .args(["archive", "--format=tar", head])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::from)?;
    let extracted = (|| -> crate::Result<()> {
        let mut archive = tar::Archive::new(child.stdout.take().unwrap());
        let mut size = 0u64;
        let mut count = 0usize;
        for entry in archive.entries()? {
            let mut e = entry?;
            let path = e.path()?.into_owned();
            if !e.header().entry_type().is_file() || !safe(&path) {
                continue;
            }
            size = size.saturating_add(e.size());
            count += 1;
            if size > 512 * 1024 * 1024 || count > 100_000 {
                return Err(Error::Refused(
                    "review checkout exceeds snapshot limit".into(),
                ));
            }
            let dest = cwd.join(path);
            std::fs::create_dir_all(dest.parent().unwrap())?;
            let mut out = File::create_new(dest)?;
            std::io::copy(&mut e, &mut out)?;
            out.flush()?;
        }
        Ok(())
    })();
    if extracted.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(Error::from)?;
    extracted?;
    if !status.success() {
        return Err(ForgeError::bad("could not materialize the PR head"));
    }
    let merge_base = bounded(root, &["merge-base", base, head], 256)?;
    let merge_base = std::str::from_utf8(&merge_base)
        .map_err(|_| ForgeError::bad("invalid Git merge base"))?
        .trim();
    sha(merge_base)?;
    let paths = bounded(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            merge_base,
            head,
            "--",
        ],
        1024 * 1024,
    )?;
    let mut lines = BTreeMap::new();
    for raw in paths.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let Ok(path) = std::str::from_utf8(raw) else {
            continue;
        };
        if !safe(Path::new(path)) {
            continue;
        }
        if lines.len() >= 3000 {
            return Err(ForgeError::bad("pull exceeds guide file limit"));
        }
        let count = |rev: &str| -> usize {
            let object = format!("{rev}:{path}");
            bounded(root, &["cat-file", "blob", &object], 2 * 1024 * 1024)
                .ok()
                .filter(|bytes| !bytes.contains(&0))
                .map_or(0, |bytes| bytes.split_inclusive(|b| *b == b'\n').count())
        };
        lines.insert(path.into(), (count(merge_base), count(head)));
    }
    let diff = bounded(
        root,
        &[
            "-c",
            "diff.external=",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            merge_base,
            head,
            "--",
        ],
        8 * 1024 * 1024,
    )?;
    let diff = String::from_utf8_lossy(&diff)
        .chars()
        .take(48_000)
        .collect();
    Ok(Checkout {
        _dir: dir,
        cwd: cwd.canonicalize().map_err(Error::from)?,
        diff,
        lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_objects_are_used_even_after_working_files_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            String::from_utf8(crate::workbench_git::git(root, args).unwrap())
                .unwrap()
                .trim()
                .to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        git(&["config", "user.name", "Kar"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://example.com/test/repo.git",
        ]);
        std::fs::write(root.join("main.rs"), "old\n").unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-qm", "old"]);
        let base = git(&["rev-parse", "HEAD"]);
        std::fs::write(root.join("main.rs"), "new\nline\n").unwrap();
        std::fs::create_dir(root.join(".codex")).unwrap();
        std::fs::write(root.join(".codex/config.toml"), "ignored\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/outside", root.join("external")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "head"]);
        let head = git(&["rev-parse", "HEAD"]);
        std::fs::write(root.join("main.rs"), "local edit\n").unwrap();
        let view = prepare(root, "test/repo", "example.com", &head, &base).unwrap();
        assert_eq!(
            std::fs::read_to_string(view.cwd.join("main.rs")).unwrap(),
            "new\nline\n"
        );
        assert_eq!(view.lines["main.rs"], (1, 2));
        assert!(!view.cwd.join(".codex").exists());
        assert!(view.cwd.join("external").symlink_metadata().is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("main.rs")).unwrap(),
            "local edit\n"
        );
    }
}
