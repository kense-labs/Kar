use super::*;
use issue::child;
mod ci;
mod edit;
mod merge;

struct Pull<'a> {
    c: &'a Context,
    n: u64,
    repo: Url,
    url: Url,
    raw: Value,
}
impl Pull<'_> {
    fn endpoint(&self, tail: &[&str]) -> Url {
        child(self.url.clone(), tail)
    }
    fn unsupported(&self, action: &str) -> ForgeError {
        ForgeError::unsupported(format!(
            "{} does not expose {action} for pull requests",
            self.c.kind
        ))
    }
    fn head(&self) -> Result<&str, ForgeError> {
        let pointer = match self.c.kind.as_str() {
            "gitlab" => "/sha",
            "bitbucket" => "/source/commit/hash",
            "azure-devops" => "/lastMergeSourceCommit/commitId",
            "gitcafe" => "/headOid",
            "tanstack-source" => "/headSha",
            _ => "/head/sha",
        };
        self.raw
            .pointer(pointer)
            .filter(|v| v.is_string())
            .or_else(|| {
                (self.c.kind == "gitlab")
                    .then(|| self.raw.pointer("/diff_refs/head_sha"))
                    .flatten()
            })
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("pull head"))
    }
}
pub(super) async fn execute(c: &Context, n: u64, action: &Request) -> Result<Value, ForgeError> {
    let id = n.to_string();
    let (repo, url, key) = match c.kind.as_str() {
        "gitlab" | "bitbucket" | "forgejo" => {
            let (path, key) = match c.kind.as_str() {
                "gitlab" => ("merge_requests", "iid"),
                "bitbucket" => ("pullrequests", "id"),
                _ => ("pulls", "number"),
            };
            let repo = rest::project(c)?;
            (repo.clone(), child(repo, &[path, &id]), key)
        }
        "gitcafe" | "cursor-origin" | "tanstack-source" => (
            hosted::resource(c, None, None)?,
            hosted::resource(c, Some("pulls"), Some(n))?,
            "number",
        ),
        "azure-devops" => (
            azure::git_endpoint(c, &[])?,
            azure::git_endpoint(c, &["pullrequests", &id])?,
            "pullRequestId",
        ),
        _ => return Err(ForgeError::unsupported("this driver has no pull requests")),
    };
    let raw = get(c, url.clone()).await?;
    if number(&raw[key])? != n {
        return Err(invalid("pull identity"));
    }
    if c.kind == "azure-devops" {
        azure::check_pull_repository(c, &raw)?;
    }
    let pull = Pull {
        c,
        n,
        repo,
        url,
        raw,
    };
    match action {
        Request::Merge {
            expected_head_sha,
            method,
            auto,
            ..
        } => {
            merge::apply(
                &pull,
                expected_head_sha,
                method.as_deref().unwrap_or("merge"),
                *auto,
            )
            .await
        }
        Request::Update { .. } => edit::apply(&pull, action).await,
        Request::Checks | Request::Logs { .. } | Request::Rerun { .. } => {
            ci::run(&pull, action).await
        }
        Request::Comment { body } => {
            if body.trim().is_empty() {
                return Err(ForgeError::bad("comment is empty"));
            }
            let (url, payload) = match c.kind.as_str() {
                "azure-devops" => return Err(pull.unsupported("comment")),
                "gitlab" => (pull.endpoint(&["notes"]), json!({"body":body})),
                "bitbucket" => (
                    pull.endpoint(&["comments"]),
                    json!({"content":{"raw":body}}),
                ),
                "gitcafe" => (pull.endpoint(&["comments", ""]), json!({"body":body})),
                "forgejo" | "tanstack-source" => (
                    child(pull.repo.clone(), &["issues", &id, "comments"]),
                    json!({"body":body}),
                ),
                _ => (pull.endpoint(&["comments"]), json!({"body":body})),
            };
            send(c, Method::POST, url, Some(payload), true).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Request as HttpRequest, State},
        http::StatusCode,
        routing::any,
    };
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;
    async fn native(State(calls): State<Calls>, request: HttpRequest) -> (StatusCode, Json<Value>) {
        let method = request.method().to_string();
        let path = request.uri().path().to_owned();
        let bytes = axum::body::to_bytes(request.into_body(), 32 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
        calls
            .lock()
            .unwrap()
            .push((method.clone(), path.clone(), body));
        let data = match (method.as_str(), path.as_str()) {
            ("GET", "/repos/team/repo/pulls/2") => {
                json!({"number":2,"headSha":"123abc","headOid":"123abc","version":4})
            }
            ("GET", "/repos/team/repo/commits/123abc/checks") => {
                json!({"items":[{"id":"1","name":"build","status":"completed","conclusion":"failure","detailsUrl":"https://github.com/team/repo/actions/runs/5/job/7"},{"id":"2","name":"sync","status":"completed","conclusion":"failure","detailsUrl":"https://git.cafe/team/repo/actions/runs/actwfrun_42"}],"next":null})
            }
            ("GET", "/repos/team/repo/ci/github/checks/7/log") => {
                json!({"content":"first\nlast\n","truncated":false})
            }
            ("POST", "/repos/team/repo/actions/runs/actwfrun_42/rerun") => Value::Null,
            ("PATCH", "/repos/team/repo/pulls/2") => json!({"version":5}),
            ("POST", "/repos/team/repo/pulls/2/close") => json!({"version":6}),
            ("GET", "/repos/team/repo/check-runs") => {
                json!([{"jobId":"build1","name":"build","status":"completed","conclusion":"failure","startedAt":20}])
            }
            ("GET", "/repos/team/repo/ci/runs") => {
                json!({"items":[{"id":"a2","workflow":"CI","headSha":"123abc"}],"next":null})
            }
            ("GET", "/repos/team/repo/ci/runs/a2") => {
                json!({"id":"a2","jobs":[{"key":"build","jobId":"build1"}]})
            }
            ("POST", "/repos/team/repo/ci/runs/a2/rerun") => Value::Null,
            _ => return (StatusCode::NOT_FOUND, Json(Value::Null)),
        };
        (StatusCode::OK, Json(data))
    }
    #[tokio::test]
    async fn cafe_and_source_ci_only_select_jobs_observed_at_this_pull_head() {
        let calls = Calls::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let router = Router::new()
            .fallback(any(native))
            .with_state(calls.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut c = Context {
            kind: "gitcafe".into(),
            host: "git.cafe".into(),
            repository: "team/repo".into(),
            api_base,
            token: None,
            client: reqwest::Client::new(),
            signin: None,
            cwd: Default::default(),
        };
        let result = execute(
            &c,
            2,
            &Request::Logs {
                check: Some("build".into()),
                tail_lines: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["checks"][0]["log"], "last");
        assert_eq!(result["checks"][0]["truncated"], true);
        let rerun = execute(
            &c,
            2,
            &Request::Rerun {
                check: Some("build".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(rerun["checks"][0]["rerun"], false);
        execute(
            &c,
            2,
            &Request::Rerun {
                check: Some("sync".into()),
            },
        )
        .await
        .unwrap();
        execute(
            &c,
            2,
            &Request::Update {
                title: Some("new".into()),
                state: Some("closed".into()),
                body: None,
                add_labels: None,
                remove_labels: None,
                request_reviewers: None,
                ready: None,
            },
        )
        .await
        .unwrap();
        let writes = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m != "GET")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[1].2["expectedVersion"], 4);
        assert_eq!(writes[2].2["expectedVersion"], 5);
        c.kind = "tanstack-source".into();
        let result = execute(
            &c,
            2,
            &Request::Rerun {
                check: Some("CI/build".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["checks"][0]["run_id"], "a2");
        assert!(
            execute(
                &c,
                2,
                &Request::Merge {
                    expected_head_sha: "other".into(),
                    method: None,
                    auto: false,
                    stack: false,
                }
            )
            .await
            .is_err()
        );
        server.abort();
    }
}
