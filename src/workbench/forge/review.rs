use super::*;
use crate::workbench_git::{ManagedWorktree, git};

pub(super) fn key(project: Uuid, ctx: &Context, number: u64) -> String {
    format!("{project}:{}:{number}", ctx.identity())
}

fn timestamp(pull: &Value) -> Option<jiff::Timestamp> {
    pull["updated_at"].as_str()?.parse().ok()
}
pub(super) async fn refresh(
    app: &Workbench,
    project: Uuid,
    c: &Context,
    number: u64,
) -> Result<Value, ForgeError> {
    let key = key(project, c, number);
    let seq = app.update(|db| {
        let r = db.forge.reviews.entry(key.clone()).or_default();
        r.requested = r
            .requested
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("review generation exhausted".into()))?;
        Ok(r.requested)
    })?;
    let pull = c.execute(&Operation::Pull { number }).await?;
    if checked_number(&pull)? != number {
        return Err(ForgeError::remote(
            502,
            "provider returned a different change request",
        ));
    }
    Ok(app.update(|db| {
        let r = db.forge.reviews.get_mut(&key).expect("registered review");
        if seq > r.applied {
            let older = matches!((timestamp(&pull),timestamp(&r.pull)),(Some(incoming),Some(saved)) if incoming < saved);
            if r.pull.is_null() || !older {
                r.pull = pull.clone();
            }
            r.applied = seq;
        }
        Ok(if r.pull.is_null(){pull}else{r.pull.clone()})
    })?)
}

fn oid(pull: &Value, name: &str) -> Result<String, ForgeError> {
    pull[name]
        .as_str()
        .filter(|s| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or_else(|| {
            ForgeError::unsupported(format!(
                "change request is missing its {name} commit identity"
            ))
        })
}
pub(super) fn fetch(
    c: &Context,
    number: u64,
    pull: &Value,
) -> Result<(String, String), ForgeError> {
    let base = oid(pull, "base_sha")?;
    let head = oid(pull, "head_sha")?;
    for (is_head, sha) in [(false, &base), (true, &head)] {
        let commit = format!("{sha}^{{commit}}");
        if git(&c.cwd, &["cat-file", "-e", &commit]).is_err() {
            let (remote, host, repository) = primary_remote(&c.cwd)?;
            if host != c.host || repository != c.repository {
                return Err(ForgeError::bad("project remote changed during review"));
            }
            let result = git(
                &c.cwd,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--",
                    &remote,
                    sha,
                ],
            );
            if let Err(e) = result {
                if !is_head {
                    return Err(e.into());
                }
                let reference = match c.kind.as_str() {
                    "github" | "forgejo" | "tanstack-source" => format!("refs/pull/{number}/head"),
                    "gitlab" => format!("refs/merge-requests/{number}/head"),
                    _ => return Err(e.into()),
                };
                git(
                    &c.cwd,
                    &[
                        "fetch",
                        "--no-tags",
                        "--no-write-fetch-head",
                        "--",
                        &remote,
                        &reference,
                    ],
                )?;
            }
            git(&c.cwd, &["cat-file", "-e", &commit])?;
        }
    }
    Ok((base, head))
}

pub(super) fn files(c: &Context, number: u64, pull: &Value) -> Result<Vec<String>, ForgeError> {
    let (base, head) = fetch(c, number, pull)?;
    // Relative to the merge base, matching the files introduced by the proposed branch.
    let range = format!("{base}...{head}");
    let out = git(
        &c.cwd,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            &range,
            "--",
        ],
    )?;
    out.split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(|v| {
            std::str::from_utf8(v)
                .map(str::to_owned)
                .map_err(|_| ForgeError::unsupported("review path is not UTF-8"))
        })
        .collect()
}
pub(super) fn patch(
    c: &Context,
    number: u64,
    pull: &Value,
    path: &str,
) -> Result<String, ForgeError> {
    if !files(c, number, pull)?.iter().any(|p| p == path) {
        return Err(ForgeError::remote(404, "path is not part of this review"));
    }
    let range = format!("{}...{}", oid(pull, "base_sha")?, oid(pull, "head_sha")?);
    let output = git(
        &c.cwd,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            &range,
            "--",
            path,
        ],
    )?;
    String::from_utf8(output).map_err(|_| ForgeError::unsupported("patch is not UTF-8 text"))
}
pub(super) fn worktree(
    app: &Workbench,
    c: &Context,
    number: u64,
    pull: &Value,
    id: Uuid,
    path: PathBuf,
) -> Result<ManagedWorktree, ForgeError> {
    let path = app.destination(&path)?;
    let (_, head) = fetch(c, number, pull)?;
    Ok(Repository::open(&c.cwd)?.create_worktree(
        id,
        &path,
        &format!("kar/review-{number}-{id}"),
        &head,
    )?)
}
