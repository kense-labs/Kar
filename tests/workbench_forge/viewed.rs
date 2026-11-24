use super::*;
use axum::http::Uri;

const NAME: &str = "dir/white space.txt";
const HEX: &str = "abcdef1";
pub(super) fn github(db: &mut Forge, q: &str, v: &Value) -> Option<Value> {
    if q.starts_with("query KarViewedPage") {
        let first = v["after"].is_null();
        let row = if first {
            json!({"path":"notes.txt","viewerViewedState":if db.viewed{"VIEWED"}else{"UNVIEWED"}})
        } else {
            json!({"path":"again.txt","viewerViewedState":"DISMISSED"})
        };
        return Some(
            json!({"repository":{"pullRequest":{"files":{"nodes":[row],"pageInfo":{"hasNextPage":first,"endCursor":"last"}}}}}),
        );
    }
    if q.starts_with("query KarViewedPull") {
        return Some(json!({"repository":{"pullRequest":{"id":"pr-1"}}}));
    }
    if q.starts_with("mutation KarSetViewed") {
        assert_eq!(v["id"], "pr-1");
        assert_eq!(v["p0"], "notes.txt");
        assert_eq!(v["p1"], "notes.txt");
        assert!(q.contains("f0:unmarkFileAsViewed") && q.contains("f1:markFileAsViewed"));
        db.viewed = true;
        return Some(json!({"f0":{},"f1":{}}));
    }
    None
}
pub(super) async fn host(c: &reqwest::Client, forge: &str) {
    let url = format!("{forge}/pulls/1/files-viewed");
    let before = ok(c.get(&url)).await;
    assert_eq!(before["files"][0]["state"], "unviewed");
    assert_eq!(before["files"][1]["state"], "dismissed");
    ok(c.put(&url).json(
        &json!({"files":[{"path":"notes.txt","viewed":false},{"path":"notes.txt","viewed":true}]}),
    ))
    .await;
    let after = ok(c.get(&url)).await;
    assert_eq!(after["files"][0]["state"], "viewed");
    assert_eq!(after["truncated"], false);
}
pub(super) fn peer(kind: &str, path: &str, uri: &Uri, b: &Value) -> Option<Response> {
    let params = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    let result = match (kind, path) {
        ("gitlab", "projects/team%2Frepo/merge_requests/42") => {
            json!({"iid":42,"diff_refs":{"head_sha":"current-head"}})
        }
        ("gitlab", "graphql")
            if b["query"]
                .as_str()
                .is_some_and(|q| q.starts_with("query KarViewedBlobs")) =>
        {
            assert_eq!(b["variables"]["ref"], "current-head");
            assert!(
                b["variables"]["paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| *p == NAME)
            );
            json!({"data":{"project":{"repository":{"blobs":{"nodes":[{"path":NAME,"oid":HEX}]}}}}})
        }
        ("azure-devops", "org/proj/_apis/git/repositories/repo/pullrequests/42/iterations") => {
            json!({"value":[{"id":4,"sourceRefCommit":{"commitId":"head"},"commonRefCommit":{"commitId":"base"}}]})
        }
        (
            "azure-devops",
            "org/proj/_apis/git/repositories/repo/pullrequests/42/iterations/4/changes",
        ) => {
            assert_eq!(params["$top"], "2000");
            if params["$skip"] == "0" {
                json!({"changeEntries":[],"nextSkip":2})
            } else {
                assert_eq!(params["$skip"], "2");
                json!({"changeEntries":[{"changeType":"edit","item":{"path":format!("/{NAME}"),"objectId":HEX}}]})
            }
        }
        ("bitbucket", "repositories/team/repo/pullrequests/42/diff") => {
            return Some(
                axum::response::Redirect::temporary("/bitbucket/repositories/team/repo/diff/head")
                    .into_response(),
            );
        }
        ("forgejo", "repos/team/repo/pulls/42.diff")
        | ("bitbucket", "repositories/team/repo/diff/head") => {
            // A hunk can contain text that looks like another file header.
            let patch = format!(
                "diff --git a/{NAME} b/{NAME}\nindex 0000001..{HEX} 100644\n--- a/{NAME}\n+++ b/{NAME}\n@@ -1 +1 @@\n--- text\n+++ b/fake\n"
            );
            return Some(patch.into_response());
        }
        _ => return None,
    };
    Some(Json(result).into_response())
}
pub(super) async fn environment(c: &reqwest::Client, forge: &str, kind: &str) {
    let url = format!("{forge}/pulls/42/files-viewed");
    if matches!(kind, "gitcafe" | "tanstack-source" | "cursor-origin") {
        assert_eq!(c.get(url).send().await.unwrap().status().as_u16(), 422);
        return;
    }
    ok(c.put(&url).json(&json!({"files":[{"path":NAME,"viewed":true,"revision":"older"},{"path":"missing","viewed":true,"revision":"formerly-present"}]}))).await;
    let marks = ok(c.get(&url)).await;
    assert_eq!(
        marks["files"],
        json!([{"path":NAME,"state":"dismissed"},{"path":"missing","state":"dismissed"}]),
        "{kind}"
    );
    ok(c.put(&url)
        .json(&json!({"files":[{"path":NAME,"viewed":true},{"path":"missing","viewed":false}]})))
    .await;
    assert_eq!(
        ok(c.get(&url)).await["files"],
        json!([{"path":NAME,"state":"viewed"}]),
        "{kind}"
    );
}
