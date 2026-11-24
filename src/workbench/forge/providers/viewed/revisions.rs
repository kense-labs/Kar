use super::{super::issue::child, *};
use std::collections::BTreeMap;
type Revisions = BTreeMap<String, String>;

pub(super) async fn available(c: &Context, n: u64, paths: &[&str]) -> Revisions {
    if paths.is_empty() {
        return Revisions::new();
    }
    let result = match c.kind.as_str() {
        "gitlab" => gitlab(c, n, paths).await,
        "azure-devops" => azure(c, n, paths).await,
        "bitbucket" | "forgejo" => patches(c, n, paths).await,
        _ => Ok(Revisions::new()),
    };
    match result {
        Ok(rows) => rows,
        Err(e) => {
            // Inability to resolve the head cannot prove that an already viewed file changed.
            eprintln!(
                "Kar could not read {} file revisions for pull {n}: {}",
                c.kind, e.kind
            );
            Revisions::new()
        }
    }
}
fn absent(result: &mut Revisions, asked: &[&str]) {
    for &p in asked {
        if !result.contains_key(p) {
            result.insert(p.into(), String::new());
        }
    }
}
async fn gitlab(c: &Context, n: u64, paths: &[&str]) -> Result<Revisions, ForgeError> {
    let pull = get(
        c,
        child(rest::project(c)?, &["merge_requests", &n.to_string()]),
    )
    .await?;
    if number(&pull["iid"])? != n {
        return Err(invalid("GitLab merge request identity"));
    }
    let head = pull["diff_refs"]["head_sha"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("GitLab diff head"))?;
    let mut map = Revisions::new();
    for group in paths.chunks(100) {
        let q = "query KarViewedBlobs($fullPath:ID!,$ref:String!,$paths:[String!]!){project(fullPath:$fullPath){repository{blobs(ref:$ref,paths:$paths){nodes{path oid}}}}}";
        let raw = send(
            c,
            Method::POST,
            rest::gitlab_graphql(c),
            Some(json!({"query":q,"variables":{"fullPath":c.repository,"ref":head,"paths":group}})),
            false,
        )
        .await?;
        if raw["errors"].as_array().is_some_and(|e| !e.is_empty()) {
            return Err(invalid("GitLab blob query"));
        }
        let Some(rows) = raw
            .pointer("/data/project/repository/blobs/nodes")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for row in rows {
            if let Some((p, id)) = row["path"].as_str().zip(row["oid"].as_str())
                && !id.is_empty()
                && group.contains(&p)
            {
                map.insert(p.into(), id.into());
            }
        }
        absent(&mut map, group);
    }
    Ok(map)
}
async fn azure(c: &Context, n: u64, paths: &[&str]) -> Result<Revisions, ForgeError> {
    let num = n.to_string();
    let url = super::super::azure::git_endpoint(c, &["pullrequests", &num, "iterations"])?;
    let raw = get(c, url).await?;
    let latest = raw["value"]
        .as_array()
        .ok_or_else(|| invalid("Azure iterations"))?
        .iter()
        .filter(|r| {
            r["sourceRefCommit"]["commitId"].is_string()
                && r["commonRefCommit"]["commitId"].is_string()
        })
        .filter_map(|r| r["id"].as_u64())
        .max();
    let mut map = Revisions::new();
    let Some(id) = latest else {
        absent(&mut map, paths);
        return Ok(map);
    };
    let mut skip = 0;
    loop {
        let mut u = super::super::azure::git_endpoint(
            c,
            &[
                "pullrequests",
                &num,
                "iterations",
                &id.to_string(),
                "changes",
            ],
        )?;
        u.query_pairs_mut()
            .append_pair("$top", "2000")
            .append_pair("$skip", &skip.to_string());
        let changes = get(c, u).await?;
        for row in changes["changeEntries"]
            .as_array()
            .ok_or_else(|| invalid("Azure iteration changes"))?
        {
            let item = &row["item"];
            if item["isFolder"] == true
                || item["gitObjectType"]
                    .as_str()
                    .is_some_and(|s| !s.eq_ignore_ascii_case("blob"))
            {
                continue;
            }
            let deleted = row["changeType"]
                .as_str()
                .is_some_and(|s| s.to_lowercase().contains("delete"));
            let Some(path) = item["path"].as_str().map(|s| s.trim_start_matches('/')) else {
                continue;
            };
            if !deleted
                && paths.contains(&path)
                && let Some(oid) = item["objectId"].as_str().filter(|s| !s.is_empty())
            {
                map.insert(path.into(), oid.into());
            }
        }
        let Some(next) = changes["nextSkip"].as_u64().filter(|n| *n > 0) else {
            absent(&mut map, paths);
            break;
        };
        if next <= skip || next >= 10_000 {
            break;
        }
        skip = next;
    }
    Ok(map)
}
async fn patches(c: &Context, n: u64, paths: &[&str]) -> Result<Revisions, ForgeError> {
    let repo = rest::project(c)?;
    let bitbucket = c.kind == "bitbucket";
    let u = if bitbucket {
        child(repo, &["pullrequests", &n.to_string(), "diff"])
    } else {
        child(repo, &["pulls", &format!("{n}.diff")])
    };
    let mut res = response(c, Method::GET, u, None, false, Some("text/x-diff")).await?;
    let limit = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(b) = res
        .chunk()
        .await
        .map_err(|_| invalid("incomplete hosted patch"))?
    {
        let remaining = limit - bytes.len();
        if b.len() > remaining {
            bytes.extend_from_slice(&b[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&b);
    }
    if truncated && !bitbucket {
        return Err(invalid("truncated Forgejo patch"));
    }
    if truncated {
        bytes.truncate(bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1));
    }
    let mut map = patch::revisions(&String::from_utf8_lossy(&bytes));
    if !truncated {
        absent(&mut map, paths);
    }
    Ok(map)
}
