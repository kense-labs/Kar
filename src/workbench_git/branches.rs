//! Native references and safe checkout operations shared by CLI and workbench clients.
use super::*;
use serde::Deserialize;
use std::{cmp::Reverse, collections::HashSet};

#[derive(Debug, Clone, Copy, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BranchKind {
    #[default]
    All,
    Local,
    Remote,
}

#[derive(Clone, Debug, Default, Deserialize, clap::Args)]
#[serde(default)]
pub struct BranchQuery {
    #[arg(long)]
    pub query: Option<String>,
    #[arg(long, value_enum, default_value = "all")]
    pub kind: BranchKind,
    #[arg(long)]
    pub include_matching_remote_refs: bool,
    #[arg(long, default_value_t = 0)]
    pub cursor: usize,
    #[arg(long)]
    pub limit: Option<usize>,
    /// Read current local Git refs. Kar uses a fresh snapshot on every request.
    #[arg(long)]
    pub refresh: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct Branch {
    pub name: String,
    pub is_remote: bool,
    pub remote_name: Option<String>,
    pub upstream_ref: Option<String>,
    pub current: bool,
    pub is_default: bool,
    pub worktree_path: Option<PathBuf>,
}
#[derive(Debug, Default, Serialize)]
pub struct BranchPage {
    pub is_repo: bool,
    pub is_at_repository_root: Option<bool>,
    pub refs: Vec<Branch>,
    pub has_primary_remote: bool,
    pub total_count: usize,
    pub next_cursor: Option<usize>,
}

impl BranchQuery {
    fn validate(&self) -> Result<()> {
        if self.limit.is_some_and(|n| !(1..=200).contains(&n))
            || self
                .query
                .as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.chars().count() > 256)
        {
            return Err(Error::Refused(
                "branch query requires up to 256 characters and a limit of 1 to 200".into(),
            ));
        }
        Ok(())
    }
}
impl Repository {
    pub fn init(dir: &Path) -> Result<Self> {
        let dir = dir.canonicalize()?;
        if !dir.is_dir() {
            return Err(Error::Refused(
                "Git initialization requires an existing directory".into(),
            ));
        }
        let _lease = ChatStore::workspace_lease_for(&dir, None)?;
        if dir.join(".git").try_exists()? {
            let existing = Self::open(&dir)?;
            if existing.root != dir {
                return Err(Error::Refused(
                    "existing Git checkout has a different root".into(),
                ));
            }
            return Ok(existing);
        }
        git(&dir, &["init"])?;
        Self::open(&dir)
    }

    pub fn branches_in(dir: &Path, query: &BranchQuery) -> Result<BranchPage> {
        Self::branches_in_if(dir, query, |_| Ok(()))
    }

    pub(crate) fn branches_in_if(
        dir: &Path,
        query: &BranchQuery,
        authorize: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<BranchPage> {
        query.validate()?;
        let dir = dir.canonicalize()?;
        if !dir.is_dir() {
            return Err(Error::Refused(
                "Git reference listing requires a directory".into(),
            ));
        }
        for p in dir.ancestors() {
            match p.join(".git").symlink_metadata() {
                Ok(_) => {
                    authorize(p)?;
                    let repo = Self::open(&dir)?;
                    if repo.root != p {
                        return Err(Error::Refused("Git checkout root changed".into()));
                    }
                    let mut page = repo.branches(query)?;
                    page.is_at_repository_root = Some(dir == repo.root);
                    return Ok(page);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(BranchPage::default())
    }

    pub fn branches(&self, query: &BranchQuery) -> Result<BranchPage> {
        query.validate()?;
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let raw = git(
            &self.root,
            &[
                "for-each-ref",
                "--format=%(refname)%09%(committerdate:unix)%09%(symref)%09%(upstream)",
                "refs/heads",
                "refs/remotes",
            ],
        )?;
        let raw_remotes = git(&self.root, &["remote"])?;
        let mut remotes: Vec<_> = utf8(&raw_remotes)?.lines().collect();
        remotes.sort_by_key(|r| Reverse(r.len()));
        let current = self.symbolic_ref("HEAD")?;
        let default = if self.ref_exists("refs/remotes/origin/HEAD")? {
            self.symbolic_ref("refs/remotes/origin/HEAD")?
        } else {
            None
        };
        let default_name = default
            .as_deref()
            .and_then(|s| s.strip_prefix("refs/remotes/origin/"));
        let paths = self.branch_paths()?;
        let mut rows = Vec::new();
        let mut locals = HashSet::new();
        for line in utf8(&raw)?.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            let [full, date, symref, upstream] = fields.as_slice() else {
                return Err(Error::Protocol("invalid Git reference row".into()));
            };
            if !symref.is_empty() {
                continue;
            }
            let local = full.strip_prefix("refs/heads/");
            let name = local
                .or_else(|| full.strip_prefix("refs/remotes/"))
                .ok_or_else(|| Error::Protocol("unexpected reference namespace".into()))?;
            let remote = if local.is_none() {
                remotes.iter().copied().find(|r| {
                    name.strip_prefix(r)
                        .is_some_and(|rest| rest.starts_with('/'))
                })
            } else {
                None
            };
            if local.is_some() {
                locals.insert(name);
            }
            let active = current.as_deref() == Some(*full);
            let primary = local.is_some() && local == default_name
                || (remote == Some("origin") && name.strip_prefix("origin/") == default_name);
            let branch = Branch {
                name: name.into(),
                is_remote: local.is_none(),
                remote_name: remote.map(str::to_owned),
                upstream_ref: (!upstream.is_empty()).then(|| (*upstream).to_string()),
                current: active,
                is_default: primary,
                worktree_path: paths.get(full.as_bytes()).cloned(),
            };
            rows.push((date.parse::<i64>().unwrap_or(0), branch));
        }
        rows.sort_by(|(da, a), (db, b)| {
            (!a.current, !a.is_default, a.is_remote, Reverse(da), &a.name).cmp(&(
                !b.current,
                !b.is_default,
                b.is_remote,
                Reverse(db),
                &b.name,
            ))
        });
        let prefix = query.query.as_deref().unwrap_or("").trim().to_lowercase();
        let matches: Vec<_> = rows
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| {
                (query.include_matching_remote_refs
                    || r.remote_name.as_deref() != Some("origin")
                    || r.name
                        .strip_prefix("origin/")
                        .is_none_or(|n| !locals.contains(n)))
                    && match query.kind {
                        BranchKind::All => true,
                        BranchKind::Remote => r.is_remote,
                        BranchKind::Local => !r.is_remote,
                    }
                    && r.name.to_lowercase().contains(&prefix)
            })
            .collect();
        let total_count = matches.len();
        let refs: Vec<_> = matches
            .into_iter()
            .skip(query.cursor)
            .take(query.limit.unwrap_or(100))
            .collect();
        let end = query.cursor.saturating_add(refs.len());
        Ok(BranchPage {
            is_repo: true,
            is_at_repository_root: Some(true),
            refs,
            has_primary_remote: !remotes.is_empty(),
            total_count,
            next_cursor: (end < total_count).then_some(end),
        })
    }

    fn symbolic_ref(&self, name: &str) -> Result<Option<String>> {
        let out = command(&self.root)
            .args(["symbolic-ref", "--quiet", name])
            .output()?;
        match out.status.code() {
            Some(0) => Ok(Some(utf8(line(&out.stdout))?.into())),
            Some(1) => Ok(None),
            _ => checked(out).map(|_| None),
        }
    }

    fn check_branch(&self, name: &str) -> Result<()> {
        if name.starts_with('-') || name.starts_with("@{") || name.is_empty() {
            return Err(Error::Refused("invalid branch name".into()));
        }
        git(
            &self.root,
            &["check-ref-format", &format!("refs/heads/{name}")],
        )?;
        git(&self.root, &["check-ref-format", "--branch", name])?;
        Ok(())
    }

    pub fn create_branch(&self, name: &str, switch: bool) -> Result<()> {
        self.check_branch(name)?;
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        if switch {
            let _lease = ChatStore::workspace_lease_for(&self.root, None)?;
            self.changing_head(&format!("refs/heads/{name}"), || {
                git(&self.root, &["switch", "--no-overwrite-ignore", "-c", name]).map(drop)
            })
        } else {
            git(&self.root, &["branch", "--", name]).map(drop)
        }
    }

    pub fn switch_branch(&self, name: &str) -> Result<Option<String>> {
        if name.trim().is_empty() || name.starts_with('-') {
            return Err(Error::Refused("expected a Git reference".into()));
        }
        let _pin = pin_saved_checkout(&self.root, self.worktree_id)?;
        let _lease = ChatStore::workspace_lease_for(&self.root, None)?;
        let local = format!("refs/heads/{name}");
        let remote = format!("refs/remotes/{name}");
        if self.ref_exists(&local)? {
            self.changing_head(&local, || {
                git(
                    &self.root,
                    &["switch", "--no-overwrite-ignore", "--no-guess", "--", name],
                )
                .map(drop)
            })?;
        } else if self.ref_exists(&remote)? {
            // Tracking names can differ from the remote branch name. Read the whole native
            // list independently of paging limits used by interactive clients.
            let tracks = git(
                &self.root,
                &[
                    "for-each-ref",
                    "--format=%(refname:lstrip=2)%09%(upstream)",
                    "refs/heads",
                ],
            )?;
            let existing = utf8(&tracks)?
                .lines()
                .filter_map(|s| s.split_once('\t'))
                .find(|(_, up)| *up == remote)
                .map(|(n, _)| n);
            let remotes = git(&self.root, &["remote"])?;
            let short = utf8(&remotes)?
                .lines()
                .filter_map(|r| {
                    name.strip_prefix(r)?
                        .strip_prefix('/')
                        .map(|s| (r.len(), s))
                })
                .max_by_key(|(n, _)| *n)
                .map(|(_, s)| s)
                .ok_or_else(|| {
                    Error::Refused("tracking reference has no configured remote".into())
                })?;
            let target = existing.unwrap_or(short);
            self.check_branch(target)?;
            self.changing_head(&format!("refs/heads/{target}"), || {
                if existing.is_some() {
                    git(
                        &self.root,
                        &[
                            "switch",
                            "--no-overwrite-ignore",
                            "--no-guess",
                            "--",
                            target,
                        ],
                    )?;
                } else {
                    git(
                        &self.root,
                        &[
                            "switch",
                            "--no-overwrite-ignore",
                            "--track",
                            "-c",
                            target,
                            &remote,
                        ],
                    )?;
                }
                Ok(())
            })?;
        } else {
            // Pin the object before switching. A stale ref can never become a path checkout.
            let object = command(&self.root)
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &format!("{name}^{{commit}}"),
                ])
                .output()?;
            match object.status.code() {
                Some(0) => {
                    let oid = utf8(line(&object.stdout))?;
                    self.changing_head("HEAD", || {
                        git(
                            &self.root,
                            &["switch", "--no-overwrite-ignore", "--detach", oid],
                        )
                        .map(drop)
                    })?;
                }
                Some(1) => {
                    // Let Git select a unique remote branch, including checkout.defaultRemote.
                    self.check_branch(name)?;
                    self.changing_head(&local, || {
                        git(
                            &self.root,
                            &["switch", "--no-overwrite-ignore", "--guess", "--", name],
                        )
                        .map(drop)
                    })?;
                }
                _ => {
                    checked(object)?;
                }
            }
        }
        Ok(self
            .symbolic_ref("HEAD")?
            .and_then(|s| s.strip_prefix("refs/heads/").map(str::to_owned)))
    }
}
