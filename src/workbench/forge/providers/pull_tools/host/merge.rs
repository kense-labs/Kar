use super::*;

pub(super) async fn apply(
    p: &Pull<'_>,
    expected: &str,
    method: &str,
    auto: bool,
) -> Result<Value, ForgeError> {
    if p.head()? != expected {
        return Err(ForgeError::bad("pull head changed"));
    }
    if !matches!(method, "merge" | "squash" | "rebase") {
        return Err(ForgeError::bad("unknown merge method"));
    }
    let unsupported = || {
        p.unsupported(if auto {
            "automatic merge with the selected strategy"
        } else {
            "the selected merge strategy"
        })
    };
    let (verb, url, body) = match p.c.kind.as_str() {
        "gitlab" if method != "rebase" => (
            Method::PUT,
            p.endpoint(&["merge"]),
            json!({"sha":expected,"squash":method=="squash","merge_when_pipeline_succeeds":auto}),
        ),
        "forgejo" if !auto => (
            Method::POST,
            p.endpoint(&["merge"]),
            json!({"Do":method,"head_commit_id":expected}),
        ),
        "bitbucket" if !auto => {
            let strategy = match method {
                "rebase" => "rebase_fast_forward",
                "squash" => "squash",
                _ => "merge_commit",
            };
            (
                Method::POST,
                p.endpoint(&["merge"]),
                json!({"merge_strategy":strategy}),
            )
        }
        "cursor-origin" if !auto && method != "rebase" => {
            if let Some(value) = p
                .raw
                .pointer("/stack/parentPullRequest/number")
                .filter(|v| !v.is_null())
            {
                let parent = number(value)?;
                let raw = get(p.c, hosted::resource(p.c, Some("pulls"), Some(parent))?).await?;
                if number(&raw["number"])? != parent {
                    return Err(invalid("stack parent"));
                }
                if raw["mergedAt"].is_null()
                    && raw["state"]
                        .as_str()
                        .is_none_or(|s| s.eq_ignore_ascii_case("open"))
                {
                    return Err(ForgeError::bad(format!(
                        "merge parent pull #{parent} first"
                    )));
                }
            }
            (
                Method::POST,
                p.endpoint(&["merge"]),
                json!({"mergeMethod":method}),
            )
        }
        "tanstack-source" if method != "rebase" => {
            let (action, body) = if auto {
                ("auto-merge", json!({"merge_method":method}))
            } else {
                ("merge", json!({"merge_method":method,"sha":expected}))
            };
            (Method::PUT, p.endpoint(&[action]), body)
        }
        "azure-devops" if method != "rebase" => {
            let mut body = json!({"completionOptions":{"mergeStrategy":if method=="squash"{"squash"}else{"noFastForward"}},"lastMergeSourceCommit":{"commitId":expected}});
            if auto {
                body["autoCompleteSetBy"] = json!({"id":azure::viewer_id(p.c).await?});
            } else {
                body["status"] = json!("completed");
            }
            (Method::PATCH, p.url.clone(), body)
        }
        "gitcafe" if !auto => {
            let base = match p.raw["mergeRoute"].as_str() {
                Some("provider") => p.raw["observedBaseOid"].clone(),
                Some("unsupported") => return Err(unsupported()),
                _ => {
                    let mut url = child(p.repo.clone(), &["commit"]);
                    let branch = p.raw["targetBranch"]
                        .as_str()
                        .ok_or_else(|| invalid("target branch"))?;
                    url.query_pairs_mut()
                        .append_pair("ref", &format!("refs/heads/{branch}"));
                    get(p.c, url).await?["oid"].clone()
                }
            };
            if base.as_str().is_none_or(str::is_empty) {
                return Err(invalid("target commit"));
            }
            let version = p.raw["version"]
                .as_u64()
                .ok_or_else(|| invalid("pull version"))?;
            (
                Method::POST,
                p.endpoint(&["merge"]),
                json!({"requestId":uuid::Uuid::new_v4(),"expectedVersion":version,"headOid":expected,"baseOid":base,"strategy":method}),
            )
        }
        _ => return Err(unsupported()),
    };
    let response = send(p.c, verb, url, Some(body), true).await?;
    if response["state"] == "failed" {
        return Err(ForgeError::bad("the provider rejected the merge"));
    }
    Ok(
        json!({"number":p.n,"submitted":true,"auto":auto,"observed_head_sha":expected,"result":response}),
    )
}
