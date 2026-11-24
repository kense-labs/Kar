use super::*;
use std::collections::BTreeSet;
mod hosted;
#[derive(Clone)]
struct Check {
    name: String,
    status: String,
    url: Option<String>,
    run: Option<String>,
    job: Option<String>,
}
impl Check {
    fn from(
        name: Option<&str>,
        state: Option<&str>,
        url: Option<&str>,
    ) -> Result<Self, ForgeError> {
        let name = name
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid("check name"))?;
        let status = match state.unwrap_or("").to_ascii_lowercase().as_str() {
            "approved" | "success" | "successful" | "succeeded" | "passed" => "success",
            "failure" | "failed" | "failing" | "broken" | "rejected" | "error" | "timed_out"
            | "startup_failure" => "failure",
            "cancelled" | "canceled" | "stopped" => "cancelled",
            "skipped" | "notapplicable" => "skipped",
            "neutral" | "stale" => "neutral",
            "waiting" | "action_required" | "action-required" | "manual" => "action_required",
            _ => "pending",
        };
        Ok(Self {
            name: name.to_owned(),
            status: status.into(),
            url: url.map(str::to_owned),
            run: None,
            job: None,
        })
    }
    fn value(&self) -> Value {
        json!({"name":self.name,"status":self.status,"details_url":self.url})
    }
}
fn rows<'a>(v: &'a Value, key: Option<&str>) -> Result<&'a Vec<Value>, ForgeError> {
    key.map_or(v, |key| &v[key])
        .as_array()
        .ok_or_else(|| invalid("check page"))
}
async fn pages(p: &Pull<'_>, base: Url, key: Option<&str>) -> Result<Vec<Value>, ForgeError> {
    let mut out = vec![];
    let mut next = None;
    let mut seen = BTreeSet::new();
    for page in 1..=1000 {
        let mut url = base.clone();
        match p.c.kind.as_str() {
            "bitbucket" => {
                if let Some(next) = next.take() {
                    url = next;
                } else {
                    url.query_pairs_mut().append_pair("pagelen", "100");
                }
            }
            "cursor-origin" => {
                url.query_pairs_mut().append_pair("pageSize", "100");
            }
            "azure-devops" => {
                url.query_pairs_mut()
                    .append_pair("$top", "100")
                    .append_pair("$skip", &((page - 1) * 100).to_string());
            }
            "gitlab" => {
                url.query_pairs_mut()
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.to_string());
            }
            _ => {
                url.query_pairs_mut()
                    .append_pair("limit", "100")
                    .append_pair("page", &page.to_string());
            }
        }
        if p.c.kind == "cursor-origin"
            && let Some(u) = next.take()
        {
            url = u;
        }
        let data = get(p.c, url).await?;
        let list = if p.c.kind == "cursor-origin" && data["checkRuns"].is_null() {
            &vec![]
        } else {
            rows(&data, key)?
        };
        out.extend(list.iter().cloned());
        let continuation = match p.c.kind.as_str() {
            "bitbucket" => data["next"].as_str(),
            "cursor-origin" => data["nextPageToken"].as_str(),
            _ if list.len() == 100 => continue,
            _ => None,
        };
        let Some(token) = continuation.filter(|s| !s.is_empty()) else {
            return Ok(out);
        };
        if !seen.insert(token.to_owned()) {
            break;
        }
        next = Some(if p.c.kind == "bitbucket" {
            base.join(token)
                .ok()
                .filter(|u| {
                    u.origin() == base.origin()
                        && u.path() == base.path()
                        && u.username().is_empty()
                        && u.password().is_none()
                        && u.fragment().is_none()
                })
                .ok_or_else(|| invalid("check pagination destination"))?
        } else {
            let mut u = base.clone();
            u.query_pairs_mut()
                .append_pair("pageSize", "100")
                .append_pair("pageToken", token);
            u
        });
    }
    Err(invalid("check pagination exceeded limit"))
}
async fn collect(p: &Pull<'_>, head: &str) -> Result<Vec<Check>, ForgeError> {
    match p.c.kind.as_str() {
        "gitcafe" | "tanstack-source" => hosted::checks(p, head).await,
        "cursor-origin" => {
            let data = pages(
                p,
                child(p.repo.clone(), &["commits", head, "check-runs"]),
                Some("checkRuns"),
            )
            .await?;
            data.iter()
                .map(|v| {
                    Check::from(
                        v["name"].as_str(),
                        v["conclusion"].as_str().or(v["status"].as_str()),
                        v["detailsUrl"].as_str(),
                    )
                })
                .collect()
        }
        "forgejo" => {
            let mut url = child(p.repo.clone(), &["statuses", head]);
            url.query_pairs_mut().append_pair("sort", "recentupdate");
            let mut seen = BTreeSet::new();
            let mut checks = vec![];
            for row in pages(p, url, None).await? {
                let c = Check::from(
                    row["context"].as_str(),
                    row["status"].as_str().or(row["state"].as_str()),
                    row["target_url"].as_str(),
                )?;
                if seen.insert(c.name.clone()) {
                    checks.push(c);
                }
            }
            Ok(checks)
        }
        "bitbucket" => pages(p, p.endpoint(&["statuses"]), Some("values"))
            .await?
            .iter()
            .map(|v| {
                Check::from(
                    v["name"].as_str().or(v["key"].as_str()),
                    v["state"].as_str(),
                    v["url"].as_str(),
                )
            })
            .collect(),
        "gitlab" => {
            let Some(pipeline) = p
                .raw
                .get("head_pipeline")
                .or(p.raw.get("pipeline"))
                .filter(|v| !v.is_null())
            else {
                return Ok(vec![]);
            };
            let id = number(&pipeline["id"])?;
            let repo = match pipeline["project_id"].as_u64() {
                Some(id) => path(p.c, &["projects", &id.to_string()])?,
                None => p.repo.clone(),
            };
            let endpoint = child(repo, &["pipelines", &id.to_string()]);
            let mut checks = vec![];
            for kind in ["jobs", "bridges"] {
                let list = match pages(p, child(endpoint.clone(), &[kind]), None).await {
                    Ok(xs) => xs,
                    Err(e) if matches!(e.status, 403 | 404) => continue,
                    Err(e) => return Err(e),
                };
                for row in list {
                    checks.push(Check::from(
                        row["name"].as_str(),
                        row["status"].as_str(),
                        row["web_url"].as_str(),
                    )?);
                }
            }
            if checks.is_empty() {
                checks.push(Check::from(
                    Some("Pipeline"),
                    pipeline["status"].as_str(),
                    pipeline["web_url"].as_str(),
                )?);
            }
            Ok(checks)
        }
        "azure-devops" => {
            let project = p
                .raw
                .pointer("/repository/project/id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Azure project identity"))?;
            let mut url = azure::policy_evaluations(p.c)?;
            url.query_pairs_mut().append_pair(
                "artifactId",
                &format!("vstfs:///CodeReview/CodeReviewId/{project}/{}", p.n),
            );
            let mut checks = vec![];
            for row in pages(p, url, Some("value")).await? {
                let conf = &row["configuration"];
                if conf["isEnabled"] == false
                    || conf["isDeleted"] == true
                    || !matches!(
                        conf["type"]["id"]
                            .as_str()
                            .map(str::to_lowercase)
                            .as_deref(),
                        Some(
                            "0609b952-1397-4640-95ec-e00a01b2c241"
                                | "cbdc66da-9728-4af8-aada-9a5a32e4a226"
                        )
                    )
                {
                    continue;
                }
                if p.raw["isDraft"] == true
                    && row["status"] == "queued"
                    && row["context"]["buildId"].is_null()
                {
                    continue;
                }
                let name = conf["settings"]["displayName"]
                    .as_str()
                    .or(conf["settings"]["defaultDisplayName"].as_str())
                    .or(row["context"]["buildDefinitionName"].as_str())
                    .or(conf["settings"]["statusName"].as_str())
                    .or(conf["type"]["displayName"].as_str())
                    .unwrap_or("Policy");
                checks.push(Check::from(Some(name), row["status"].as_str(), None)?);
            }
            Ok(checks)
        }
        _ => Err(p.unsupported("checks")),
    }
}
pub(super) async fn run(p: &Pull<'_>, action: &Request) -> Result<Value, ForgeError> {
    let head = p.head()?;
    let (filter, lines) = match action {
        Request::Checks => (None, 200),
        Request::Logs { check, tail_lines } => {
            (check.as_deref(), usize::from(tail_lines.unwrap_or(200)))
        }
        Request::Rerun { check } => (check.as_deref(), 200),
        _ => return Err(invalid("CI action")),
    };
    if !(1..=2000).contains(&lines) {
        return Err(ForgeError::bad("tail_lines must be 1..2000"));
    }
    if !matches!(action, Request::Checks)
        && !matches!(p.c.kind.as_str(), "gitcafe" | "tanstack-source")
    {
        return Err(
            p.unsupported("loading check logs or rerunning jobs from the registered driver")
        );
    }
    let checks = collect(p, head).await?;
    if matches!(action, Request::Checks) {
        return Ok(
            json!({"head_sha":head,"checks":checks.iter().map(Check::value).collect::<Vec<_>>()}),
        );
    }
    let selected: Vec<_> = checks
        .iter()
        .filter(|c| {
            filter.map_or(c.status == "failure" || c.status == "cancelled", |name| {
                c.name == name
            })
        })
        .collect();
    if filter.is_some() && selected.is_empty() {
        return Err(ForgeError::bad("no such check at the pull head"));
    }
    let result =
        hosted::operate(p, &selected, matches!(action, Request::Rerun { .. }), lines).await?;
    Ok(json!({"head_sha":head,"checks":result}))
}
