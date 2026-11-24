use super::*;
fn segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
}

fn cafe_ref(c: &Context, link: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(url) = link.and_then(|s| Url::parse(s).ok()) else {
        return (None, None);
    };
    let parts = url.path().trim_matches('/').split('/').collect::<Vec<_>>();
    let host = url.host_str().unwrap_or("");
    match parts.as_slice() {
        [org, repo, "actions", "runs", run, ..]
            if host.eq_ignore_ascii_case(&c.host)
                && format!("{org}/{repo}").eq_ignore_ascii_case(&c.repository)
                && run.strip_prefix("actwfrun_").is_some_and(|s| {
                    !s.is_empty() && s.bytes().all(|v| v.is_ascii_alphanumeric())
                }) =>
        {
            (Some((*run).to_owned()), None)
        }
        [_org, _repo, "actions", "runs", run, "job", id]
            if host.eq_ignore_ascii_case("github.com")
                && run.parse::<u64>().is_ok()
                && id.parse::<u64>().is_ok() =>
        {
            (None, Some((*id).to_owned()))
        }
        _ => (None, None),
    }
}
async fn source_jobs(
    p: &Pull<'_>,
    head: &str,
) -> Result<BTreeMap<String, (String, String, String)>, ForgeError> {
    let base = child(p.repo.clone(), &["ci", "runs"]);
    let mut cursor = None::<String>;
    let mut seen = BTreeSet::new();
    let mut jobs = BTreeMap::new();
    for _ in 0..10 {
        let mut url = base.clone();
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("pull", &p.n.to_string())
                .append_pair("limit", "50");
            if let Some(s) = &cursor {
                query.append_pair("cursor", s);
            }
        }
        let page = get(p.c, url).await?;
        let runs = rows(&page, Some("items"))?;
        let matching: Vec<_> = runs.iter().filter(|r| r["headSha"] == head).collect();
        if matching.is_empty() && !jobs.is_empty() {
            break;
        }
        for run in matching {
            let id = run["id"]
                .as_str()
                .filter(|s| segment(s))
                .ok_or_else(|| invalid("CI run id"))?;
            let detail = get(p.c, child(base.clone(), &[id])).await?;
            if detail["id"] != id {
                return Err(invalid("CI run identity"));
            }
            for job in rows(&detail, Some("jobs"))? {
                if let Some(job_id) = job["jobId"].as_str() {
                    let key = job["key"]
                        .as_str()
                        .filter(|s| segment(s))
                        .ok_or_else(|| invalid("job key"))?;
                    jobs.entry(job_id.to_owned()).or_insert_with(|| {
                        (
                            id.to_owned(),
                            key.to_owned(),
                            run["workflow"].as_str().unwrap_or("").to_owned(),
                        )
                    });
                }
            }
        }
        let Some(next) = page["next"].as_str().filter(|s| !s.is_empty()) else {
            break;
        };
        if !seen.insert(next.to_owned()) {
            return Err(invalid("repeated run cursor"));
        }
        cursor = Some(next.to_owned());
    }
    Ok(jobs)
}
pub(super) async fn checks(p: &Pull<'_>, head: &str) -> Result<Vec<Check>, ForgeError> {
    if p.c.kind == "gitcafe" {
        let raw =
            edit::cafe_pages(p.c, child(p.repo.clone(), &["commits", head, "checks"])).await?;
        let mut rows = vec![];
        let mut newest: BTreeMap<(ValueKey, ValueKey, String), (u128, usize)> = BTreeMap::new();
        for row in raw {
            let mut check = Check::from(
                row["name"].as_str(),
                row["conclusion"].as_str().or(row["status"].as_str()),
                row["detailsUrl"].as_str(),
            )?;
            (check.run, check.job) = cafe_ref(p.c, check.url.as_deref());
            if let Some(id) = row["id"].as_str().and_then(|s| s.parse::<u128>().ok()) {
                let key = (
                    row["provider"].as_str().map(str::to_owned),
                    row["githubApp"].as_str().map(str::to_owned),
                    check.name.clone(),
                );
                if let Some((order, slot)) = newest.get_mut(&key) {
                    if id > *order {
                        *order = id;
                        rows[*slot] = check;
                    }
                    continue;
                }
                newest.insert(key, (id, rows.len()));
            }
            rows.push(check);
        }
        return Ok(rows);
    }
    let mut url = child(p.repo.clone(), &["check-runs"]);
    url.query_pairs_mut().append_pair("sha", head);
    let raw = get(p.c, url).await?;
    let jobs = match source_jobs(p, head).await {
        Ok(jobs) => jobs,
        Err(e) if matches!(e.status, 403 | 404) => BTreeMap::new(),
        Err(e) => return Err(e),
    };
    let mut latest: BTreeMap<String, (i64, Check)> = BTreeMap::new();
    for row in rows(&raw, None)? {
        if row["view"] == "superseded" {
            continue;
        }
        let mut c = Check::from(
            row["name"].as_str(),
            row["conclusion"].as_str().or(row["status"].as_str()),
            row["url"].as_str(),
        )?;
        if let Some((run, key, workflow)) = row["jobId"].as_str().and_then(|id| jobs.get(id)) {
            c.run = Some(run.clone());
            c.job = Some(key.clone());
            if !workflow.is_empty() {
                c.name = format!("{workflow}/{}", c.name);
            }
        }
        let at = row["startedAt"]
            .as_i64()
            .or(row["completedAt"].as_i64())
            .unwrap_or(0);
        if latest.get(&c.name).is_none_or(|(old, _)| at > *old) {
            latest.insert(c.name.clone(), (at, c));
        }
    }
    Ok(latest.into_values().map(|(_, c)| c).collect())
}
type ValueKey = Option<String>;
fn tail(text: &str, count: usize) -> (String, bool) {
    let mut rows = text.lines().rev().take(count + 1).collect::<Vec<_>>();
    let cut = rows.len() > count;
    if cut {
        rows.pop();
    }
    rows.reverse();
    (rows.join("\n"), cut)
}
async fn log(p: &Pull<'_>, check: &Check, lines: usize) -> Result<Value, ForgeError> {
    let Some(job) = &check.job else {
        return Ok(
            json!({"name":check.name,"log":null,"detail":"This check has no provider-hosted log","url":check.url}),
        );
    };
    let (text, truncated) = if p.c.kind == "gitcafe" {
        let mut url = child(p.repo.clone(), &["ci", "github", "checks", job, "log"]);
        url.query_pairs_mut().append_pair("tailBytes", "262144");
        let data = get(p.c, url).await?;
        let content = data["content"]
            .as_str()
            .ok_or_else(|| invalid("GitCafe log"))?;
        let (text, cut) = tail(content, lines);
        (text, cut || data["truncated"] == true)
    } else {
        let run = check.run.as_ref().ok_or_else(|| invalid("CI run id"))?;
        let url = child(p.repo.clone(), &["ci", "runs", run, "jobs", job, "logs"]);
        let mut first = url.clone();
        first
            .query_pairs_mut()
            .append_pair("from", "1")
            .append_pair("to", "1");
        let res = response(p.c, Method::GET, first, None, false, Some("text/plain")).await?;
        let (mut bytes, headers) = read_bytes(res, false, 2 * 1024 * 1024).await?;
        let total = headers
            .get("x-log-steps")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|s| s["lines"].as_u64())
                    .fold(0u64, u64::saturating_add)
            })
            .unwrap_or(0);
        let from = total.saturating_sub(lines as u64).saturating_add(1).max(1);
        if total > 1 {
            let mut last = url;
            last.query_pairs_mut()
                .append_pair("from", &from.to_string())
                .append_pair("to", &total.to_string());
            let res = response(p.c, Method::GET, last, None, false, Some("text/plain")).await?;
            bytes = read_bytes(res, false, 2 * 1024 * 1024).await?.0;
        }
        let (text, cut) = tail(&String::from_utf8_lossy(&bytes), lines);
        (text, cut || from > 1)
    };
    Ok(json!({"name":check.name,"url":check.url,"log":text,"truncated":truncated}))
}
pub(super) async fn operate(
    p: &Pull<'_>,
    checks: &[&Check],
    rerun: bool,
    lines: usize,
) -> Result<Vec<Value>, ForgeError> {
    let mut done = BTreeSet::new();
    let mut out = vec![];
    for c in checks {
        if !rerun {
            match log(p, c, lines).await {
                Ok(v) => out.push(v),
                Err(e) if !matches!(e.status, 401 | 403 | 429) => {
                    out.push(json!({"name":c.name,"log":null,"detail":e.detail}))
                }
                Err(e) => return Err(e),
            }
            continue;
        }
        let Some(id) = &c.run else {
            out.push(json!({"name":c.name,"rerun":false,"detail":"The selected driver cannot rerun this check"}));
            continue;
        };
        if !done.contains(id) {
            let cafe = p.c.kind == "gitcafe";
            let url = child(
                p.repo.clone(),
                &[if cafe { "actions" } else { "ci" }, "runs", id, "rerun"],
            );
            send(
                p.c,
                Method::POST,
                url,
                (!cafe).then(|| json!({"failedOnly":true})),
                true,
            )
            .await
            .map_err(|e| {
                if done.is_empty() {
                    e
                } else {
                    ForgeError::uncertain(format!(
                        "{} earlier reruns accepted before {}",
                        done.len(),
                        e.detail
                    ))
                }
            })?;
            done.insert(id.clone());
        }
        out.push(json!({"name":c.name,"run_id":id,"rerun":true}));
    }
    Ok(out)
}
