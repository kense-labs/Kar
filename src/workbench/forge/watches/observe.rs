use super::*;
use crate::workbench::forge::providers::{get, issue, rest};
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Baseline {
    pub head: Option<String>,
    pub state: String,
    pub conflict: bool,
    pub rebase: bool,
    pub checks: Option<BTreeMap<String, String>>,
    pub remarks: Option<BTreeMap<String, String>>,
}
impl Baseline {
    pub fn settled(&self) -> bool {
        self.checks
            .as_ref()
            .is_some_and(|s| !s.is_empty() && s.values().all(|v| v != "pending" && v != "approval"))
    }
    pub fn passed(&self) -> bool {
        self.settled()
            && self
                .checks
                .as_ref()
                .is_some_and(|s| s.values().all(|v| v == "success"))
    }
    pub fn changes(&self, old: &Self) -> Vec<String> {
        let mut out = vec![];
        if self.state != old.state && matches!(self.state.as_str(), "merged" | "closed") {
            out.push(format!("pull request {}", self.state));
        }
        if self.conflict && !old.conflict {
            out.push("merge conflicts detected".into());
        }
        if self.rebase && !old.rebase {
            out.push("the branch now needs rebasing".into());
        }
        if let (Some(checks), Some(prior)) = (&self.checks, &old.checks) {
            for (name, status) in checks {
                if matches!(status.as_str(), "failure" | "approval")
                    && (old.head != self.head || prior.get(name) != Some(status))
                {
                    out.push(format!("check {name}: {status}"));
                }
            }
            if self.passed() && (!old.passed() || old.head != self.head) {
                out.push(format!("all {} reported checks passed", checks.len()));
            } else if self.settled() && !old.settled() && !self.passed() {
                out.push("all reported checks have finished".into());
            }
        }
        if let (Some(now), Some(prior)) = (&self.remarks, &old.remarks) {
            for (id, preview) in now {
                if !prior.contains_key(id) {
                    out.push(format!("new review activity {id}: {preview}"));
                }
            }
        }
        out
    }
}
async fn github_remarks(c: &Context, n: u64) -> Result<BTreeMap<String, String>, ForgeError> {
    let repo = rest::project(c)?;
    let n = n.to_string();
    let mut remarks = BTreeMap::new();
    for (kind, path) in [
        ("comment", vec!["issues", &n, "comments"]),
        ("review", vec!["pulls", &n, "reviews"]),
        ("inline", vec!["pulls", &n, "comments"]),
    ] {
        for page in 1..=1000 {
            let mut url = issue::child(repo.clone(), &path);
            url.query_pairs_mut()
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let raw = get(c, url).await?;
            let rows = raw
                .as_array()
                .ok_or_else(|| ForgeError::bad("invalid pull discussion page"))?;
            for row in rows {
                let id = row["id"]
                    .as_u64()
                    .ok_or_else(|| ForgeError::bad("missing pull discussion identity"))?;
                let author = row["user"]["login"].as_str().unwrap_or("unknown");
                let detail = row["body"]
                    .as_str()
                    .filter(|b| !b.trim().is_empty())
                    .or(row["state"].as_str())
                    .unwrap_or("");
                remarks.insert(
                    format!("{kind}/{id}"),
                    format!("{author}: {}", detail.chars().take(200).collect::<String>()),
                );
            }
            if rows.len() < 100 {
                break;
            }
            if page == 1000 {
                return Err(ForgeError::bad("pull discussion exceeds page bound"));
            }
        }
    }
    Ok(remarks)
}
pub(super) async fn poll(c: &Context, n: u64) -> Result<Baseline, ForgeError> {
    let raw = c.execute(&Operation::Pull { number: n }).await?;
    let mut b = Baseline {
        head: raw["head_sha"].as_str().map(str::to_owned),
        state: raw["state"]
            .as_str()
            .ok_or_else(|| ForgeError::bad("missing pull state"))?
            .into(),
        conflict: raw["mergeable"] == false || raw["mergeability"] == "conflicting",
        rebase: raw["mergeable_state"] == "behind",
        ..Default::default()
    };
    if b.state == "opened" {
        b.state = "open".into();
    }
    if b.state != "open" {
        return Ok(b);
    }
    if let Ok(raw) = c
        .execute(&Operation::PullCommand {
            number: n,
            command: PullCommand::Checks,
        })
        .await
        && raw["head_sha"].as_str() == b.head.as_deref()
        && let Some(checks) = raw["checks"].as_array()
    {
        let mut states = BTreeMap::new();
        for c in checks {
            let Some(name) = c["name"].as_str() else {
                continue;
            };
            let status = match c["conclusion"].as_str().or(c["status"].as_str()) {
                Some("success" | "neutral" | "skipped") => "success",
                Some("action_required" | "action-required") => "approval",
                Some(
                    "failure" | "failed" | "error" | "timed_out" | "cancelled" | "startup_failure"
                    | "stale",
                ) => "failure",
                _ => "pending",
            };
            states.insert(name.to_owned(), status.to_owned());
        }
        b.checks = Some(states);
    }
    if c.kind == "github" {
        b.remarks = github_remarks(c, n).await.ok();
    } else if let Ok(page) = c
        .execute(&Operation::ReviewThreads {
            number: n,
            cursor: None,
        })
        .await
        && let Some(threads) = page["threads"].as_array()
    {
        let mut remarks = BTreeMap::new();
        for thread in threads {
            for row in thread["comments"].as_array().into_iter().flatten() {
                if let Some(id) = row["id"].as_str() {
                    let author = row["author"].as_str().unwrap_or("unknown");
                    let body = row["body"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect::<String>();
                    remarks.insert(format!("review/{id}"), format!("{author}: {body}"));
                }
            }
        }
        b.remarks = Some(remarks);
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_head_can_wake_for_a_repeated_failure_and_old_comments_are_silent() {
        let old = Baseline {
            head: Some("old".into()),
            state: "open".into(),
            checks: Some(BTreeMap::from([("build".into(), "failure".into())])),
            remarks: Some(BTreeMap::from([("issue/1".into(), "old comment".into())])),
            ..Default::default()
        };
        assert!(old.changes(&old).is_empty());
        let mut next = old.clone();
        next.head = Some("new".into());
        next.remarks
            .as_mut()
            .unwrap()
            .insert("review/2".into(), "new feedback".into());
        assert_eq!(
            next.changes(&old),
            [
                "check build: failure",
                "new review activity review/2: new feedback"
            ]
        );
        assert!(next.changes(&next).is_empty());
        next.state = "merged".into();
        assert!(
            next.changes(&old)
                .iter()
                .any(|s| s == "pull request merged")
        );
    }
}
