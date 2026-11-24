//! The host's immutable revisions, fork and original filename must reach both blob reads.
use super::*;
use axum::{extract::Query, http::Uri};
use std::collections::HashMap;

const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TIP: &str = "cccccccccccccccccccccccccccccccccccccccc";
const OLD_PATH: &str = "dir/old #&.md";
const NEW_PATH: &str = "dir/new #&.md";

pub(super) fn github(router: Router<Mock>) -> Router<Mock> {
    router
        .route(
            "/repos/team/repo/compare/{refs}",
            get(|Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q["page"], "2");
                Json(json!({"merge_base_commit":{"sha":OLD}}))
            }),
        )
        .route(
            "/repos/team/repo/commits/{sha}",
            get(|Param(sha): Param<String>| async move { Json(json!({"sha":sha,"parents":[]})) }),
        )
        .route(
            "/repos/team/repo/contents/{*path}",
            get(
                |Param(p): Param<String>,
                 Query(q): Query<HashMap<String, String>>,
                 h: HeaderMap| async move {
                    assert_eq!(h["accept"], "application/vnd.github.raw+json");
                    assert_eq!(h["authorization"], "Bearer local-token");
                    match p.as_str() {
                        OLD_PATH => {
                            assert_eq!(q["ref"], OLD);
                            "before\n".into_response()
                        }
                        NEW_PATH => "after�\n".into_response(),
                        "binary" => vec![255u8, 1].into_response(),
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
}
pub(super) fn peer(kind: &str, path: &str, uri: &Uri) -> Option<Response> {
    let q = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    let read = |p: &str, rev: &str| -> &'static str {
        match p {
            OLD_PATH => {
                assert_eq!(rev, OLD, "{kind} before");
                "before\n"
            }
            NEW_PATH => {
                assert_eq!(rev, NEW, "{kind} after");
                "after�\n"
            }
            _ => panic!("unexpected file path {p}"),
        }
    };
    let native = match (kind, path) {
        ("forgejo", "repos/team/repo/pulls/19") => {
            json!({"number":19,"merge_base":OLD,"base":{"sha":TIP},"head":{"sha":NEW,"repo":{"full_name":"fork/topic"}}})
        }
        ("gitlab", "projects/team%2Frepo/merge_requests/19") => {
            json!({"iid":19,"diff_refs":{"start_sha":TIP,"base_sha":OLD,"head_sha":NEW}})
        }
        ("gitcafe", "repos/team/repo/pulls/19") => {
            json!({"number":19,"version":5,"headOid":NEW,"observedBaseOid":TIP,"sourceRepo":{"owner":"fork","name":"topic"}})
        }
        ("gitcafe", "repos/team/repo/pulls/19/changes") => {
            assert_eq!(q["expectedVersion"], "5");
            assert_eq!(q["limit"], "1");
            json!({"version":5,"headOid":NEW,"comparisonBaseOid":OLD})
        }
        ("tanstack-source", "repos/team/repo/pulls/19") => {
            json!({"number":19,"headRepo":"fork/topic"})
        }
        ("tanstack-source", "repos/team/repo/pulls/19/files") => {
            json!({"headSha":NEW,"mergeBaseSha":OLD})
        }
        ("azure-devops", "org/proj/_apis/git/repositories/repo/pullrequests/19/iterations") => {
            json!({"value":[{"id":2,"commonRefCommit":{"commitId":OLD},"sourceRefCommit":{"commitId":NEW}},{"id":1,"commonRefCommit":{"commitId":TIP},"sourceRefCommit":{"commitId":TIP}}]})
        }
        ("azure-devops", "org/proj/_apis/git/repositories/repo/items") => {
            assert_eq!(q["versionDescriptor.versionType"], "commit");
            assert_eq!(q["includeContentMetadata"], "true");
            assert_eq!(q["api-version"], "7.1");
            json!({"content":read(q["path"].strip_prefix('/').unwrap(),&q["versionDescriptor.version"])})
        }
        ("gitcafe", "repos/team/repo/blob" | "repos/fork/topic/blob") => {
            let p = &q["path"];
            assert_eq!(path.starts_with("repos/fork"), p == NEW_PATH);
            let contents = read(p, &q["oid"]);
            json!({"path":p,"commit":q["oid"],"size":contents.len(),"binary":false,"content":contents})
        }
        ("gitlab", p)
            if p.starts_with("projects/team%2Frepo/repository/files/") && p.ends_with("/raw") =>
        {
            let encoded = p
                .strip_prefix("projects/team%2Frepo/repository/files/")
                .unwrap()
                .strip_suffix("/raw")
                .unwrap();
            let name = percent_encoding::percent_decode_str(encoded)
                .decode_utf8()
                .unwrap();
            return Some(read(&name, &q["ref"]).into_response());
        }
        ("forgejo" | "tanstack-source", p) if p.contains("/contents/") => {
            let (repopath, encoded) = p.split_once("/contents/").unwrap();
            let name = percent_encoding::percent_decode_str(encoded)
                .decode_utf8()
                .unwrap();
            assert_eq!(repopath == "repos/fork/topic", name == NEW_PATH);
            let contents = read(&name, &q["ref"]);
            if kind == "forgejo" {
                use base64::Engine;
                json!({"encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(contents)})
            } else {
                json!({"content":contents,"binary":false,"truncated":false})
            }
        }
        _ => return None,
    };
    Some(Json(native).into_response())
}
pub(super) async fn verify(c: &reqwest::Client, forge: &str, kind: &str) {
    let number = if kind == "github" { 1 } else { 19 };
    let url = format!("{forge}/pulls/{number}/diff/contents");
    let mut input = json!({"changeType":"rename-changed","oldPath":OLD_PATH,"newPath":NEW_PATH});
    if matches!(kind, "cursor-origin" | "bitbucket") {
        let r = c.post(url).json(&input).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 422);
        return;
    }
    let actual = ok(c.post(&url).json(&input)).await;
    assert_eq!(
        actual,
        json!({"oldContents":"before\n","newContents":"after�\n"}),
        "{kind}"
    );
    if kind == "github" {
        input["commit"] = json!("a1b2c3d");
        input["changeType"] = json!("new");
        let root = ok(c.post(&url).json(&input)).await;
        assert_eq!(root["oldContents"], "");
        input["newPath"] = json!("binary");
        assert_eq!(
            c.post(&url)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            422
        );
        input["newPath"] = json!("../private");
        assert_eq!(
            c.post(&url)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            400
        );
    }
    if matches!(kind, "gitcafe" | "tanstack-source" | "azure-devops") {
        input["commit"] = json!(TIP);
        assert_eq!(
            c.post(&url)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            422
        );
    }
}
