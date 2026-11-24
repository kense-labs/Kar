//! A sequence of Git writes owns one checkout lease, including commit hooks.
use super::*;
use crate::chat::checkpoint::WorkspaceLease;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    sync::mpsc,
};

pub(crate) struct StackCheckout {
    pub repo: Repository,
    _pin: Option<File>,
    _lease: WorkspaceLease,
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum HookEvent {
    HookStarted {
        hook_name: String,
    },
    HookOutput {
        hook_name: Option<String>,
        stream: &'static str,
        text: String,
    },
    HookFinished {
        hook_name: String,
        exit_code: Option<i64>,
        duration_ms: Option<u64>,
    },
}
impl StackCheckout {
    pub fn acquire(repo: Repository) -> Result<Self> {
        let pin = pin_saved_checkout(repo.root(), repo.worktree_id)?;
        let lease = ChatStore::workspace_lease_for(repo.root(), None)?;
        Ok(Self {
            repo,
            _pin: pin,
            _lease: lease,
        })
    }
    pub fn branch(&self, name: &str) -> Result<()> {
        self.repo.check_branch(name)?;
        self.repo.changing_head(&format!("refs/heads/{name}"), || {
            git(
                self.repo.root(),
                &["switch", "--no-overwrite-ignore", "-c", name],
            )
            .map(drop)
        })
    }
    pub fn selected(&self, paths: Option<&[PathBuf]>) -> Result<Vec<PathBuf>> {
        let status = self.repo.status()?;
        if let Some(paths) = paths {
            if paths.is_empty() || paths.iter().any(|p| {
                p.as_os_str().is_empty()
                    || p.components().any(
                        |c| !matches!(c,Component::Normal(s) if !s.eq_ignore_ascii_case(".git")),
                    )
            }) {
                return Err(Error::Refused(
                    "expected literal checkout-relative files".into(),
                ));
            }
            for head in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
                if self.repo.resolve(head).is_ok() {
                    return Err(Error::Refused(
                        "cannot partially commit an in-progress Git operation".into(),
                    ));
                }
            }
        }
        let mut selected = Vec::new();
        for entry in status.files {
            if paths.is_some_and(|files| !files.iter().any(|p| entry.path.starts_with(p))) {
                continue;
            }
            if entry.index == 'U'
                || entry.worktree == 'U'
                || matches!((entry.index, entry.worktree), ('A', 'A') | ('D', 'D'))
            {
                return Err(Error::Refused(format!(
                    "resolve the conflict in {} before committing",
                    entry.path.display()
                )));
            }
            selected.push(entry.path);
        }
        Ok(selected)
    }
    pub fn summary(&self, files: &[PathBuf]) -> Result<String> {
        let raw = command(self.repo.root())
            .args(["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--"])
            .args(files)
            .output()?;
        let names = files
            .iter()
            .take(120)
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        let patch = if raw.status.success() {
            &raw.stdout[..raw.stdout.len().min(50000)]
        } else {
            &[]
        };
        Ok(format!("{names}\n{}", String::from_utf8_lossy(patch)))
    }
    pub fn commit(
        &self,
        message: &str,
        selected: Option<&[PathBuf]>,
        mut notify: impl FnMut(HookEvent) -> Result<()>,
    ) -> Result<Option<String>> {
        if self.selected(selected)?.is_empty() {
            return Ok(None);
        }
        let mut add = command(self.repo.root());
        add.args(["add", "-A"]);
        if let Some(paths) = selected {
            add.arg("--").args(paths);
        }
        checked(add.output()?)?;
        let mut changed = command(self.repo.root());
        changed.args(["diff", "--cached", "--quiet", "--exit-code", "--"]);
        if let Some(paths) = selected {
            changed.args(paths);
        }
        let changed = changed.output()?;
        match changed.status.code() {
            Some(0) => return Ok(None),
            Some(1) => {}
            _ => {
                checked(changed)?;
            }
        }
        let mut cmd = command(self.repo.root());
        cmd.args(["commit", "-m", message]);
        if let Some(paths) = selected {
            cmd.arg("--only").arg("--").args(paths);
        }
        commit_output(cmd, &mut notify)?;
        self.repo.resolve("HEAD").map(Some)
    }
    pub fn push(&self, branch: &str) -> Result<(&'static str, String)> {
        if self.repo.branch()? != branch {
            return Err(Error::Refused("checkout branch changed before push".into()));
        }
        let config = |key: &str| -> Result<Option<String>> {
            let out = command(self.repo.root())
                .args(["config", "--get", key])
                .output()?;
            match out.status.code() {
                Some(0) => Ok(Some(utf8(line(&out.stdout))?.into())),
                Some(1) => Ok(None),
                _ => checked(out).map(|_| None),
            }
        };
        let remote = config(&format!("branch.{branch}.pushRemote"))?
            .or(config("remote.pushDefault")?)
            .or(config(&format!("branch.{branch}.remote"))?)
            .unwrap_or_else(|| "origin".into());
        // A transport-level rejection, including a protected or diverged branch, stays an error.
        let upstream = self.repo.status()?.upstream;
        let mut cmd = command(self.repo.root());
        cmd.args(["push", "--porcelain"]);
        if upstream.is_none() {
            cmd.arg("--set-upstream");
        }
        let out = checked(
            cmd.arg("--")
                .arg(&remote)
                .arg(format!("refs/heads/{branch}:refs/heads/{branch}"))
                .output()?,
        )?;
        let lines = utf8(&out)?;
        let state = if lines.lines().any(|s| s.starts_with("=\t")) {
            "skipped_up_to_date"
        } else {
            "pushed"
        };
        Ok((state, remote))
    }
}

fn read_lines(
    reader: impl Read,
    stream: &'static str,
    tx: mpsc::SyncSender<(&'static str, Vec<u8>)>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        if reader
            .by_ref()
            .take(64 * 1024)
            .read_until(b'\n', &mut line)?
            == 0
        {
            break;
        }
        if tx.send((stream, line)).is_err() {
            break;
        }
    }
    Ok(())
}
fn commit_output(mut cmd: Command, notify: &mut impl FnMut(HookEvent) -> Result<()>) -> Result<()> {
    let mut child = cmd
        .env("GIT_TRACE2_EVENT", "2")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let (tx, rx) = mpsc::sync_channel(32);
    let mut hooks = HashMap::new();
    let mut current = None;
    let mut diagnostic = String::new();
    let mut progress = Ok(());
    std::thread::scope(|scope| -> Result<()> {
        let out_tx = tx.clone();
        let out = scope.spawn(move || read_lines(stdout, "stdout", out_tx));
        let err = scope.spawn(move || read_lines(stderr, "stderr", tx));
        for (stream, bytes) in rx {
            let line = String::from_utf8_lossy(&bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            let event = if stream == "stderr"
                && let Ok(v) = serde_json::from_str::<Value>(line)
                && let (Some(sid), Some(kind)) = (v["sid"].as_str(), v["event"].as_str())
            {
                let key = (sid.to_owned(), v["child_id"].as_u64().unwrap_or_default());
                match kind {
                    "child_start" if v["child_class"] == "hook" => {
                        let name = v["hook_name"].as_str().unwrap_or("hook").to_owned();
                        current = Some(name.clone());
                        hooks.insert(key, name.clone());
                        Some(HookEvent::HookStarted { hook_name: name })
                    }
                    "child_exit" => hooks.remove(&key).map(|name| {
                        if current.as_ref() == Some(&name) {
                            current = None;
                        }
                        HookEvent::HookFinished {
                            hook_name: name,
                            exit_code: v["code"].as_i64(),
                            duration_ms: v["t_rel"].as_f64().map(|s| (s * 1000.0) as u64),
                        }
                    }),
                    _ => None,
                }
            } else if !line.is_empty() {
                if stream == "stderr" && diagnostic.len() < 4096 {
                    diagnostic.extend(line.chars().take(1024));
                    diagnostic.push('\n');
                }
                Some(HookEvent::HookOutput {
                    hook_name: current.clone(),
                    stream,
                    text: line.into(),
                })
            } else {
                None
            };
            if progress.is_ok()
                && let Some(event) = event
            {
                progress = notify(event);
            }
        }
        for worker in [out, err] {
            worker
                .join()
                .map_err(|_| Error::Closed("commit output reader stopped".into()))??;
        }
        Ok(())
    })?;
    let status = child.wait()?;
    progress?;
    if !status.success() {
        return Err(Error::Refused(format!(
            "git commit ({status}): {}",
            diagnostic.trim_end()
        )));
    }
    Ok(())
}
