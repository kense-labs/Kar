//! Explicitly selected disposable GitHub repository. Creates a topic branch and one review PR.
use super::*;
use reqwest::Client;

pub(super) async fn run() {
    let name = std::env::var("KAR_FORGE_TEST_REPO")
        .expect("set KAR_FORGE_TEST_REPO to a writable disposable owner/repository");
    let parts: Vec<_> = name.split('/').collect();
    assert!(
        parts.len() == 2
            && parts.iter().all(|p| !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
    );
    let token = std::env::var("GH_TOKEN")
        .expect("GH_TOKEN for the selected test repository and a configured Git credential helper");
    let temp = tempfile::tempdir().unwrap();
    let roots = temp.path().join("projects");
    fs::create_dir(&roots).unwrap();
    let repo = roots.join("repo");
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    let (mut process, url) = server::serve(&temp.path().join("state"), &roots, &native).await;
    let host = server::client();
    let project=ok(host.post(format!("{url}/projects/clone")).json(&json!({"source":format!("https://github.com/{name}.git"),"directory":repo,"name":"hosted test"}))).await;
    let project_id = project["id"].as_str().unwrap();
    let scope = format!("{url}/projects/{project_id}");
    let identity = ok(host
        .post(format!("{scope}/forge/query"))
        .json(&json!({"op":"identity"})))
    .await;
    assert!(name.eq_ignore_ascii_case(identity["repository"].as_str().unwrap()));
    let base = identity["default_branch"].as_str().unwrap();
    let id = Uuid::new_v4();
    let branch = format!("kar-test-{id}");
    let tree = roots.join("topic");
    ok(host.post(format!("{scope}/git")).json(
        &json!({"op":"worktree_add","id":id,"branch":branch,"directory":tree,"start":"HEAD"}),
    ))
    .await;
    let work = ok(host
        .post(format!("{url}/projects"))
        .json(&json!({"name":"topic","root":tree})))
    .await;
    let topic = format!("{url}/projects/{}", work["id"].as_str().unwrap());
    let chat = Uuid::new_v4();
    ok(host
        .post(format!("{topic}/chats"))
        .json(&json!({"id":chat,"runtime":"codex"})))
    .await;
    let note = format!("kar-fixture-{id}.txt");
    ok(host
        .put(format!("{topic}/file"))
        .query(&[("path", &note)])
        .json(&json!({"text":id.to_string()})))
    .await;
    for op in [
        json!({"op":"stage","paths":[note]}),
        json!({"op":"commit","message":format!("Verify Kar review {id}")}),
        json!({"op":"push"}),
    ] {
        ok(host.post(format!("{topic}/git")).json(&op)).await;
    }
    let input = json!({"id":id,"chat":chat,"op":"create_pull","title":format!("Kar acceptance {id}"),"body":"Created by the Kar backend acceptance run.","head":branch,"base":base});
    let pr = ok(host.post(format!("{topic}/forge/operations")).json(&input)).await;
    let n = pr["number"].as_u64().unwrap();
    assert_eq!(
        ok(host.post(format!("{topic}/forge/operations")).json(&input)).await,
        pr
    );
    let diff = ok(host.get(format!("{topic}/forge/pulls/{n}/diff"))).await;
    assert!(diff["files"].as_array().unwrap().iter().any(|f| f == &note));
    ok(host
        .put(format!("{topic}/forge/pulls/{n}/reviewed"))
        .json(&json!({"head":pr["head_sha"],"path":note,"reviewed":true})))
    .await;
    // Close only the pull created above and retain its address for inspection.
    let api = Client::builder()
        .user_agent("Kar acceptance")
        .build()
        .unwrap();
    let endpoint = format!("https://api.github.com/repos/{name}/pulls/{n}");
    let actual: Value = api
        .get(&endpoint)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(actual["head"]["ref"], branch);
    assert!(
        actual["body"]
            .as_str()
            .unwrap()
            .contains(&format!("kar-operation:{id}"))
    );
    api.patch(endpoint)
        .bearer_auth(&token)
        .json(&json!({"state":"closed"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let closed = ok(host
        .post(format!("{topic}/forge/query"))
        .json(&json!({"op":"pull","number":n})))
    .await;
    assert_eq!(closed["state"], "closed");
    println!(
        "{}",
        json!({"repository":name,"branch":branch,"chat":chat,"pull":closed["url"],"head":closed["head_sha"],"state":closed["state"]})
    );
    process.kill().await.unwrap();
}
