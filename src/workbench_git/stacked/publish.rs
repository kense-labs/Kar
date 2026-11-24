//! A tracking branch can be a comparison base, or the same head on a different remote.
use super::*;

pub(crate) struct PublishTarget {
    pub remote: String,
    pub branch: String,
    // When publishing a new head, Git should subsequently track that head.
    set_upstream: bool,
    comparison: Option<(String, String)>,
    independent: bool,
}

impl Repository {
    fn config(&self, key: &str) -> Result<Option<String>> {
        let out = command(self.root())
            .args(["config", "--get", key])
            .output()?;
        match out.status.code() {
            Some(0) => Ok(Some(utf8(line(&out.stdout))?.to_owned()).filter(|s| !s.is_empty())),
            Some(1) => Ok(None),
            _ => checked(out).map(|_| None),
        }
    }
    pub(crate) fn publish_target(&self, branch: &str) -> Result<PublishTarget> {
        let branch_key = |field| format!("branch.{branch}.{field}");
        let push_remote = self
            .config(&branch_key("pushRemote"))?
            .or(self.config("remote.pushDefault")?);
        let tracking = self.config(&branch_key("remote"))?.zip(
            self.config(&branch_key("merge"))?
                .and_then(|r| r.strip_prefix("refs/heads/").map(str::to_owned)),
        );
        let own = tracking.as_ref().filter(|(r, b)| {
            r != "."
                && (b == branch
                    || (branch.ends_with(&format!("/{b}"))
                        && format!("{r}/{b}").ends_with(&format!("/{branch}"))))
        });
        let remotes = git(self.root(), &["remote"])?;
        let remotes: Vec<_> = utf8(&remotes)?.lines().collect();
        let remote = push_remote
            .as_deref()
            .or_else(|| own.map(|(r, _)| r.as_str()))
            .or_else(|| {
                if remotes.contains(&"origin") {
                    Some("origin")
                } else {
                    remotes.first().copied()
                }
            })
            .filter(|r| remotes.contains(r))
            .ok_or_else(|| Error::Refused("no matching git publish remote is configured".into()))?
            .to_owned();
        let local_base = tracking.as_ref().is_some_and(|(r, _)| r == ".");
        let name = if let Some((_, name)) = own {
            name
        } else if local_base {
            branch
        } else {
            branch
                .strip_prefix(&format!("{remote}/"))
                .filter(|s| !s.is_empty())
                .unwrap_or(branch)
        };
        let set_upstream = own.is_none()
            || self
                .resolve(&format!("{branch}@{{upstream}}"))
                .is_err();
        let independent = own.is_some_and(|(r, _)| r != &remote);
        let comparison = if own.is_none() {
            tracking.clone()
        } else {
            None
        };
        Ok(PublishTarget {
            remote,
            branch: name.to_owned(),
            set_upstream,
            comparison,
            independent,
        })
    }
    pub(crate) fn comparison_branch(&self, local: &str, target: &PublishTarget) -> Result<Option<String>> {
        if let Some(reference) = self.config(&format!("branch.{local}.kar-merge-base-ref"))?
            && self.resolve(&reference).is_ok()
        {
            if let Some(name) = reference.strip_prefix("refs/heads/") {
                return Ok(Some(name.to_owned()));
            }
            if let Some(remote_ref) = reference.strip_prefix("refs/remotes/") {
                let remotes = git(self.root(), &["remote"])?;
                let found = utf8(&remotes)?
                    .lines()
                    .filter_map(|r| {
                        remote_ref
                            .strip_prefix(&format!("{r}/"))
                            .map(|name| (r.len(), name))
                    })
                    .max_by_key(|(n, _)| *n);
                if let Some((_, name)) = found {
                    return Ok(Some(name.to_owned()));
                }
            }
        }
        let key = format!("branch.{local}.kar-merge-base");
        Ok(self
            .config(&key)?
            .or_else(|| target.comparison.as_ref().map(|(_, b)| b.clone())))
    }
    /// A non-network push URL can serve as the local transport for a registered hosted remote.
    pub(crate) fn publish_urls(&self, target: &PublishTarget) -> Result<(String, Vec<String>)> {
        let fetch = git(
            self.root(),
            &["remote", "get-url", "--", &target.remote],
        )?;
        let push = git(
            self.root(),
            &["remote", "get-url", "--push", "--all", "--", &target.remote],
        )?;
        Ok((
            utf8(line(&fetch))?.to_owned(),
            utf8(&push)?.lines().map(str::to_owned).collect(),
        ))
    }
    /// Publish exactly the admitted commit without touching the active checkout or its upstream.
    pub(crate) fn publish_commit(&self, commit:&str, branch:&str, url:&str)->Result<()> {
        self.check_branch(branch)?;
        if commit.len()!=40 && commit.len()!=64 || !commit.bytes().all(|c|c.is_ascii_hexdigit()){
            return Err(Error::Refused("invalid publication commit".into()));
        }
        checked(command(self.root()).args(["-c","core.hooksPath=/dev/null","push","--porcelain","--",url,&format!("{commit}:refs/heads/{branch}")]).output()?).map(drop)
    }
}
impl StackCheckout {
    pub fn push(&self, local: &str, target: &PublishTarget) -> Result<(&'static str, bool)> {
        if self.repo.branch()? != local {
            return Err(Error::Refused("checkout branch changed before push".into()));
        }
        let cwd = self.repo.root();
        let status = self.repo.status()?;
        let refname = format!("refs/heads/{}", target.branch);
        if target.independent && !target.set_upstream && status.ahead == 0 && status.behind == 0 {
            let (fetch_url, push_urls) = self.repo.publish_urls(target)?;
            let tracking = format!("refs/remotes/{}/{}", target.remote, target.branch);
            // A cached tracking ref may predate a deletion/rewind on the fork.
            if push_urls == [fetch_url]
                && command(cwd)
                    .args([
                        "fetch",
                        "--no-tags",
                        "--no-write-fetch-head",
                        "--",
                        &target.remote,
                        &format!("+{refname}:{tracking}"),
                    ])
                    .output()?
                    .status
                    .success()
                && command(cwd)
                    .args(["merge-base", "--is-ancestor", "HEAD", &tracking])
                    .output()?
                    .status
                    .success()
            {
                return Ok(("skipped_up_to_date", false));
            }
        }
        if let Some((base_remote, base)) = &target.comparison {
            let key = format!("branch.{local}.kar-merge-base");
            if self.repo.config(&key)?.is_none() {
                let reference = if base_remote == "." {
                    format!("refs/heads/{base}")
                } else {
                    format!("refs/remotes/{base_remote}/{base}")
                };
                let ref_key = format!("branch.{local}.kar-merge-base-ref");
                if self.repo.config(&ref_key)?.is_none() {
                    git(cwd, &["config", "--local", &ref_key, &reference])?;
                }
                git(cwd, &["config", "--local", &key, base])?;
            }
        }
        let mut cmd = command(cwd);
        cmd.args(["push", "--porcelain"]);
        if target.set_upstream {
            cmd.arg("--set-upstream");
        }
        let out = checked(
            cmd.arg("--")
                .arg(&target.remote)
                .arg(format!("refs/heads/{local}:{refname}"))
                .output()?,
        )?;
        let unchanged = utf8(&out)?.lines().any(|s| s.starts_with("=\t"));
        let recorded = target.set_upstream
            && self.repo.config(&format!("branch.{local}.remote"))?.as_deref()
                == Some(target.remote.as_str())
            && self.repo.config(&format!("branch.{local}.merge"))?.as_deref() == Some(refname.as_str());
        Ok((
            if unchanged {
                "skipped_up_to_date"
            } else {
                "pushed"
            },
            recorded,
        ))
    }
}
