use super::*;

pub(super) async fn create(
    State(mock): State<Mock>,
    Param(owner): Param<String>,
    Json(req): Json<Value>,
) -> Response {
    assert_eq!(owner, "kar-org");
    mock.lock().unwrap().publications += 1;
    let name = req["name"].as_str().unwrap();
    assert_eq!(req["private"], name == "private-repo");
    if name == "no-receipt" {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    Json(json!({"full_name":format!("{owner}/{name}"),"html_url":format!("https://review.example/{owner}/{name}"),"ssh_url":format!("git@review.example:{owner}/{name}.git")})).into_response()
}

pub(super) struct Later {
    project: String,
    request: Value,
    lost: Value,
    bare: std::path::PathBuf,
    work: std::path::PathBuf,
}

pub(super) async fn first(client: &reqwest::Client, url: &str, root: &Path, mock: &Mock) -> Later {
    let work = root.join("publish-source");
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    git(
        &work,
        &[
            "remote",
            "add",
            "origin",
            "https://review.example/untouched/origin",
        ],
    );
    let project = ok(client
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"publication"})))
    .await;
    let project = project["id"].as_str().unwrap().to_owned();
    let endpoint = format!("{url}/projects/{project}/forge/publish");
    let request = |name: &str, visibility: &str| json!({"id":Uuid::new_v4(),"provider":"github","host":"review.example","repository":format!("kar-org/{name}"),"visibility":visibility,"protocol":"https"});
    let empty = request("private-repo", "private");
    let result = ok(client.post(&endpoint).json(&empty)).await;
    assert_eq!(result["status"], "remote_added");
    assert_eq!(result["branch"], "main");
    assert_eq!(result["remoteName"], "origin-2");
    assert_eq!(ok(client.post(&endpoint).json(&empty)).await, result);
    assert_eq!(
        git(&work, &["config", "remote.origin.url"]),
        "https://review.example/untouched/origin"
    );
    fs::write(work.join("artifact"), "published bytes").unwrap();
    git(&work, &["add", "artifact"]);
    git(&work, &["commit", "-m", "first"]);
    let bare = root.join("publication-remote");
    git(
        &work,
        &[
            "config",
            &format!("url.{}.insteadOf", bare.display()),
            "https://review.example/kar-org/public-repo",
        ],
    );
    let requested = request("public-repo", "public");
    let response = client
        .post(&endpoint)
        .json(&requested)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(
        response.json::<Value>().await.unwrap()["detail"]
            .as_str()
            .unwrap()
            .contains("was created")
    );
    let lost = request("no-receipt", "public");
    let response = client.post(&endpoint).json(&lost).send().await.unwrap();
    assert_eq!(
        response.json::<Value>().await.unwrap()["kind"],
        "needs_reconciliation"
    );
    assert_eq!(mock.lock().unwrap().publications, 3);
    Later {
        project,
        request: requested,
        lost,
        bare,
        work,
    }
}
pub(super) async fn after_restart(client: &reqwest::Client, url: &str, mock: &Mock, later: Later) {
    let endpoint = format!("{url}/projects/{}/forge/publish", later.project);
    fs::create_dir(&later.bare).unwrap();
    git(&later.bare, &["init", "--bare"]);
    let done = ok(client.post(&endpoint).json(&later.request)).await;
    assert_eq!(done["status"], "pushed");
    assert_eq!(done["remoteName"], "origin-3");
    assert_eq!(
        git(&later.bare, &["show", "main:artifact"]),
        "published bytes"
    );
    assert_eq!(
        git(&later.work, &["config", "branch.main.remote"]),
        "origin-3"
    );
    assert_eq!(ok(client.post(&endpoint).json(&later.request)).await, done);
    let no = client
        .post(&endpoint)
        .json(&later.lost)
        .send()
        .await
        .unwrap();
    assert_eq!(
        no.json::<Value>().await.unwrap()["kind"],
        "needs_reconciliation"
    );
    assert_eq!(mock.lock().unwrap().publications, 3);
}
