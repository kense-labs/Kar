//! GitCafe, Cursor Origin and TanStack Source use different wire shapes for repository pulls.
use super::*;

fn repository(c: &Context) -> Result<(&str, &str), ForgeError> {
    let id = if c.kind == "tanstack-source" {
        c.repository.trim_start_matches('@')
    } else {
        &c.repository
    };
    id.split_once('/')
        .filter(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
        .ok_or_else(|| ForgeError::bad("expected owner/repository"))
}
pub(in crate::workbench::forge) fn resource(
    c: &Context,
    collection: Option<&str>,
    n: Option<u64>,
) -> Result<Url, ForgeError> {
    let (owner, repo) = repository(c)?;
    let mut url = path(c, &["repos", owner, repo])?;
    if let Some(collection) = collection {
        let mut parts = url.path_segments_mut().unwrap();
        parts.push(collection);
        if let Some(num) = n {
            parts.push(&num.to_string());
        }
    }
    Ok(url)
}
fn item(c: &Context, v: &Value, pull: bool) -> Result<Value, ForgeError> {
    let cafe = c.kind == "gitcafe";
    let origin = c.kind == "cursor-origin";
    let n = number(&v["number"])?;
    let (owner, repo) = repository(c)?;
    let state = if v["merged"] == true
        || (origin && v["mergedAt"].is_string())
        || (cafe
            && v.pointer("/providerMerge/mergedAt")
                .is_some_and(Value::is_string))
    {
        "merged"
    } else {
        match v["state"].as_str().map(str::to_ascii_lowercase).as_deref() {
            Some("open") => "open",
            Some("closed") => "closed",
            Some("merged") => "merged",
            None if origin && pull => "open",
            _ => return Err(invalid("object state")),
        }
    };
    let url = if origin {
        v["webUrl"]
            .as_str()
            .ok_or_else(|| invalid("Origin pull URL"))?
            .into()
    } else if cafe {
        format!("https://{}/{owner}/{repo}/pulls/{n}", c.host)
    } else {
        format!(
            "https://{}/@{owner}/{repo}/{}/{n}",
            c.host,
            if pull { "pull" } else { "issues" }
        )
    };
    let mut out = json!({"number":n,"title":v["title"].as_str().ok_or_else(||invalid("object title"))?,
        "state":state,"url":url,"updated_at":v.get("updatedAt"),"body":v[if cafe{"description"}else{"body"}]});
    if c.kind == "tanstack-source" {
        out["updated_at"] = ["updatedAt", "mergedAt", "createdAt"]
            .into_iter()
            .filter_map(|key| v[key].as_i64())
            .max()
            .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
            .map(|time| json!(time.to_string()))
            .unwrap_or_default();
    }
    if pull {
        let (base, head, bsha, hsha) = if origin {
            (
                &v["base"]["ref"],
                &v["head"]["ref"],
                &v["base"]["sha"],
                &v["head"]["sha"],
            )
        } else if cafe {
            (
                &v["targetBranch"],
                &v["sourceBranch"],
                &v["observedBaseOid"],
                &v["headOid"],
            )
        } else {
            (&v["base"], &v["head"], &v["baseSha"], &v["headSha"])
        };
        out["base_ref"] = base.clone();
        out["head_ref"] = if c.kind == "tanstack-source" {
            let name = head
                .as_str()
                .unwrap_or_default()
                .trim_start_matches("refs/heads/");
            json!(name.split_once(':').map_or(name, |(_, s)| s))
        } else {
            head.clone()
        };
        out["head_repository"] = if cafe {
            v["sourceRepo"]["owner"]
                .as_str()
                .zip(v["sourceRepo"]["name"].as_str())
                .map(|(o, r)| json!(format!("{o}/{r}")))
                .unwrap_or(Value::Null)
        } else if origin {
            v.pointer("/head/repo/full_name")
                .cloned()
                .unwrap_or_else(|| json!(c.repository))
        } else {
            let owner = head
                .as_str()
                .and_then(|s| s.split_once(':'))
                .map(|(o, _)| o)
                .unwrap_or(owner);
            v.get("headRepo")
                .filter(|r| r.is_string())
                .cloned()
                .unwrap_or_else(|| json!(format!("{owner}/{repo}")))
        };
        out["base_sha"] = bsha.clone();
        out["head_sha"] = hsha.clone();
    }
    Ok(out)
}
async fn detail(c: &Context, collection: &str, n: u64) -> Result<Value, ForgeError> {
    verify(
        item(
            c,
            &get(c, resource(c, Some(collection), Some(n))?).await?,
            collection == "pulls",
        )?,
        n,
    )
}
async fn list(c: &Context, collection: &str) -> Result<Value, ForgeError> {
    let origin = c.kind == "cursor-origin";
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    loop {
        let mut u = resource(c, Some(collection), None)?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("state", "open");
            if origin {
                q.append_pair("pageSize", "100")
                    .append_pair("sortBy", "updated");
            } else {
                q.append_pair("limit", "100");
            }
            if let Some(next) = &cursor {
                q.append_pair(
                    if origin {
                        "pageToken"
                    } else if c.kind == "gitcafe" {
                        "after"
                    } else {
                        "cursor"
                    },
                    next,
                );
            }
        }
        let raw = get(c, u).await?;
        let key = if origin { "pullRequests" } else { "items" };
        if !(origin && raw[key].is_null()) {
            for row in raw[key].as_array().ok_or_else(|| invalid("host listing"))? {
                rows.push(item(c, row, collection == "pulls")?);
            }
        }
        cursor = raw[if origin { "nextPageToken" } else { "next" }]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let Some(next) = &cursor else { break };
        if !seen.insert(next.clone()) {
            return Err(invalid("repeated pagination cursor"));
        }
    }
    Ok(json!(rows))
}
pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let cafe = c.kind == "gitcafe";
    let origin = c.kind == "cursor-origin";
    if matches!(
        op,
        Operation::Issues | Operation::Issue { .. } | Operation::SetIssueState { .. }
    ) && (cafe || origin)
    {
        return Err(ForgeError::unsupported(format!(
            "the registered {} driver has no issue tracker",
            c.kind
        )));
    }
    match op {
        op @ (Operation::SubmitReview { .. }
        | Operation::ReplyToThread { .. }
        | Operation::SetThreadResolution { .. }
        | Operation::ReviewThreads { .. }
        | Operation::ThreadComments { .. }) => super::reviews::execute(c, op).await,
        op @ (Operation::UpdateIssue { .. }
        | Operation::CreateIssue { .. }
        | Operation::CommentIssue { .. }
        | Operation::IssueComments { .. }) => super::issue::execute(c, op).await,
        Operation::Identity => {
            let repo = get(c, resource(c, None, None)?).await?;
            let (owner, name) = repository(c)?;
            let repository = format!("{owner}/{name}");
            let correct_owner = if origin {
                repo.pointer("/owner/slug")
            } else {
                repo.get("owner")
            };
            if repo["name"] != name
                || correct_owner
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != owner)
                || repo["fullName"].as_str().is_some_and(|s| s != repository)
            {
                return Err(invalid("repository identity mismatch"));
            }
            let account = if c.token.is_none() {
                Value::Null
            } else if origin {
                let list = get(c, path(c, &["namespaces"])?).await?;
                let ns = list["namespaces"]
                    .as_array()
                    .and_then(|xs| xs.iter().find(|x| x["namespace"]["type"] == "user"));
                ns.and_then(|x| x["namespace"]["slug"].as_str())
                    .map(|s| json!(s))
                    .ok_or_else(|| invalid("Origin personal namespace"))?
            } else {
                let endpoint = if cafe {
                    &["auth", "principal"][..]
                } else {
                    &["user"][..]
                };
                let user = get(c, path(c, endpoint)?).await?;
                account(&user, if cafe { "handle" } else { "username" })?
            };
            Ok(
                json!({"host":c.host,"repository":repository,"account":account,"permissions":repo.get("permissions"),"default_branch":repo["defaultBranch"]}),
            )
        }
        Operation::Pull { number } => detail(c, "pulls", *number).await,
        Operation::Issue { number } => detail(c, "issues", *number).await,
        Operation::Pulls => list(c, "pulls").await,
        Operation::Issues => list(c, "issues").await,
        Operation::SetIssueState { number, state } => {
            if !matches!(state.as_str(), "open" | "closed") {
                return Err(ForgeError::bad(
                    "TanStack Source issue state must be open or closed",
                ));
            }
            detail(c, "issues", *number).await?;
            send(
                c,
                Method::PATCH,
                resource(c, Some("issues"), Some(*number))?,
                Some(json!({"state":state})),
                true,
            )
            .await?;
            let result = detail(c, "issues", *number)
                .await
                .map_err(|_| ForgeError::uncertain("could not confirm the updated issue"))?;
            if result["state"] != *state {
                return Err(ForgeError::uncertain("issue state is unconfirmed"));
            }
            Ok(result)
        }
        Operation::CreatePull {
            head,
            head_repository,
            base,
            title,
            body,
        } => {
            let payload = if cafe {
                let source = Context {
                    repository: head_repository
                        .clone()
                        .unwrap_or_else(|| c.repository.clone()),
                    ..c.clone()
                };
                let repo = get(c, resource(&source, None, None)?).await?;
                let id = repo["id"]
                    .as_str()
                    .ok_or_else(|| invalid("head repository ID"))?;
                json!({"repo":id,"source":head,"target":base,"description":body,"title":title})
            } else {
                json!({"head":head,"base":base,"title":title,"body":body,"draft":false})
            };
            let reply = send(
                c,
                Method::POST,
                resource(c, Some("pulls"), None)?,
                Some(payload),
                true,
            )
            .await?;
            let number = number(
                reply
                    .get("number")
                    .or_else(|| reply.pointer("/pullRequest/number"))
                    .unwrap_or(&Value::Null),
            )
            .map_err(|_| {
                ForgeError::uncertain("the create response did not identify the pull request")
            })?;
            detail(c, "pulls", number).await.map_err(|_| {
                ForgeError::uncertain("the created pull request could not be confirmed")
            })
        }
    }
}
