//! Resolve and pin the source of a managed checkout before mutating its worktree.
use super::*;

impl Repository {
    pub(crate) fn handoff_base(
        &self,
        base: &str,
        fetch_remote: bool,
    ) -> Result<(String, Option<String>)> {
        let local = || self.resolve(base).map(|id| (id, None));
        if !fetch_remote {
            return local();
        }
        let current = if base == "HEAD" {
            self.status()?.branch
        } else {
            None
        };
        let name = current.as_deref().unwrap_or(base);
        let name = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/remotes/"))
            .unwrap_or(name);
        let names = git(&self.root, &["remote"])?;
        let mut remotes: Vec<_> = utf8(&names)?.lines().collect();
        remotes.sort_by_key(|r| std::cmp::Reverse(r.len()));
        if remotes.is_empty() {
            return local();
        }
        let local_ref = format!("refs/heads/{name}");
        let rows = git(
            &self.root,
            &[
                "for-each-ref",
                "--format=%(refname)%09%(upstream:remotename)%09%(upstream:remoteref)",
                "--",
                &local_ref,
            ],
        )?;
        let entry = utf8(&rows)?
            .lines()
            .map(|s| s.split('\t'))
            .find(|parts| parts.clone().next() == Some(local_ref.as_str()));
        let selected = if let Some(mut fields) = entry {
            fields.next();
            fields
                .next()
                .filter(|r| remotes.contains(r))
                .zip(fields.next().and_then(|s| s.strip_prefix("refs/heads/")))
        } else {
            remotes.iter().find_map(|&r| {
                name.strip_prefix(r)
                    .and_then(|s| s.strip_prefix('/'))
                    .map(|branch| (r, branch))
            })
        };
        let (remote, branch) = selected.unwrap_or(("origin", name));
        let source = format!("refs/heads/{branch}");
        if !remotes.contains(&remote)
            || branch == "HEAD"
            || branch.starts_with('-')
            || !command(&self.root)
                .args(["check-ref-format", &source])
                .output()?
                .status
                .success()
        {
            return local();
        }
        let tracking = format!("refs/remotes/{remote}/{branch}");
        let output = command(&self.root)
            .env("LC_ALL", "C")
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--",
                remote,
                &format!("+{source}:{tracking}"),
            ])
            .output()?;
        if !output.status.success()
            && String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|s| s == format!("fatal: couldn't find remote ref {source}"))
        {
            if name == format!("{remote}/{branch}") && !self.ref_exists(&local_ref)? {
                return Err(Error::Refused(
                    "requested remote base is no longer published".into(),
                ));
            }
            return local();
        }
        checked(output)?;
        Ok((self.resolve(&tracking)?, Some(tracking)))
    }
}
