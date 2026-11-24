//! Registered review operations with host-specific capabilities and revision-scoped writes.
use super::issue::child;
use super::*;
use crate::workbench::forge::actions::{self, Comment, Position, Side, Verdict};
pub(super) mod github;
mod gitlab;
mod threads;

fn partial(count: usize, failure: ForgeError) -> ForgeError {
    if count == 0 {
        failure
    } else {
        ForgeError::uncertain(format!(
            "{count} review writes were acknowledged before {}: inspect the remote conversation",
            failure.kind
        ))
    }
}
fn numeric_id(s: &str) -> Result<u64, ForgeError> {
    s.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| ForgeError::bad("this provider requires a positive numeric thread ID"))
}
fn slug(s: &str) -> Result<(), ForgeError> {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Ok(())
    } else {
        Err(ForgeError::bad(
            "GitCafe requires an opaque alphanumeric thread ID",
        ))
    }
}

async fn submit(
    c: &Context,
    root: Url,
    n: u64,
    verdict: Verdict,
    body: &str,
    notes: &[Comment],
) -> Result<Value, ForgeError> {
    if c.kind == "gitlab" {
        return gitlab::submit(c, root, n, verdict, body, notes).await;
    }
    let detail = if matches!(c.kind.as_str(), "gitcafe" | "forgejo" | "tanstack-source") {
        get(c, root.clone()).await?
    } else {
        Value::Null
    };
    if detail.is_object() && number(&detail["number"])? != n {
        return Err(invalid("pull request identity"));
    }
    let native = match verdict {
        Verdict::Approve => "approve",
        Verdict::RequestChanges => "request_changes",
        Verdict::Comment => "comment",
    };
    if c.kind == "gitcafe" {
        let head = detail["headOid"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("GitCafe pull head"))?;
        let version = detail["version"]
            .as_u64()
            .ok_or_else(|| invalid("GitCafe version"))?;
        if detail["capabilities"]["review"] == false {
            return Err(ForgeError::remote(403, "GitCafe denies review access"));
        }
        let mut payload = json!({"verdict":native,"commitOid":head,"expectedVersion":version,"requestId":uuid::Uuid::new_v4()});
        if !body.is_empty() {
            payload["body"] = body.into();
        }
        let r = send(
            c,
            Method::POST,
            child(root, &["reviews"]),
            Some(payload),
            true,
        )
        .await?;
        let id = r["id"].as_str().ok_or_else(|| {
            ForgeError::uncertain("GitCafe did not identify the submitted review")
        })?;
        return Ok(json!({"number":n,"review_id":id}));
    }
    let mut inline = vec![];
    for note in notes {
        let (side, line) = note.position.point();
        let left = side == Side::Left;
        let value = match c.kind.as_str() {
            "forgejo" => {
                json!({"path":if left{note.old_path.as_ref().unwrap_or(&note.path)}else{&note.path},"body":note.body,"old_position":if left{line}else{0},"new_position":if left{0}else{line}})
            }
            "bitbucket" => {
                json!({"content":{"raw":note.body},"inline":{"path":note.path,(if left{"from"}else{"to"}):line}})
            }
            "cursor-origin" => {
                json!({"body":note.body,"inline":{"path":note.path,"side":if left{"left"}else{"right"},"startLine":line,"endLine":line}})
            }
            "tanstack-source" => {
                let head = detail["headSha"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("Source pull head"))?;
                let mut v = json!({"path":note.path,"body":note.body,"side":if left{"old"}else{"new"},"line":line,"commitId":head});
                if let Some((s, l)) = note.start_position.map(Position::point)
                    && s == side
                    && l < line
                {
                    v["startLine"] = l.into();
                }
                v
            }
            _ => unreachable!(),
        };
        inline.push(value);
    }
    if c.kind == "bitbucket" {
        let mut count = 0;
        if !body.trim().is_empty() {
            inline.push(json!({"content":{"raw":body}}));
        }
        for payload in inline {
            send(
                c,
                Method::POST,
                child(root.clone(), &["comments"]),
                Some(payload),
                true,
            )
            .await
            .map_err(|e| partial(count, e))?;
            count += 1;
        }
        if verdict != Verdict::Comment {
            send(
                c,
                Method::POST,
                child(
                    root,
                    &[if verdict == Verdict::Approve {
                        "approve"
                    } else {
                        "request-changes"
                    }],
                ),
                None,
                true,
            )
            .await
            .map_err(|e| partial(count, e))?;
            count += 1;
        }
        return Ok(json!({"number":n,"confirmed_writes":count}));
    }
    let payload = match c.kind.as_str() {
        "forgejo" => {
            json!({"event":if verdict==Verdict::Approve{"APPROVED"}else{verdict.event()},"commit_id":detail["head"]["sha"].as_str().filter(|s|!s.is_empty()).ok_or_else(||invalid("Forgejo head SHA"))?,"body":body,"comments":inline})
        }
        "cursor-origin" => json!({"verdict":native,"body":body,"comments":inline}),
        "tanstack-source" => {
            let mut v = json!({"verdict":match verdict {Verdict::Approve=>"approved",Verdict::RequestChanges=>"changes_requested",Verdict::Comment=>"commented"}});
            if !body.is_empty() {
                v["body"] = body.into();
            }
            if !inline.is_empty() {
                v["comments"] = inline.into();
            }
            v
        }
        _ => unreachable!(),
    };
    let row = send(
        c,
        Method::POST,
        child(root, &["reviews"]),
        Some(payload),
        true,
    )
    .await?;
    Ok(json!({"number":n,"review_id":row["id"]}))
}

async fn reply(c: &Context, root: Url, n: u64, key: &str, body: &str) -> Result<Value, ForgeError> {
    let (u, v) = match c.kind.as_str() {
        "gitlab" => (
            child(root, &["discussions", key, "notes"]),
            json!({"body":body}),
        ),
        "cursor-origin" => (
            child(root, &["comments"]),
            json!({"body":body,"threadId":key}),
        ),
        "gitcafe" => {
            slug(key)?;
            (
                child(root, &["comments", key, "replies"]),
                json!({"body":body}),
            )
        }
        "tanstack-source" => (
            child(root, &["comments"]),
            json!({"body":body,"replyTo":numeric_id(key)?}),
        ),
        "bitbucket" => (
            child(root, &["comments"]),
            json!({"content":{"raw":body},"parent":{"id":numeric_id(key)?}}),
        ),
        _ => unreachable!(),
    };
    let r = send(c, Method::POST, u, Some(v), true).await?;
    Ok(json!({"number":n,"thread_id":key,"comment_id":r["id"]}))
}

async fn resolve(
    c: &Context,
    root: Url,
    repo: Url,
    n: u64,
    key: &str,
    resolved: bool,
) -> Result<Value, ForgeError> {
    let (method, u, input) = match c.kind.as_str() {
        "gitlab" => (
            Method::PUT,
            child(root, &["discussions", key]),
            Some(json!({"resolved":resolved})),
        ),
        "cursor-origin" => {
            // The native endpoint addresses threads at repository scope. Check the PR before writing.
            threads::find(c, &root, key).await?;
            (
                Method::PATCH,
                child(repo, &["pulls", "threads", key]),
                Some(json!({"resolved":resolved})),
            )
        }
        "gitcafe" => {
            slug(key)?;
            let t = threads::find(c, &root, key).await?;
            if t["resolved"] == resolved {
                return Ok(json!({"number":n,"thread_id":key,"resolved":resolved}));
            }
            let version = t["version"]
                .as_u64()
                .ok_or_else(|| invalid("GitCafe thread root version"))?;
            (
                Method::POST,
                child(
                    root,
                    &[
                        "comments",
                        key,
                        if resolved { "resolve" } else { "unresolve" },
                    ],
                ),
                Some(json!({"expectedVersion":version})),
            )
        }
        "tanstack-source" => {
            numeric_id(key)?;
            (
                Method::PUT,
                child(root, &["threads", key]),
                Some(json!({"resolved":resolved})),
            )
        }
        "bitbucket" => {
            numeric_id(key)?;
            (
                if resolved {
                    Method::POST
                } else {
                    Method::DELETE
                },
                child(root, &["comments", key, "resolve"]),
                None,
            )
        }
        _ => unreachable!(),
    };
    send(c, method, u, input, true).await?;
    Ok(json!({"number":n,"thread_id":key,"resolved":resolved}))
}

pub(in crate::workbench::forge) async fn execute(
    c: &Context,
    op: &Operation,
) -> Result<Value, ForgeError> {
    let caps = actions::capabilities(&c.kind);
    let n = match op {
        Operation::SubmitReview {
            number, comments, ..
        } => {
            if caps["submit"] != true || (!comments.is_empty() && caps["inline"] != true) {
                return Err(ForgeError::unsupported(
                    "this driver does not support the requested review",
                ));
            }
            if comments.iter().any(|note| note.start_position.is_some()) && caps["range"] != true {
                return Err(ForgeError::unsupported(
                    "this driver cannot anchor a multi-line review comment",
                ));
            }
            *number
        }
        Operation::ReplyToThread { number, .. } => {
            if caps["reply"] != true {
                return Err(ForgeError::unsupported(
                    "this driver does not support thread replies",
                ));
            }
            *number
        }
        Operation::SetThreadResolution { number, .. } => {
            if caps["resolve"] != true {
                return Err(ForgeError::unsupported(
                    "this driver does not support thread resolution",
                ));
            }
            *number
        }
        Operation::ReviewThreads { number, .. } => *number,
        Operation::ThreadComments { number, .. } => {
            if c.kind != "github" {
                return Err(ForgeError::unsupported(
                    "this driver returns thread comments in the activity page",
                ));
            }
            *number
        }
        _ => unreachable!(),
    };
    if c.kind == "azure-devops" {
        return Ok(json!({"threads":[],"nextCursor":null}));
    }
    if c.kind == "linear" {
        return Err(ForgeError::unsupported(
            "Linear registers an issue tracker only",
        ));
    }
    let repo = if matches!(
        c.kind.as_str(),
        "gitcafe" | "cursor-origin" | "tanstack-source"
    ) {
        hosted::resource(c, None, None)?
    } else {
        rest::project(c)?
    };
    let collection = match c.kind.as_str() {
        "gitlab" => "merge_requests",
        "bitbucket" => "pullrequests",
        _ => "pulls",
    };
    let root = child(repo.clone(), &[collection, &n.to_string()]);
    if c.kind == "github" {
        return github::execute(c, root, op, n).await;
    }
    match op {
        Operation::SubmitReview {
            verdict,
            body,
            comments,
            ..
        } => submit(c, root, n, *verdict, body, comments).await,
        Operation::ReplyToThread {
            thread_id, body, ..
        } => reply(c, root, n, thread_id, body).await,
        Operation::SetThreadResolution {
            thread_id,
            resolved,
            ..
        } => resolve(c, root, repo, n, thread_id, *resolved).await,
        Operation::ReviewThreads { cursor, .. } => {
            if cursor.is_some() {
                return Err(ForgeError::bad(
                    "this driver returns all threads in one result",
                ));
            }
            threads::read(c, &root).await
        }
        _ => unreachable!(),
    }
}
