//! Compile now; the unified macOS run executes the CLI, process restarts, and a
//! Codex protocol peer (the latter is not evidence of a live provider result).
#[path = "support/codex_chat_peer.rs"]
mod codex_chat_peer;
#[path = "support/workbench_server.rs"]
mod server;

use hmac::{Hmac, KeyInit, Mac};
use kar::{
    chat::ChatStore,
    workbench::{
        Workbench,
        workflows::{Clock, Definition, Schedule, Status, Step},
    },
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use server::json as ok;
use std::{
    fs,
    sync::{
        Arc, Barrier,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

struct ManualClock(AtomicI64);
impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
fn definition(schedule: Schedule) -> Definition {
    Definition {
        name: "fixture".into(),
        enabled: true,
        schedule,
        steps: vec![Step::Ask {
            question: "Proceed?".into(),
        }],
    }
}
fn ms(stamp: &str) -> i64 {
    stamp.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}

async fn scheduling() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("store");
    let t = ms("2025-01-06T08:59:00Z"); // Monday, before 09:00 UTC.
    let clock = Arc::new(ManualClock(AtomicI64::new(t)));
    let open = || {
        Workbench::open_with_workflow_clock(
            ChatStore::open(&db).unwrap(),
            std::slice::from_ref(&root),
            clock.clone(),
        )
        .unwrap()
    };
    let app = open();
    let project = app.register(&root, "project").unwrap().id;
    let other_root = root.join("second");
    fs::create_dir(&other_root).unwrap();
    let other = app.register(&other_root, "second").unwrap().id;
    let once = Uuid::new_v4();
    let every = Uuid::new_v4();
    let daily = Uuid::new_v4();
    let within = Uuid::new_v4();
    let fixed = |time: &str| Schedule::FixedTime {
        time: time.into(),
        weekdays: vec![1],
        zone: Some("UTC".into()),
    };
    for (id, schedule, next) in [
        (once, Schedule::Once { at_ms: t + 60_000 }, t + 60_000),
        (every, Schedule::Interval { every_ms: 60_000 }, t + 60_000),
        (daily, fixed("09:00"), t + 60_000),
        (within, fixed("09:01"), t + 120_000),
    ] {
        assert_eq!(
            app.save_workflow(project, id, definition(schedule), None)
                .unwrap()
                .next_ms,
            Some(next)
        );
    }
    // Manual delivery and its retry before the one-off instant leave it scheduled.
    let op = Uuid::new_v4();
    let manual = app
        .trigger_workflow(project, once, op, json!("one"))
        .unwrap();
    assert_eq!(
        app.trigger_workflow(project, once, op, json!("one"))
            .unwrap()
            .id,
        manual.id
    );
    assert!(
        app.trigger_workflow(project, once, op, json!("different"))
            .is_err()
    );
    assert!(app.workflow_run(other, manual.id).is_err());
    drop(app);
    clock.0.store(t + 12 * 60_000, Ordering::SeqCst);
    let app = open();
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let app = app.clone();
        let barrier = barrier.clone();
        workers.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            app.tick_workflows().unwrap()
        }));
    }
    let mut count = 0;
    for worker in workers {
        count += worker.await.unwrap();
    }
    assert_eq!(count, 3); // once, one coalesced interval and daily exactly ten minutes late.
    let defs = app.workflows(project).unwrap();
    let find = |id| defs.iter().find(|d| d.id == id).unwrap();
    assert_eq!(find(once).next_ms, None);
    assert_eq!(find(every).next_ms, Some(t + 13 * 60_000));
    assert_eq!(find(daily).last_missed_ms, Some(t + 60_000));
    assert_eq!(find(daily).next_ms, Some(ms("2025-01-13T09:00:00Z")));
    for (id, n) in [(once, 2), (every, 1), (daily, 0), (within, 1)] {
        let runs = app.workflow_runs(project, id).unwrap();
        assert_eq!(runs.len(), n);
        assert!(runs.iter().all(|r| r.status == Status::Asking));
    }
    assert_eq!(app.tick_workflows().unwrap(), 0);
    let ny = Schedule::FixedTime {
        time: "09:00".into(),
        weekdays: vec![],
        zone: Some("America/New_York".into()),
    };
    assert_eq!(
        ny.next_after(ms("2025-03-08T14:01:00Z")).unwrap(),
        Some(ms("2025-03-09T13:00:00Z"))
    );
}

const SECRET: &str = "kar-fixture-webhook-key";
fn tag(body: &[u8]) -> String {
    use std::fmt::Write;
    let mut mac = Hmac::<sha2_mac::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body);
    let mut tag = "sha256=".to_owned();
    for b in mac.finalize().into_bytes() {
        write!(tag, "{b:02x}").unwrap();
    }
    tag
}

async fn wait(host: &Client, base: &str, p: &str, id: &str, status: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let run = ok(host.get(format!("{base}/projects/{p}/workflow-runs/{id}"))).await;
            if run["status"] == status {
                return run;
            }
            if run["status"] == "failed"
                || run["status"] == "reconcile"
                || run["status"] == "cancelled"
                || run["status"] == "completed"
            {
                panic!("unexpected workflow result while waiting for {status}: {run}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
async fn save(host: &Client, base: &str, p: &str, schedule: Value, steps: Value) -> Uuid {
    let id = Uuid::new_v4();
    let def=ok(host.put(format!("{base}/projects/{p}/workflows/{id}")).json(&json!({"name":"pipeline","enabled":true,"schedule":schedule,"steps":steps,"webhook_secret":SECRET}))).await;
    assert!(def.get("webhook_secret").is_none());
    id
}
async fn start(host: &Client, base: &str, p: &str, id: Uuid) -> String {
    ok(host
        .post(format!("{base}/projects/{p}/workflows/{id}/runs"))
        .json(&json!({"operation":Uuid::new_v4(),"input":{"repo":"test"}})))
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned()
}
async fn action(host: &Client, base: &str, p: &str, id: &str, body: &Value) -> Value {
    ok(host
        .post(format!("{base}/projects/{p}/workflow-runs/{id}/actions"))
        .json(body))
    .await
}

async fn http_scenarios() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    let state = dir.path().join("state");
    let native = dir.path().join("native");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&native).unwrap();
    let host = server::client();
    let remote = Client::new();
    let (mut child, mut base) = server::serve(&state, &root, &native).await;
    let project = ok(host
        .post(format!("{base}/projects"))
        .json(&json!({"name":"fixture","root":root})))
    .await;
    let p = project["id"].as_str().unwrap();
    let hook=save(&host,&base,p,json!({"type":"webhook"}),json!([{"type":"script","command":"test -z \"$KAR_WORKBENCH_TOKEN\" && printf H >> hooks"}])).await;
    let url = format!("{base}/hooks/workflows/{p}/{hook}");
    let body = serde_json::to_vec(&json!({"id":"event-1","input":{"message":"signed"}})).unwrap();
    let post = |signature: &str| {
        remote
            .post(&url)
            .header("x-kar-signature", signature)
            .body(body.clone())
    };
    assert_eq!(
        post(&tag(b"tampered")).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert!(!root.join("hooks").exists());
    let (first, duplicate) = tokio::join!(ok(post(&tag(&body))), ok(post(&tag(&body))));
    assert_eq!(first, duplicate);
    wait(
        &host,
        &base,
        p,
        first["run_id"].as_str().unwrap(),
        "completed",
    )
    .await;
    assert_eq!(fs::read(root.join("hooks")).unwrap(), b"H");
    assert_eq!(
        ok(host.get(format!("{base}/projects/{p}/workflows/{hook}/runs")))
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let management = format!("{base}/projects/{p}/workflows/{hook}");
    let rotate = || host.post(format!("{management}/webhook/rotate"));
    let first_path = ok(rotate()).await["path"].as_str().unwrap().to_owned();
    let second_path = ok(rotate()).await["path"].as_str().unwrap().to_owned();
    assert_ne!(first_path, second_path);
    let signed_at = |path: &str, key: &str| {
        remote
            .post(format!("{base}{path}?access_token=hide&kind=issue"))
            .header("content-type", "application/json")
            .header("x-kar-signature", key)
            .body(body.clone())
    };
    assert_eq!(
        signed_at(&first_path, &tag(&body))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        signed_at(&second_path, &tag(b"wrong"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(ok(signed_at(&second_path, &tag(&body))).await, first);
    let logged = ok(host.get(format!("{management}/deliveries?accept_any_outcome=true"))).await;
    let attempts = logged["deliveries"].as_array().unwrap();
    assert_eq!(attempts.len(), 4);
    assert_eq!(attempts[0]["outcome"], "duplicate");
    assert_eq!(attempts[1]["outcome"], "rejected_signature");
    assert_eq!(attempts[1]["signature_verified"], false);
    let record = attempts[0]["id"].as_str().unwrap().to_owned();
    let detail = ok(host.get(format!(
        "{management}/deliveries/{record}?accept_any_outcome=true"
    )))
    .await;
    assert_eq!(
        detail["delivery"]["headers"]["x-kar-signature"],
        "[redacted]"
    );
    assert_eq!(
        detail["delivery"]["query"],
        "access_token=[redacted]&kind=issue"
    );
    assert_eq!(detail["delivery"]["body_bytes"], body.len());
    let visible = ok(host.get(format!("{management}/deliveries"))).await;
    assert_eq!(visible["deliveries"].as_array().unwrap().len(), 2);
    let other = save(
        &host,
        &base,
        p,
        json!({"type":"manual"}),
        json!([{"type":"ask","question":"another?"}]),
    )
    .await;
    assert_eq!(
        host.get(format!(
            "{base}/projects/{p}/workflows/{other}/deliveries/{record}?accept_any_outcome=true"
        ))
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_FOUND
    );
    child.kill().await.unwrap();
    (child, base) = server::serve(&state, &root, &native).await;
    let after = ok(host.get(format!(
        "{base}/projects/{p}/workflows/{hook}/deliveries/{record}?accept_any_outcome=true"
    )))
    .await;
    assert_eq!(after, detail);
    assert_eq!(fs::read(root.join("hooks")).unwrap(), b"H");
    let view = format!("{base}/projects/{p}/workflow-state");
    let listed = ok(host.get(&view)).await;
    let counts = ok(host.get(format!("{view}/counts"))).await;
    assert_eq!(listed["counts"], counts);
    assert!(counts["total"].as_u64().unwrap() >= 1);
    let mut feed = host
        .get(format!("{view}/events?schedules_only=true"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let frame = feed.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&frame).contains("event: snapshot"));
    drop(feed);
    let toggle = format!("{base}/projects/{p}/workflows/{other}/enabled");
    let disabled = ok(host.put(&toggle).json(&json!({"enabled":false}))).await;
    assert_eq!(disabled["enabled"], false);
    assert!(disabled["next_ms"].is_null());
    ok(host.put(&toggle).json(&json!({"enabled":true}))).await;
    let removed = ok(host.delete(format!("{base}/projects/{p}/workflows/{other}"))).await;
    assert_eq!(removed["id"], other.to_string());
    assert_eq!(
        host.put(&toggle)
            .json(&json!({"enabled":true}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let grouped=save(&host,&base,p,json!({"type":"webhook"}),json!([{"type":"agent","runtime":"codex","prompt":"Issue {{body.title}}; optional={{query.nope}}"}])).await;
    let manage = format!("/projects/{p}/workflows/{grouped}");
    let rules = json!({"signature":{"header":"x-issue-digest","encoding":"base64","prefix":"hMac "},
        "filters":[{"path":"body.labels[].name","op":"in","value":["kar","chat"]}],
        "replay_key":"{{headers.x-delivery}}","coalesce_key":"{{body.issue}}","coalesce_window_seconds":10,"render_agent_prompts":true});
    ok(host
        .put(format!("{base}{manage}/webhook/rules"))
        .json(&rules))
    .await;
    let path = ok(host.post(format!("{base}{manage}/webhook/rotate"))).await["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let post_issue = |url: &str, delivery: &str, label: &str, title: &str| {
        use base64::Engine as _;
        let data = serde_json::to_vec(&json!({"issue":42,"labels":[{"name":label}],"title":title}))
            .unwrap();
        let mut mac = Hmac::<sha2_mac::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(&data);
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        remote
            .post(format!("{url}{path}"))
            .header("x-issue-digest", format!("HMAC {signature}"))
            .header("x-delivery", delivery.to_owned())
            .body(data)
    };
    assert_eq!(
        ok(post_issue(&base, "a", "ignore", "unused")).await["outcome"],
        "filtered"
    );
    let first = ok(post_issue(&base, "a", "kar", "version one")).await;
    let grouped_id = first["run_id"].as_str().unwrap();
    assert_eq!(
        ok(post_issue(&base, "b", "chat", "@helper /go /next")).await,
        first
    );
    child.kill().await.unwrap();
    (child, base) = server::serve(&state, &root, &native).await;
    assert_eq!(ok(post_issue(&base, "a", "kar", "duplicate")).await, first);
    let finished = wait(&host, &base, p, grouped_id, "completed").await;
    let rendered = finished["steps"][0]["prompt"].as_str().unwrap();
    assert!(
        rendered.contains("<webhook-data-")
            && rendered.contains("@\u{2060}helper /\u{2060}go /\u{2060}next")
            && !rendered.contains("version one")
    );
    let records = ok(host.get(format!("{base}{manage}/deliveries?accept_any_outcome=true"))).await;
    let outcomes = records["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["outcome"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes, ["duplicate", "coalesced", "accepted", "filtered"]);
    let mid = records["deliveries"][1]["id"].as_str().unwrap();
    let entry = ok(host.get(format!(
        "{base}{manage}/deliveries/{mid}?accept_any_outcome=true"
    )))
    .await;
    assert_eq!(entry["delivery"]["headers"]["x-issue-digest"], "[redacted]");
    assert_eq!(entry["delivery"]["missing"], json!(["query.nope"]));
    assert_eq!(
        ok(host.get(format!("{base}{manage}/runs")))
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let def = save(
        &host,
        &base,
        p,
        json!({"type":"manual"}),
        json!([
            {"type":"script","command":"printf start >> first; printf hello"},
            {"type":"agent","runtime":"codex","model":null,"prompt":"Reply to the workflow input."},
            {"type":"ask","question":"Publish?"},
            {"type":"wait","duration_ms":4000},
            {"type":"script","command":"cat > context.json; printf end >> last"}
        ]),
    )
    .await;
    let id = start(&host, &base, p, def).await;
    let ask = wait(&host, &base, p, &id, "asking").await;
    assert_eq!(ask["cursor"], 2);
    assert_eq!(ask["receipts"][0]["result"]["stdout"]["text"], "hello");
    assert!(
        ask["receipts"][1]["result"]["text"]
            .as_str()
            .unwrap()
            .contains("haha")
    );
    let chat = ask["receipts"][1]["attempt"]["chat"].as_str().unwrap();
    let snap = ok(host.get(format!("{base}/projects/{p}/chats/{chat}/snapshot"))).await;
    assert!(
        snap.to_string()
            .contains(ask["receipts"][1]["attempt"]["id"].as_str().unwrap())
    );
    child.kill().await.unwrap();
    (child, base) = server::serve(&state, &root, &native).await;
    let same = wait(&host, &base, p, &id, "asking").await;
    assert_eq!(same["attempt"], ask["attempt"]);
    let answer = json!({"type":"answer","operation":Uuid::new_v4(),"attempt":ask["attempt"]["id"],"answer":"approved"});
    action(&host, &base, p, &id, &answer).await;
    let sleeping = wait(&host, &base, p, &id, "waiting").await;
    child.kill().await.unwrap();
    (child, base) = server::serve(&state, &root, &native).await;
    action(&host, &base, p, &id, &answer).await;
    let end = wait(&host, &base, p, &id, "completed").await;
    assert_eq!(end["receipts"].as_array().unwrap().len(), 5);
    assert_eq!(
        end["receipts"][3]["attempt"]["id"],
        sleeping["attempt"]["id"]
    );
    let context: Value =
        serde_json::from_slice(&fs::read(root.join("context.json")).unwrap()).unwrap();
    assert_eq!(context["receipts"][2]["result"], "approved");
    assert_eq!(fs::read(root.join("first")).unwrap(), b"start");
    assert_eq!(fs::read(root.join("last")).unwrap(), b"end");

    let fail = save(
        &host,
        &base,
        p,
        json!({"type":"manual"}),
        json!([{"type":"script","command":"printf R >> retries; test -e allow"}]),
    )
    .await;
    let rid = start(&host, &base, p, fail).await;
    let failed = wait(&host, &base, p, &rid, "failed").await;
    fs::write(root.join("allow"), []).unwrap();
    let retry = json!({"type":"retry","operation":Uuid::new_v4(),"attempt":failed["receipts"][0]["attempt"]["id"]});
    action(&host, &base, p, &rid, &retry).await;
    action(&host, &base, p, &rid, &retry).await;
    let retried = wait(&host, &base, p, &rid, "completed").await;
    assert_eq!(retried["receipts"].as_array().unwrap().len(), 2);
    assert_eq!(fs::read(root.join("retries")).unwrap(), b"RR");

    let uncertain=save(&host,&base,p,json!({"type":"manual"}),json!([{"type":"script","command":"printf U >> effects; while [ ! -f release ]; do sleep 0.05; done"},{"type":"ask","question":"Continue?"}])).await;
    let uid = start(&host, &base, p, uncertain).await;
    while !root.join("effects").exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.kill().await.unwrap();
    fs::write(root.join("release"), []).unwrap();
    (child, base) = server::serve(&state, &root, &native).await;
    let orphan = wait(&host, &base, p, &uid, "reconcile").await;
    let attempt = &orphan["attempt"]["id"];
    let url = format!("{base}/projects/{p}/workflow-runs/{uid}/actions");
    assert_eq!(
        host.post(url)
            .json(&json!({"type":"retry","operation":Uuid::new_v4(),"attempt":attempt}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let confirm = json!({"type":"reconcile","operation":Uuid::new_v4(),"attempt":attempt,"succeeded":true,"evidence":"checked effect file","result":"published"});
    action(&host, &base, p, &uid, &confirm).await;
    let next = wait(&host, &base, p, &uid, "asking").await;
    assert_eq!(next["cursor"], 1);
    assert_eq!(
        next["receipts"][0]["result"]["evidence"],
        "checked effect file"
    );
    action(
        &host,
        &base,
        p,
        &uid,
        &json!({"type":"cancel","operation":Uuid::new_v4()}),
    )
    .await;
    child.kill().await.unwrap();
    (child, base) = server::serve(&state, &root, &native).await;
    wait(&host, &base, p, &uid, "cancelled").await;
    assert_eq!(fs::read(root.join("effects")).unwrap(), b"U");
    child.kill().await.unwrap();
}

async fn source_scenarios() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let state = tmp.path().join("state");
    let native = tmp.path().join("native");
    let claude = tmp.path().join("claude");
    let scripts = claude.join("projects").join("fixture");
    for dir in [&root, &native, &scripts] {
        fs::create_dir_all(dir).unwrap();
    }
    let env = [("CLAUDE_CONFIG_DIR", claude.as_os_str())];
    let host = server::client();
    let (mut child, base) = server::serve_with_env(&state, &root, &native, &env).await;
    let project = ok(host
        .post(format!("{base}/projects"))
        .json(&json!({"root":root,"name":"sources"})))
    .await;
    let p = project["id"].as_str().unwrap();
    let list = format!("{base}/projects/{p}/workflow-definitions");
    let save = format!("{list}/review.kar.json");
    let id = Uuid::new_v4();
    let spec = |title: &str| {
        json!({"workflows":[{"id":id,"version":1,"name":title,"enabled":true,"schedule":{"type":"manual"},"steps":[{"type":"ask","question":"Proceed?"}]}]}).to_string()
    };
    let original = spec("review");
    let changed = ok(host.put(&save).json(&json!({"source":original}))).await;
    assert_eq!(changed["changed"], true);
    let refused = ok(host.put(&save).json(&json!({"source":"invalid JSON"}))).await;
    assert_eq!(refused["changed"], false);
    assert!(!refused["problems"].as_array().unwrap().is_empty());
    let duplicate = ok(host
        .put(format!("{list}/colliding.kar.json"))
        .json(&json!({"source":original})))
    .await;
    assert_eq!(duplicate["changed"], false);
    let listed = ok(host.get(&list)).await;
    assert_eq!(listed["files"][0]["source"], original);
    let file = std::path::Path::new(listed["directory"].as_str().unwrap()).join("review.kar.json");
    fs::write(&file, "{").unwrap();
    let invalid = ok(host.get(&list)).await;
    assert!(invalid["files"][0]["problem"].is_string());
    let run = start(&host, &base, p, id).await;
    let waiting = wait(&host, &base, p, &run, "asking").await;
    let retained = ok(host.delete(&save)).await;
    assert_eq!(retained["changed"], false);
    assert!(file.exists());
    child.kill().await.unwrap();
    let (mut child, base) = server::serve_with_env(&state, &root, &native, &env).await;
    let stable = wait(&host, &base, p, &run, "asking").await;
    assert_eq!(stable["attempt"], waiting["attempt"]);
    action(&host,&base,p,&run,&json!({"type":"answer","operation":Uuid::new_v4(),"attempt":stable["attempt"]["id"],"answer":"ok"})).await;
    wait(&host, &base, p, &run, "completed").await;
    fs::write(&file, spec("new review")).unwrap();
    let tasks = format!("{base}/projects/{p}/workflows");
    loop {
        let items = ok(host.get(&tasks)).await;
        if items[0]["name"] == "new review" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let removed = ok(host.delete(format!(
        "{base}/projects/{p}/workflow-definitions/review.kar.json"
    )))
    .await;
    assert_eq!(removed["changed"], true);
    assert!(!file.exists());
    let script = scripts.join("review.js");
    fs::write(&script, vec![b'x'; 256 * 1024 + 1]).unwrap();
    let script_url = format!("{base}/workflows/scripts");
    let preview = ok(host.get(&script_url).query(&[("path", &script)])).await;
    assert_eq!(preview["truncated"], true);
    assert_eq!(preview["contents"].as_str().unwrap().len(), 256 * 1024);
    let outside = root.join("other.js");
    fs::write(&outside, "outside").unwrap();
    assert!(
        !host
            .get(&script_url)
            .query(&[("path", &outside)])
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    #[cfg(unix)]
    {
        let link = scripts.join("link.js");
        std::os::unix::fs::symlink(outside, &link).unwrap();
        assert!(
            !host
                .get(&script_url)
                .query(&[("path", &link)])
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    child.kill().await.unwrap();
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("app-server") => codex_chat_peer::agent(),
        Some("--version") => println!("codex 0.169.0"),
        _ => tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(120), async {
                scheduling().await;
                http_scenarios().await;
                source_scenarios().await;
            })
            .await
            .unwrap();
        }),
    }
}
