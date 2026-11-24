use super::*;
use std::collections::VecDeque;

fn part(c: &Value, key: &str) -> Result<u64, ForgeError> {
    c[key]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| ForgeError::bad(format!("invalid GitHub {key}")))
}
fn failed(c: &Value) -> bool {
    matches!(
        c["conclusion"].as_str(),
        Some(
            "failure" | "timed_out" | "action_required" | "startup_failure" | "cancelled" | "stale"
        )
    )
}

async fn checks(c: &Context, repo: &Url, head: &str) -> Result<Vec<Value>, ForgeError> {
    let base = issue::child(repo.clone(), &["commits", head, "check-runs"]);
    let mut rows = vec![];
    for page in 1..=1000 {
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("per_page", "100")
            .append_pair("page", &page.to_string());
        let value = get(c, url).await?;
        let entries = value["check_runs"]
            .as_array()
            .ok_or_else(|| ForgeError::bad("invalid GitHub check list"))?;
        for v in entries {
            if v["head_sha"] != head {
                return Err(ForgeError::bad("check did not match the pull head"));
            }
            rows.push(v.clone());
        }
        if entries.len() < 100 {
            return Ok(rows);
        }
    }
    Err(ForgeError::bad("check list exceeds page limit"))
}
fn job(c: &Context, url: &str) -> Option<u64> {
    let url = Url::parse(url).ok()?;
    let pieces = url.path().trim_matches('/').split('/').collect::<Vec<_>>();
    let [org, repo, "actions", "runs", run, "job", id] = pieces.as_slice() else {
        return None;
    };
    if !url.host_str()?.eq_ignore_ascii_case(&c.host)
        || !format!("{org}/{repo}").eq_ignore_ascii_case(&c.repository)
        || run.parse::<u64>().is_err()
    {
        return None;
    }
    id.parse().ok()
}

async fn log(c: &Context, url: Url, lines: usize) -> Result<(String, bool), ForgeError> {
    let mut req = c.client.get(url.clone());
    if let Some(token) = &c.token {
        req = req.bearer_auth(token);
    }
    let mut reply = req
        .send()
        .await
        .map_err(|e| ForgeError::remote(502, e.without_url().to_string()))?;
    if reply.status().is_redirection() {
        let location = reply
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| url.join(v).ok())
            .ok_or_else(|| ForgeError::bad("invalid check-log redirect"))?;
        let host = location.host_str().unwrap_or("");
        if location.username() != ""
            || location.password().is_some()
            || (location.origin() != url.origin()
                && !(location.scheme() == "https"
                    && (host.ends_with(".blob.core.windows.net")
                        || host.ends_with(".githubusercontent.com"))))
        {
            return Err(ForgeError::bad("untrusted check-log download location"));
        }
        reply = c
            .client
            .get(location)
            .send()
            .await
            .map_err(|_| ForgeError::remote(502, "check-log download failed"))?;
    }
    if !reply.status().is_success() {
        return Err(ForgeError::remote(
            reply.status().as_u16(),
            "check log unavailable",
        ));
    }
    // Keep a bounded tail even for multi-gigabyte CI output.
    const BYTES: usize = 2 * 1024 * 1024;
    let mut tail = VecDeque::new();
    let mut truncated = false;
    while let Some(bytes) = reply
        .chunk()
        .await
        .map_err(|_| ForgeError::remote(502, "check log interrupted"))?
    {
        let excess = (tail.len() + bytes.len()).saturating_sub(BYTES);
        if excess > 0 {
            truncated = true;
        }
        let existing = excess.min(tail.len());
        tail.drain(..existing);
        tail.extend(&bytes[excess - existing..]);
    }
    let bytes: Vec<u8> = tail.into_iter().collect();
    let text = String::from_utf8_lossy(&bytes);
    let mut parts: Vec<_> = text.lines().rev().take(lines + 1).collect();
    if parts.len() > lines {
        truncated = true;
        parts.pop();
    }
    parts.reverse();
    Ok((parts.join("\n"), truncated))
}

pub(super) async fn run(
    c: &Context,
    repo: &Url,
    pull: &Value,
    action: &Request,
) -> Result<Value, ForgeError> {
    let head = pull
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .ok_or_else(|| ForgeError::bad("missing pull head"))?;
    let (name, lines) = match action {
        Request::Checks => (None, 200),
        Request::Logs { check, tail_lines } => {
            (check.as_deref(), usize::from(tail_lines.unwrap_or(200)))
        }
        Request::Rerun { check } => (check.as_deref(), 200),
        _ => return Err(ForgeError::bad("invalid CI action")),
    };
    if !(1..=2000).contains(&lines) {
        return Err(ForgeError::bad("tail_lines must be 1..2000"));
    }
    let mut all = checks(c, repo, head).await?;
    let legacy = issue::child(repo.clone(), &["commits", head, "status"]);
    let mut seen = std::collections::BTreeSet::new();
    for page in 1..=1000 {
        let mut url = legacy.clone();
        url.query_pairs_mut()
            .append_pair("per_page", "100")
            .append_pair("page", &page.to_string());
        let combined = match get(c, url).await {
            Ok(v) => v,
            Err(e) if e.status == 404 => break,
            Err(e) => return Err(e),
        };
        if combined["sha"] != head {
            return Err(ForgeError::bad("commit status did not match pull head"));
        }
        let statuses = combined["statuses"]
            .as_array()
            .ok_or_else(|| ForgeError::bad("invalid GitHub commit statuses"))?;
        for s in statuses {
            let name = s["context"]
                .as_str()
                .ok_or_else(|| ForgeError::bad("missing status context"))?;
            if seen.insert(name.to_owned()) {
                let state = s["state"].as_str().unwrap_or("pending");
                all.push(json!({"name":name,"source":"commit_status","head_sha":head,"id":s["id"],"status":if state=="pending"{"pending"}else{"completed"},"conclusion":match state{"error"|"failure"=>Some("failure"),"success"=>Some("success"),_=>None},"details_url":s["target_url"]}));
            }
        }
        if statuses.len() < 100 {
            break;
        }
        if page == 1000 {
            return Err(ForgeError::bad("commit statuses exceed page bound"));
        }
    }
    if matches!(action, Request::Checks) {
        return Ok(json!({"head_sha":head,"checks":all}));
    }
    let mut selected = vec![];
    for row in all
        .into_iter()
        .filter(|v| name.map_or_else(|| failed(v), |n| v["name"] == n))
    {
        let id = row["details_url"].as_str().and_then(|u| job(c, u));
        selected.push((row, id));
    }
    if name.is_some() && selected.is_empty() {
        return Err(ForgeError::bad("named check not found on this head"));
    }
    let mut results = vec![];
    let mut by_run: BTreeMap<u64, Vec<(Value, u64)>> = BTreeMap::new();
    for (check, id) in selected {
        let Some(id) = id else {
            results.push(json!({"name":check["name"],"url":check["details_url"],"detail":"check is not a GitHub Actions job","rerun":false,"log":null}));
            continue;
        };
        let endpoint = issue::child(repo.clone(), &["actions", "jobs", &id.to_string()]);
        let job = get(c, endpoint.clone()).await?;
        if job["head_sha"] != head || part(&job, "id")? != id {
            return Err(ForgeError::bad("job does not belong to the pull head"));
        }
        if matches!(action, Request::Rerun { .. }) {
            by_run
                .entry(part(&job, "run_id")?)
                .or_default()
                .push((check, id));
        } else {
            let (log, truncated) = log(c, issue::child(endpoint, &["logs"]), lines).await?;
            results.push(json!({"name":check["name"],"url":check["details_url"],"log":log,"truncated":truncated}));
        }
    }
    let mut submitted = 0;
    for (run, jobs) in by_run {
        if jobs.len() > 1 {
            let mut url =
                issue::child(repo.clone(), &["actions", "runs", &run.to_string(), "jobs"]);
            url.query_pairs_mut()
                .append_pair("filter", "latest")
                .append_pair("per_page", "100");
            let page = get(c, url).await?;
            let all = page["jobs"]
                .as_array()
                .ok_or_else(|| ForgeError::bad("invalid run jobs"))?;
            let wanted: std::collections::BTreeSet<u64> = jobs.iter().map(|(_, id)| *id).collect();
            let failing = all
                .iter()
                .filter(|j| failed(j))
                .map(|j| part(j, "id"))
                .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            if page["total_count"].as_u64() != Some(all.len() as u64) || wanted != failing {
                for (check, _) in jobs {
                    results.push(json!({"name":check["name"],"rerun":false,"detail":"the request does not cover every failed job in this run; select one job"}));
                }
                continue;
            }
        }
        let endpoint = if jobs.len() == 1 {
            issue::child(
                repo.clone(),
                &["actions", "jobs", &jobs[0].1.to_string(), "rerun"],
            )
        } else {
            issue::child(
                repo.clone(),
                &["actions", "runs", &run.to_string(), "rerun-failed-jobs"],
            )
        };
        send(c, Method::POST, endpoint, None, true)
            .await
            .map_err(|e| {
                if submitted == 0 {
                    e
                } else {
                    ForgeError::uncertain(format!(
                        "{submitted} rerun requests accepted before {}",
                        e.detail
                    ))
                }
            })?;
        submitted += 1;
        for (check, _) in jobs {
            results.push(json!({"name":check["name"],"rerun":true}));
        }
    }
    Ok(json!({"head_sha":head,"checks":results}))
}
