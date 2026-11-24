//! Review immutable checkpoints without reading HEAD, the index, or working files.
use super::{ChatStore, err};
use crate::{Error, Result, chat::TurnStatus};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
};
use uuid::Uuid;

const OUTPUT_LIMIT: usize = 10_000_000;
const TRUNCATED: &str = "\n\n[truncated]";

#[derive(Serialize)]
pub struct TurnDiff {
    pub chat: Uuid,
    pub from: usize,
    pub to: usize,
    pub diff: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repositories: Option<Vec<RepositoryDiff>>,
}

#[derive(Serialize)]
pub struct RepositoryDiff {
    pub name: String,
    pub repository_path: PathBuf,
    pub relative_path: PathBuf,
    pub diff: String,
}

fn relative(from: &Path, to: &Path) -> PathBuf {
    let Some(base) = from.ancestors().find(|base| to.starts_with(base)) else {
        return to.to_owned();
    };
    let mut result = PathBuf::new();
    for _ in from.strip_prefix(base).unwrap().components() {
        result.push("..");
    }
    result.push(to.strip_prefix(base).unwrap());
    result
}

fn bounded_patch(
    odb: &Path,
    from: &str,
    to: &str,
    ignore_whitespace: bool,
    limit: usize,
) -> Result<String> {
    // Only object IDs produced by the checkpoint journal are valid revisions.
    let from = gix::ObjectId::from_hex(from.as_bytes())
        .map_err(err)?
        .to_string();
    let to = gix::ObjectId::from_hex(to.as_bytes())
        .map_err(err)?
        .to_string();
    if from == to {
        return Ok(String::new());
    }
    let mut stderr = tempfile::tempfile()?;
    let mut command = crate::workbench_git::command(odb);
    command.args([
        "--git-dir",
        ".",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-relative",
    ]);
    if ignore_whitespace {
        command.arg("--ignore-all-space");
    }
    let mut child = command
        .args([from.as_str(), to.as_str(), "--"])
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .expect("piped git diff")
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes);
    let oversized = bytes.len() > limit;
    if result.is_err() || oversized {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    if !oversized && !status.success() {
        stderr.seek(SeekFrom::Start(0))?;
        let mut detail = Vec::new();
        stderr.take(4096).read_to_end(&mut detail)?;
        return Err(err(format!(
            "cannot compare stored trees ({status}): {}",
            String::from_utf8_lossy(&detail).trim()
        )));
    }
    let mut patch = String::from_utf8_lossy(&bytes).into_owned();
    if oversized || patch.len() > limit {
        if limit < TRUNCATED.len() {
            return Ok(String::new());
        }
        let mut end = limit - TRUNCATED.len();
        while !patch.is_char_boundary(end) {
            end -= 1;
        }
        patch.truncate(end);
        patch.push_str(TRUNCATED);
    }
    Ok(patch)
}

impl ChatStore {
    pub fn turn_diff(
        &self,
        chat: Uuid,
        from: usize,
        to: usize,
        ignore_whitespace: bool,
    ) -> Result<TurnDiff> {
        if from > to {
            return Err(Error::Refused(
                "from must be less than or equal to to".into(),
            ));
        }
        let history = self.history(chat)?;
        let mut result = TurnDiff {
            chat,
            from,
            to,
            diff: String::new(),
            repositories: None,
        };
        if from == to {
            return Ok(result);
        }
        let missing = || Error::Refused("the requested completed checkpoint is unavailable".into());
        let end = history
            .turns
            .get(to - 1)
            .filter(|t| t.status == TurnStatus::Completed)
            .and_then(|t| t.checkpoint.as_ref())
            .ok_or_else(missing)?;
        let before = if from == 0 {
            &history
                .turns
                .first()
                .and_then(|t| t.checkpoint.as_ref())
                .ok_or_else(missing)?
                .before_tree
        } else if from == to - 1 {
            &end.before_tree
        } else {
            history
                .turns
                .get(from - 1)
                .filter(|t| t.status == TurnStatus::Completed)
                .and_then(|t| t.checkpoint.as_ref()?.after_tree.as_ref())
                .ok_or_else(missing)?
        };
        let after = end.after_tree.as_ref().ok_or_else(missing)?;
        let odb = self.odb_path(history.chat.checkpoint_store());
        result.diff = bounded_patch(&odb, before, after, ignore_whitespace, OUTPUT_LIMIT)?;
        let mut ranges = BTreeMap::new();
        let mut has_scopes = false;
        for cp in history.turns[from..to]
            .iter()
            .filter_map(|t| t.checkpoint.as_ref())
        {
            has_scopes |= !cp.repositories.is_empty();
            for scope in &cp.repositories {
                if let (Some(first), Some(last)) = (&scope.before_tree, &scope.after_tree)
                    && first != last
                {
                    let row = ranges.entry(&scope.scope.repository_path).or_insert((
                        &scope.scope,
                        first,
                        last,
                    ));
                    row.2 = last;
                }
            }
        }
        if has_scopes {
            let mut bytes = OUTPUT_LIMIT.saturating_sub(result.diff.len());
            let mut patches = Vec::new();
            for (_, (scope, before, after)) in ranges {
                if bytes < TRUNCATED.len() {
                    break;
                }
                let patch = bounded_patch(&odb, before, after, ignore_whitespace, bytes)?;
                if patch.is_empty() {
                    continue;
                }
                bytes -= patch.len();
                patches.push(RepositoryDiff {
                    name: scope.name.clone(),
                    repository_path: scope.repository_path.clone(),
                    relative_path: relative(&history.chat.workspace, &scope.worktree_path),
                    diff: patch,
                });
            }
            result.repositories = Some(patches);
        }
        Ok(result)
    }
}
