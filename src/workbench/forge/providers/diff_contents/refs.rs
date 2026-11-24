use super::*;

fn commit_sides(
    commit: &str,
    head: &Value,
    base: &Value,
    input: &Request,
) -> Result<Sides, ForgeError> {
    let head = oid(head)?;
    if !head
        .to_ascii_lowercase()
        .starts_with(&commit.to_ascii_lowercase())
    {
        return Err(invalid("selected commit identity"));
    }
    let old = if base.is_null() && input.change_type == Change::New {
        None
    } else {
        Some(oid(base)?.into())
    };
    Ok(Sides {
        old,
        new: Some(head.into()),
        head_repo: None,
    })
}
async fn git_hosts(c: &Context, n: u64, input: &Request) -> Result<Sides, ForgeError> {
    let repo = rest::project(c)?;
    let gl = c.kind == "gitlab";
    let fj = c.kind == "forgejo";
    if let Some(commit) = &input.commit {
        if !valid_ref(commit) {
            return Err(ForgeError::bad("invalid commit hex identifier"));
        }
        let mut u = if gl {
            child(repo, &["repository", "commits", commit])
        } else if fj {
            child(repo.clone(), &["git", "commits", commit])
        } else {
            child(repo, &["commits", commit])
        };
        if fj {
            u.query_pairs_mut()
                .extend_pairs([("files", "false"), ("verification", "false")]);
        }
        let v = get(c, u).await?;
        let mut sides = if gl {
            commit_sides(commit, &v["id"], &v["parent_ids"][0], input)?
        } else {
            commit_sides(commit, &v["sha"], &v["parents"][0]["sha"], input)?
        };
        if fj {
            let p = get(c, child(rest::project(c)?, &["pulls", &n.to_string()])).await?;
            if number(&p["number"])? != n {
                return Err(invalid("Forgejo pull number"));
            }
            sides.head_repo = p["head"]["repo"]["full_name"].as_str().map(str::to_owned);
        }
        return Ok(sides);
    }
    let collection = if gl { "merge_requests" } else { "pulls" };
    let p = get(c, child(repo.clone(), &[collection, &n.to_string()])).await?;
    if number(&p[if gl { "iid" } else { "number" }])? != n {
        return Err(invalid("pull number"));
    }
    if gl {
        return Sides::pair(&p["diff_refs"]["base_sha"], &p["diff_refs"]["head_sha"]);
    }
    let mut sides = Sides::pair(&p["base"]["sha"], &p["head"]["sha"])?;
    if fj {
        if !p["merge_base"].is_null() && p["merge_base"] != "" {
            sides.old = Some(oid(&p["merge_base"])?.into());
        }
        sides.head_repo = p["head"]["repo"]["full_name"].as_str().map(str::to_owned);
    } else if input.change_type != Change::New {
        let range = format!("{}...{}", oid(&p["base"]["sha"])?, oid(&p["head"]["sha"])?);
        let mut u = child(repo, &["compare", &range]);
        u.query_pairs_mut()
            .extend_pairs([("per_page", "1"), ("page", "2")]);
        if let Ok(v) = get(c, u).await
            && let Ok(sha) = oid(&v["merge_base_commit"]["sha"])
        {
            sides.old = Some(sha.into());
        }
    }
    Ok(sides)
}
async fn azure_sides(c: &Context, n: u64, input: &Request) -> Result<Sides, ForgeError> {
    let u = azure::git_endpoint(c, &["pullrequests", &n.to_string(), "iterations"])?;
    let v = get(c, u).await?;
    let it = v["value"]
        .as_array()
        .ok_or_else(|| invalid("Azure iterations"))?
        .iter()
        .filter(|r| {
            r["id"].as_u64().is_some()
                && r["sourceRefCommit"]["commitId"].is_string()
                && r["commonRefCommit"]["commitId"].is_string()
        })
        .max_by_key(|r| r["id"].as_u64());
    if let Some(commit) = &input.commit
        && it.is_none_or(|r| r["sourceRefCommit"]["commitId"] != *commit)
    {
        return Err(ForgeError::unsupported(
            "Azure supplies the latest pull iteration comparison",
        ));
    }
    match it {
        None => Ok(Sides {
            old: None,
            new: None,
            head_repo: None,
        }),
        Some(r) => Sides::pair(
            &r["commonRefCommit"]["commitId"],
            &r["sourceRefCommit"]["commitId"],
        ),
    }
}
async fn hosted_sides(c: &Context, n: u64, input: &Request) -> Result<Sides, ForgeError> {
    if input.commit.is_some() {
        return Err(ForgeError::unsupported(
            "host has no per-commit pull diff contents",
        ));
    }
    let target = hosted::resource(c, Some("pulls"), Some(n))?;
    let pull = get(c, target.clone()).await?;
    if number(&pull["number"])? != n {
        return Err(invalid("pull number"));
    }
    if c.kind == "tanstack-source" {
        let files = get(c, child(target, &["files"])).await?;
        let mut s = Sides::pair(&files["mergeBaseSha"], &files["headSha"])?;
        s.head_repo = pull["headRepo"].as_str().map(str::to_owned);
        return Ok(s);
    }
    let ver = pull["version"]
        .as_u64()
        .ok_or_else(|| invalid("GitCafe pull version"))?;
    let strict_oid = |v: &Value| {
        v.as_str().is_some_and(|s| {
            s.len() == 40
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        })
    };
    if !strict_oid(&pull["headOid"]) {
        return Err(invalid("immutable GitCafe head"));
    }
    let mut u = child(target, &["changes"]);
    u.query_pairs_mut()
        .append_pair("expectedVersion", &ver.to_string())
        .append_pair("limit", "1");
    let snap = get(c, u).await?;
    if snap["version"] != ver || snap["headOid"] != pull["headOid"] {
        return Err(ForgeError::remote(409, "GitCafe pull revision changed"));
    }
    if !strict_oid(&snap["comparisonBaseOid"]) {
        return Err(invalid("immutable GitCafe merge base"));
    }
    let mut s = Sides::pair(&snap["comparisonBaseOid"], &snap["headOid"])?;
    if !pull["sourceRepo"].is_null() {
        let source = &pull["sourceRepo"];
        let (owner, name) = source["owner"]
            .as_str()
            .zip(source["name"].as_str())
            .ok_or_else(|| invalid("GitCafe source repository"))?;
        s.head_repo = Some(format!("{owner}/{name}"));
    }
    Ok(s)
}
pub(super) async fn resolve(c: &Context, n: u64, input: &Request) -> Result<Sides, ForgeError> {
    match c.kind.as_str() {
        "azure-devops" => azure_sides(c, n, input).await,
        "gitcafe" | "tanstack-source" => hosted_sides(c, n, input).await,
        _ => git_hosts(c, n, input).await,
    }
}
