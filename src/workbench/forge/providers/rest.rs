use super::*;

pub(in crate::workbench::forge) fn project(c: &Context) -> Result<Url, ForgeError> {
    if c.kind == "gitlab" {
        return path(c, &["projects", &c.repository]);
    }
    let (owner, repo) = c
        .repository
        .split_once('/')
        .filter(|(_, r)| !r.contains('/'))
        .ok_or_else(|| ForgeError::bad("forge requires owner/repository"))?;
    path(
        c,
        &[
            if c.kind == "bitbucket" {
                "repositories"
            } else {
                "repos"
            },
            owner,
            repo,
        ],
    )
}
pub(super) fn gitlab_graphql(c: &Context) -> Url {
    let mut url = c.api_base.clone();
    if url.path().trim_end_matches('/').ends_with("/v4") {
        url.path_segments_mut().unwrap().pop_if_empty().pop();
    }
    super::issue::child(url, &["graphql"])
}
fn project_resource(c: &Context, collection: &str, n: Option<u64>) -> Result<Url, ForgeError> {
    let mut u = project(c)?;
    let mut p = u.path_segments_mut().unwrap();
    p.push(collection);
    if let Some(n) = n {
        p.push(&n.to_string());
    }
    drop(p);
    Ok(u)
}
fn state(raw: &Value) -> Result<&'static str, ForgeError> {
    match raw.as_str().map(str::to_ascii_lowercase).as_deref() {
        Some("open" | "opened") => Ok("open"),
        Some("closed" | "declined" | "superseded") => Ok("closed"),
        Some("merged") => Ok("merged"),
        _ => Err(invalid("PR or issue state")),
    }
}
fn item(c: &Context, raw: &Value, pull: bool) -> Result<Value, ForgeError> {
    let v = raw;
    let gitlab = c.kind == "gitlab";
    let bitbucket = c.kind == "bitbucket";
    if !pull && v.get("pull_request").is_some_and(|x| !x.is_null()) {
        return Err(ForgeError::remote(
            404,
            "issue number denotes a pull request",
        ));
    }
    let n = number(
        &v[if gitlab {
            "iid"
        } else if bitbucket {
            "id"
        } else {
            "number"
        }],
    )?;
    let mut out = json!({
        "number":n, "title":v["title"].as_str().ok_or_else(||invalid("object title"))?,
        "body":v[if gitlab||bitbucket {"description"} else {"body"}],
        "state":if v["merged"]==true {"merged"} else {state(&v["state"])?},
        "url":if gitlab {&v["web_url"]} else if bitbucket {&v["links"]["html"]["href"]} else {&v["html_url"]},
        "updated_at":v[if bitbucket {"updated_on"} else {"updated_at"}],
    });
    if pull {
        let (base, head, bsha, hsha) = if gitlab {
            (
                &v["target_branch"],
                &v["source_branch"],
                &v["diff_refs"]["base_sha"],
                v.pointer("/diff_refs/head_sha").unwrap_or(&v["sha"]),
            )
        } else if bitbucket {
            (
                &v["destination"]["branch"]["name"],
                &v["source"]["branch"]["name"],
                &v["destination"]["commit"]["hash"],
                &v["source"]["commit"]["hash"],
            )
        } else {
            (
                &v["base"]["ref"],
                &v["head"]["ref"],
                v.get("merge_base")
                    .filter(|x| x.as_str().is_some_and(|s| !s.is_empty()))
                    .unwrap_or(&v["base"]["sha"]),
                &v["head"]["sha"],
            )
        };
        out["base_ref"] = base.clone();
        out["head_ref"] = head.clone();
        if gitlab {
            out["head_project_id"] = v["source_project_id"].clone();
            if v["source_project_id"].is_number()
                && v["target_project_id"] == v["source_project_id"]
            {
                out["head_repository"] = json!(c.repository);
            }
        } else if bitbucket {
            out["head_repository"] = v["source"]["repository"]["full_name"].clone();
        } else {
            out["head_repository"] = v["head"]["repo"]["full_name"].clone();
        }
        out["base_sha"] = bsha.clone();
        out["head_sha"] = hsha.clone();
    }
    Ok(out)
}
async fn list(c: &Context, pull: bool, collection: &str) -> Result<Value, ForgeError> {
    let base = project_resource(c, collection, None)?;
    let mut page = 1;
    let mut out = Vec::new();
    let mut next = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let url = next.take().unwrap_or_else(|| {
            let mut u = base.clone();
            let mut q = u.query_pairs_mut();
            match c.kind.as_str() {
                "gitlab" => {
                    q.append_pair("state", "opened")
                        .append_pair("per_page", "100");
                }
                "bitbucket" => {
                    q.append_pair("state", "OPEN").append_pair("pagelen", "100");
                }
                _ => {
                    q.append_pair("state", "open").append_pair("limit", "50");
                    if !pull {
                        q.append_pair("type", "issues");
                    }
                }
            };
            q.append_pair("page", &page.to_string());
            drop(q);
            u
        });
        let data = get(c, url).await?;
        let rows = if c.kind == "bitbucket" {
            &data["values"]
        } else {
            &data
        };
        let rows = rows.as_array().ok_or_else(|| invalid("listing"))?;
        for row in rows {
            if !pull && row.get("pull_request").is_some_and(|v| !v.is_null()) {
                continue;
            }
            let value = item(c, row, pull)?;
            if !seen.insert(number(&value["number"])?) {
                return Err(invalid("repeated listing object"));
            }
            out.push(value);
        }
        if c.kind == "bitbucket" {
            let Some(link) = data["next"].as_str() else {
                break;
            };
            let target = base.join(link).map_err(|_| invalid("pagination link"))?;
            if target.origin() != base.origin()
                || target.path() != base.path()
                || target.username() != ""
                || target.password().is_some()
                || target.fragment().is_some()
            {
                return Err(invalid("pagination destination"));
            }
            if rows.is_empty() {
                return Err(invalid("empty listing with a next page"));
            }
            next = Some(target);
        } else if rows.is_empty() {
            break;
        }
        page += 1;
    }
    Ok(json!(out))
}
pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let gitlab = c.kind == "gitlab";
    let bb = c.kind == "bitbucket";
    let pull = matches!(
        op,
        Operation::Pull { .. }
            | Operation::CreatePull { .. }
            | Operation::Pulls
            | Operation::SubmitReview { .. }
            | Operation::ReviewThreads { .. }
            | Operation::ThreadComments { .. }
            | Operation::ReplyToThread { .. }
            | Operation::SetThreadResolution { .. }
    );
    let coll = if !pull {
        "issues"
    } else if gitlab {
        "merge_requests"
    } else if bb {
        "pullrequests"
    } else {
        "pulls"
    };
    if bb && !pull && !matches!(op, Operation::Identity) {
        return Err(ForgeError::unsupported(
            "the registered Bitbucket driver has no issue tracker",
        ));
    }
    match op {
        Operation::PullCommand{number,command}=>crate::workbench::forge::providers::pull_tools::execute(c,*number,command).await,
        op @ (Operation::SubmitReview { .. }
        | Operation::ReplyToThread { .. }
        | Operation::SetThreadResolution { .. }
        | Operation::ReviewThreads { .. }
        | Operation::ThreadComments { .. }) => super::reviews::execute(c, op).await,
        Operation::SetIssueReaction { .. } => super::reactions::execute(c, op, true).await,
        op @ (Operation::UpdateIssue { .. }
        | Operation::CreateIssue { .. }
        | Operation::CommentIssue { .. }
        | Operation::IssueComments { .. }) => super::issue::execute(c, op).await,
        Operation::Identity => {
            let v = get(c, project(c)?).await?;
            let full = v[if gitlab {
                "path_with_namespace"
            } else {
                "full_name"
            }]
            .as_str()
            .ok_or_else(|| invalid("repository identity"))?;
            if !full.eq_ignore_ascii_case(&c.repository) {
                return Err(invalid("repository identity mismatch"));
            }
            let login = if c.token.is_some() {
                let viewer = get(c, path(c, &["user"])?).await?;
                if bb {
                    viewer
                        .get("nickname")
                        .or_else(|| viewer.get("account_id"))
                        .and_then(Value::as_str)
                        .map(|s| json!(s))
                        .ok_or_else(|| invalid("Bitbucket account"))?
                } else {
                    account(&viewer, if gitlab { "username" } else { "login" })?
                }
            } else {
                Value::Null
            };
            Ok(
                json!({"repository":full,"host":c.host,"account":login,"permissions":v["permissions"],"default_branch":if bb{&v["mainbranch"]["name"]}else{&v["default_branch"]}}),
            )
        }
        Operation::Pulls | Operation::Issues => list(c, pull, coll).await,
        Operation::Pull { number: n } | Operation::Issue { number: n } => {
            let raw = get(c, project_resource(c, coll, Some(*n))?).await?;
            let mut item = verify(item(c, &raw, pull)?, *n)?;
            if bb && pull {
                for (field, side) in [("head_sha", "source"), ("base_sha", "destination")] {
                    let Some(short) = item[field].as_str().filter(|s| {
                        (7..40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
                    }) else {
                        continue;
                    };
                    let repository = raw[side]["repository"]["full_name"]
                        .as_str()
                        .unwrap_or(&c.repository);
                    let (owner, repo) = repository
                        .split_once('/')
                        .filter(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
                        .ok_or_else(|| invalid("Bitbucket commit repository"))?;
                    let commit =
                        get(c, path(c, &["repositories", owner, repo, "commit", short])?).await?;
                    let hash = commit["hash"]
                        .as_str()
                        .filter(|s| {
                            matches!(s.len(), 40 | 64)
                                && s.bytes().all(|b| b.is_ascii_hexdigit())
                                && s.starts_with(short)
                        })
                        .ok_or_else(|| invalid("Bitbucket commit identity"))?;
                    item[field] = json!(hash);
                }
            }
            Ok(item)
        }
        Operation::CreatePull {
            head,
            head_repository,
            base,
            title,
            body,
        } => {
            let source = head_repository
                .as_ref()
                .filter(|s| !s.eq_ignore_ascii_case(&c.repository));
            let mut url = project_resource(c, coll, None)?;
            let payload = if gitlab {
                let mut v = json!({"source_branch":head,"target_branch":base,"title":title,"description":body});
                if let Some(source) = source {
                    let target = get(c, project(c)?).await?;
                    v["target_project_id"] = json!(number(&target["id"])?);
                    url = path(c, &["projects", source, coll])?;
                }
                v
            } else if bb {
                let mut v = json!({"source":{"branch":{"name":head}},"destination":{"branch":{"name":base}},"title":title,"description":body});
                if let Some(source) = source {
                    v["source"]["repository"] = json!({"full_name":source});
                }
                v
            } else {
                let selector = source
                    .and_then(|s| s.split_once('/'))
                    .map(|(owner, _)| format!("{owner}:{head}"))
                    .unwrap_or_else(|| head.clone());
                json!({"head":selector,"base":base,"title":title,"body":body})
            };
            let raw = send(c, Method::POST, url, Some(payload), true).await?;
            item(c, &raw, true)
                .map_err(|_| ForgeError::uncertain("created pull request identity is unconfirmed"))
        }
        Operation::SetIssueState { number: n, state } => {
            let state = match state.as_str() {
                "Open" | "open" => "open",
                "Closed" | "closed" => "closed",
                _ => return Err(ForgeError::bad("expected Open or Closed")),
            };
            let u = project_resource(c, coll, Some(*n))?;
            verify(item(c, &get(c, u.clone()).await?, false)?, *n)?;
            let (method, body) = if gitlab {
                (
                    Method::PUT,
                    json!({"state_event":if state=="open"{"reopen"}else{"close"}}),
                )
            } else {
                (Method::PATCH, json!({"state":state}))
            };
            let updated = send(c, method, u, Some(body), true).await?;
            let confirmed = verify(
                item(c, &updated, false)
                    .map_err(|_| ForgeError::uncertain("issue state response unconfirmed"))?,
                *n,
            )
            .map_err(|_| ForgeError::uncertain("updated issue identity unconfirmed"))?;
            if confirmed["state"] != state {
                return Err(ForgeError::uncertain(
                    "forge did not confirm the requested issue state",
                ));
            }
            Ok(confirmed)
        }
    }
}
