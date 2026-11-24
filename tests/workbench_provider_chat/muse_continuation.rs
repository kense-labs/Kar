//! Exercise the normal authenticated workbench against a deterministic MSP peer.
use super::{
    emit,
    workbench_server::{client, json as ok, serve_with_env},
};
use serde_json::{Value, json};
use std::{ffi::OsStr, fs, io::BufRead, path::Path, time::Duration};
use uuid::Uuid;

fn notification(method: &str, session: &str, mut params: Value) {
    params["sessionId"] = session.into();
    emit(json!({"jsonrpc":"2.0","method":method,"params":params}));
}
fn completed_item(session: &str, turn: &str, item: &str, text: &str) {
    notification(
        "item/completed",
        session,
        json!({"turnId":turn,"item":{"itemId":item,"turnId":turn,"kind":"agentMessage","status":"completed","revision":1,"text":text}}),
    );
}
fn child_request(session: &str) {
    notification(
        "approval/requested",
        session,
        json!({
            "approvalId":"child-approval","turnId":"workflow-run","subagentOrigin":{"subagentId":"child"},
            "currentRequirementId":{"approvalId":"child-approval","sourceIndex":3},
            "availableChoices":[{"choiceId":"no","decision":"denied","scope":"once"},{"choiceId":"yes","decision":"approved","scope":"once"}]
        }),
    );
}

pub(super) fn peer(args: &[String]) {
    assert_eq!(args, ["serve", "--trust-workspace"]);
    let mut session = String::new();
    for line in std::io::stdin().lock().lines() {
        let r: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let p = &r["params"];
        let reply = |value: Value| emit(json!({"jsonrpc":"2.0","id":r["id"],"result":value}));
        match r["method"].as_str().unwrap() {
            "initialize" => reply(
                json!({"schema":{"version":1,"fingerprint":"fixture"},"grantedCapabilities":[]}),
            ),
            "initialized" => {}
            "session/start" | "session/resume" => {
                session = p["sessionId"].as_str().unwrap().into();
                let resumed = r["method"] == "session/resume";
                if resumed {
                    assert_eq!(
                        session,
                        fs::read_to_string("original-muse-session").unwrap()
                    );
                    // MSP is allowed to write notifications before the RPC receipt.
                    notification(
                        "turn/started",
                        &session,
                        json!({"turnId":"already-running"}),
                    );
                } else {
                    fs::write("original-muse-session", &session).unwrap();
                }
                reply(
                    json!({"session":{"sessionId":session,"activeTurnId":resumed.then_some("already-running")}}),
                );
            }
            "session/setApprovalMode" => reply(json!({})),
            "turn/start" => {
                let native = p["commandId"].as_str().unwrap();
                reply(json!({"turnId":native,"commandId":native}));
                notification("turn/started", &session, json!({"turnId":native}));
                if p["displayText"] == "lose active host" {
                    std::process::exit(63);
                } else if p["displayText"] == "first" {
                    notification("turn/started", &session, json!({"turnId":"joined"}));
                    completed_item(&session, "joined", "joined-item", "joined output");
                    notification(
                        "turn/completed",
                        &session,
                        json!({"turnId":"joined","terminal":"completed"}),
                    );
                    completed_item(&session, native, "answer", "first answer");
                    notification(
                        "turn/completed",
                        &session,
                        json!({"turnId":native,"terminal":"completed"}),
                    );
                    notification("turn/started", &session, json!({"turnId":"wake"}));
                    completed_item(&session, "wake", "wake-item", "autonomous answer");
                    notification(
                        "turn/completed",
                        &session,
                        json!({"turnId":"wake","terminal":"completed"}),
                    );
                    // A replay has the same native run and must not create another turn.
                    notification("turn/started", &session, json!({"turnId":"wake"}));
                    notification(
                        "turn/completed",
                        &session,
                        json!({"turnId":"wake","terminal":"completed"}),
                    );
                    child_request(&session);
                    child_request(&session);
                } else {
                    // The unowned pre-restart native run must never settle this input.
                    completed_item(&session, "already-running", "stale", "stale output");
                    notification(
                        "turn/completed",
                        &session,
                        json!({"turnId":"already-running","terminal":"failed"}),
                    );
                    completed_item(&session, native, "second-reply", "resumed answer");
                    notification(
                        "turn/completed",
                        &session,
                        json!({"turnId":native,"terminal":"completed"}),
                    );
                }
            }
            "approval/decide" => {
                assert_eq!(p["approvalId"], "child-approval");
                assert_eq!(p["requirementId"]["sourceIndex"], 3);
                assert_eq!(p["choiceId"], "no");
                assert_eq!(p["sessionId"], session);
                let marker = Path::new("decision-seen");
                assert!(!marker.exists(), "duplicate child approval decision");
                fs::write(marker, b"confirmed").unwrap();
                reply(json!({}));
                notification(
                    "approval/resolved",
                    &session,
                    json!({"approvalId":"child-approval"}),
                );
                child_request(&session);
            }
            other => panic!("unexpected MSP method {other}"),
        }
    }
}

async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let state = temp.path().join("state");
    fs::create_dir(&work).unwrap();
    let executable = std::env::current_exe().unwrap();
    let env = [
        ("KAR_MUSE_BIN", executable.as_os_str()),
        ("KAR_CHAT_TEST_PROVIDER", OsStr::new("muse")),
        ("KAR_MUSE_TEST_SCENARIO", OsStr::new("continuation")),
    ];
    let a = client();
    let b = client();
    let (mut service, url) = serve_with_env(&state, &work, temp.path(), &env).await;
    let project = ok(a
        .post(format!("{url}/projects"))
        .json(&json!({"root":work,"name":"Muse test"})))
    .await;
    let id = Uuid::new_v4();
    let project = project["id"].as_str().unwrap();
    let path = format!("/projects/{project}/chats");
    ok(a.post(format!("{url}{path}"))
        .json(&json!({"id":id,"runtime":"muse","approvals":"host"})))
    .await;
    let chat = format!("{url}{path}/{id}");
    let first = ok(a
        .post(format!("{chat}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"first"})))
    .await;
    assert_eq!(first["status"], "completed");
    assert!(
        first.to_string().contains("first answer") && first.to_string().contains("joined output")
    );
    let (wake, request) = loop {
        let snap = ok(b.get(format!("{chat}/snapshot"))).await;
        let turns = snap["history"]["turns"].as_array().unwrap();
        if turns.len() == 2
            && turns[1]["status"] == "completed"
            && let Some(req) = snap["history"]["pending_requests"].get(0)
        {
            assert_eq!(turns[1]["native_continuation"], true);
            assert_eq!(turns[1]["native_turn_id"], "wake");
            assert!(turns[1].to_string().contains("autonomous answer"));
            assert!(!turns[1].to_string().contains("first answer"));
            assert_eq!(req["id"], "child-approval");
            break (turns[1]["id"].clone(), req.clone());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let decision =
        json!({"id":request["id"],"cursor":request["cursor"],"action":"decide","decision":"deny"});
    ok(b.post(format!("{chat}/requests/respond")).json(&decision)).await;
    loop {
        let snap = ok(a.get(format!("{chat}/snapshot"))).await;
        if snap["history"]["pending_requests"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !a.post(format!("{chat}/requests/respond"))
            .json(&decision)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    service.kill().await.unwrap();
    service.wait().await.unwrap();
    let (mut next, new_url) = serve_with_env(&state, &work, temp.path(), &env).await;
    let chat = format!("{new_url}{path}/{id}");
    assert!(
        !a.post(format!("{chat}/requests/respond"))
            .json(&decision)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let next_turn = ok(a
        .post(format!("{chat}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":"second"})))
    .await;
    assert_eq!(next_turn["status"], "completed");
    assert!(next_turn.to_string().contains("resumed answer"));
    let snap = ok(b.get(format!("{chat}/snapshot"))).await;
    let turns = snap["history"]["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[1]["id"], wake);
    assert!(!turns.iter().any(|t| t.to_string().contains("stale output")));
    let lost = Uuid::new_v4();
    a.post(format!("{chat}/messages"))
        .json(&json!({"id":lost,"text":"lose active host"}))
        .send()
        .await
        .unwrap();
    let snap = ok(b.get(format!("{chat}/snapshot"))).await;
    let failed = snap["history"]["turns"].as_array().unwrap().last().unwrap();
    assert_eq!(failed["id"], lost.to_string());
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["failure"]["class"], "runtime_exited");
    next.kill().await.unwrap();
    next.wait().await.unwrap();
}
#[tokio::main]
pub(super) async fn run() {
    tokio::time::timeout(Duration::from_secs(90), scenario())
        .await
        .expect("Muse continuation deadline");
}
