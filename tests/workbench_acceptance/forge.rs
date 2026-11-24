use super::*;
use axum::{
    Json, Router,
    extract::{Path as Param, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::{Arc, Mutex};
pub struct Peer {
    pub addr: std::net::SocketAddr,
    pub created: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort()
    }
}
#[derive(Clone)]
struct Db {
    bare: PathBuf,
    items: Arc<Mutex<Vec<Value>>>,
}
fn read_pr(s: &Db, n: u64) -> Option<Value> {
    let mut raw = s
        .items
        .lock()
        .unwrap()
        .get(n.checked_sub(1)? as usize)?
        .clone();
    for side in ["head", "base"] {
        let branch = raw[side]["ref"].as_str().unwrap();
        raw[side]["sha"] = json!(git(
            &s.bare,
            &["rev-parse", &format!("refs/heads/{branch}")]
        ));
    }
    Some(raw)
}
async fn pull(State(s): State<Db>, Param(n): Param<u64>) -> Response {
    match read_pr(&s, n) {
        Some(value) => Json(value).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn create(State(s): State<Db>, Json(v): Json<Value>) -> Response {
    let seq = s.items.lock().unwrap().len() + 1;
    if v["body"]
        .as_str()
        .is_none_or(|txt| !txt.contains("kar-operation:"))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let pr = json!({"number":seq, "state":"open","updated_at":"2026-11-01T10:00:00Z","title":v["title"],"body":v["body"],"html_url":format!("https://review.example/team/repo/pull/{seq}"),"head":{"ref":v["head"]},"base":{"ref":v["base"]}});
    s.items.lock().unwrap().push(pr);
    Json(read_pr(&s, seq as u64).unwrap()).into_response()
}
pub async fn serve(bare: &Path) -> Peer {
    let created = Arc::new(Mutex::new(vec![
        json!({"number":1,"title":"Initial PR","body":"Existing change","state":"open","html_url":"https://review.example/team/repo/pull/1","head":{"ref":"feature"},"base":{"ref":"main"}}),
    ]));
    let state = Db {
        bare: bare.into(),
        items: created.clone(),
    };
    let app=Router::new().route("/repos/team/repo/pulls/{n}",get(pull))
        .route("/repos/team/repo/pulls",axum::routing::post(create))
        .route("/repos/team/repo/issues/7",get(||async{Json(json!({"number":7,"title":"Coordinate release","state":"open","html_url":"https://review.example/team/repo/issues/7"}))}))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Peer {
        addr,
        created,
        task,
    }
}
pub fn seed(checkout: &Path, bare: &Path) {
    fs::create_dir_all(checkout).unwrap();
    fs::create_dir_all(bare).unwrap();
    git(bare, &["init", "--bare"]);
    git(checkout, &["init", "-b", "main"]);
    git(checkout, &["config", "user.name", "Kar Acceptance"]);
    git(
        checkout,
        &["config", "user.email", "acceptance@example.invalid"],
    );
    git(checkout, &["config", "commit.gpgSign", "false"]);
    fs::write(checkout.join("notes.md"), BASE_TEXT).unwrap();
    git(checkout, &["add", "notes.md"]);
    git(checkout, &["commit", "-m", "baseline"]);
    git(
        checkout,
        &[
            "remote",
            "add",
            "origin",
            "https://review.example/team/repo.git",
        ],
    );
    git(
        checkout,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            bare.to_str().unwrap(),
        ],
    );
    git(checkout, &["push", "origin", "main"]);
    git(checkout, &["switch", "-c", "feature"]);
    fs::write(checkout.join("issue.txt"), "issue-7").unwrap();
    git(checkout, &["add", "issue.txt"]);
    git(checkout, &["commit", "-m", "feature"]);
    git(checkout, &["push", "origin", "feature"]);
}
