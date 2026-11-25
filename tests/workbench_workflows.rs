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
        workflows::{Action, Clock, Definition, Schedule, Status, Step, Trigger},
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
            timeout_ms: None,
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
        (
            within,
            Schedule::FixedTime {
                time: "9:01".into(),
                weekdays: vec![3, 1],
                zone: Some("UTC".into()),
            },
            t + 120_000,
        ),
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
    // An equivalent edit after restart still owes the one occurrence inside the grace period.
    let equivalent = Schedule::FixedTime {
        time: "09:01".into(),
        weekdays: vec![1, 3, 1],
        zone: Some("UTC".into()),
    };
    assert_eq!(
        app.save_workflow(project, within, definition(equivalent), None)
            .unwrap()
            .next_ms,
        Some(t + 120_000)
    );
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
                || run["status"] == "stopped"
                || run["status"] == "skipped"
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

async fn nested_and_webhook_backlog_scenarios() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("store");
    let t0 = ms("2026-02-10T12:00:00Z");
    let clock = Arc::new(ManualClock(AtomicI64::new(t0)));
    let app = Workbench::open_with_workflow_clock(
        ChatStore::open(&db).unwrap(),
        std::slice::from_ref(&root),
        clock.clone(),
    )
    .unwrap();
    let project = app.register(&root, "nested").unwrap().id;

    // Self-recursive Step::Workflow must be rejected at definition validation time.
    let self_wf = Uuid::new_v4();
    assert!(
        app.save_workflow(
            project,
            self_wf,
            Definition {
                name: "self-loop".into(),
                enabled: true,
                schedule: Schedule::Manual,
                steps: vec![Step::Workflow {
                    workflow: self_wf,
                    input: None,
                }],
            },
            None,
        )
        .is_err()
    );

    // Parent invoking child workflow + child question timeout + parent completion.
    let child_wf = Uuid::new_v4();
    let parent_wf = Uuid::new_v4();
    app.save_workflow(
        project,
        child_wf,
        Definition {
            name: "child-flow".into(),
            enabled: true,
            schedule: Schedule::Manual,
            steps: vec![Step::Ask {
                question: "Approve child?".into(),
                timeout_ms: Some(5_000),
            }],
        },
        None,
    )
    .unwrap();
    app.save_workflow(
        project,
        parent_wf,
        Definition {
            name: "parent-flow".into(),
            enabled: true,
            schedule: Schedule::Manual,
            steps: vec![Step::Workflow {
                workflow: child_wf,
                input: Some(json!({"stage": "sub"})),
            }],
        },
        None,
    )
    .unwrap();

    let parent_run = app
        .trigger_workflow(project, parent_wf, Uuid::new_v4(), json!({"top": 1}))
        .unwrap();
    assert_eq!(parent_run.parent_run_id, None);
    app.tick_workflows().unwrap();
    let waiting_parent = app.workflow_run(project, parent_run.id).unwrap();
    assert_eq!(waiting_parent.status, Status::Waiting);
    let parent_attempt = waiting_parent.attempt.as_ref().unwrap().id;
    let child_runs = app.workflow_runs(project, child_wf).unwrap();
    assert_eq!(child_runs.len(), 1);
    let child_id = child_runs[0].id;
    assert_eq!(child_runs[0].parent_run_id, Some(parent_run.id));
    assert_eq!(child_runs[0].parent_attempt_id, Some(parent_attempt));
    assert_eq!(
        child_runs[0].trigger,
        Trigger::Child {
            parent_run: parent_run.id,
            attempt: parent_attempt,
        }
    );
    assert_eq!(child_runs[0].input, json!({"stage": "sub"}));

    // Tick child into Asking, then advance clock past timeout_ms (5_000ms) so it expires and completes both.
    app.tick_workflows().unwrap();
    assert_eq!(
        app.workflow_run(project, child_id).unwrap().status,
        Status::Asking
    );
    clock.0.store(t0 + 5_000, Ordering::SeqCst);
    app.tick_workflows().unwrap();
    let done_child = app.workflow_run(project, child_id).unwrap();
    assert_eq!(done_child.status, Status::Completed);
    assert_eq!(done_child.receipts[0].result["expired"], true);
    let done_parent = app.workflow_run(project, parent_run.id).unwrap();
    assert_eq!(done_parent.status, Status::Completed);
    assert_eq!(done_parent.receipts[0].result["run_id"], json!(child_id));
    assert_eq!(done_parent.receipts[0].result["outcome"], "completed");

    // Stopping a waiting parent recursively stops its active child and preserves the reason.
    let p2 = app
        .trigger_workflow(project, parent_wf, Uuid::new_v4(), Value::Null)
        .unwrap();
    app.tick_workflows().unwrap();
    app.tick_workflows().unwrap();
    let c2 = app
        .workflow_runs(project, child_wf)
        .unwrap()
        .into_iter()
        .find(|r| r.parent_run_id == Some(p2.id))
        .unwrap();
    assert_eq!(c2.status, Status::Asking);
    let stopped_parent = app
        .workflow_action(
            project,
            p2.id,
            Uuid::new_v4(),
            Action::Stop {
                reason: Some("maintenance window".into()),
            },
        )
        .unwrap();
    assert_eq!(stopped_parent.status, Status::Stopped);
    assert_eq!(
        stopped_parent.detail.as_deref(),
        Some("maintenance window")
    );
    let stopped_child = app.workflow_run(project, c2.id).unwrap();
    assert_eq!(stopped_child.status, Status::Stopped);
    assert_eq!(stopped_child.detail.as_deref(), Some("maintenance window"));

    // Webhook max_delivery_age_minutes, 429 rate-limit, and Bucket 0/1/2 receipt retention.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router("kar-workbench-fixture".into()).unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{addr}");
    let host = server::client();
    let remote = Client::new();

    let hook = save(
        &host,
        &base,
        &project.to_string(),
        json!({"type": "webhook"}),
        json!([{"type": "skip", "reason": "fast"}]),
    )
    .await;
    let manage = format!("{base}/projects/{project}/workflows/{hook}");
    ok(host.put(format!("{manage}/webhook/rules")).json(&json!({
        "max_delivery_age_minutes": 5,
        "filters": [{"path": "body.kind", "op": "equals", "value": "push"}]
    })))
    .await;
    let hook_url = format!("{base}/hooks/workflows/{project}/{hook}");
    let signed_post = |id: String, kind: &str, extra_hdr: Option<(&str, String)>| {
        let payload = serde_json::to_vec(&json!({"id": id, "input": {}, "kind": kind})).unwrap();
        let sig = tag(&payload);
        let mut req = remote
            .post(&hook_url)
            .header("content-type", "application/json")
            .header("x-kar-signature", sig);
        if let Some((k, v)) = extra_hdr {
            req = req.header(k, v);
        }
        req.body(payload)
    };

    let now_ms = clock.0.load(Ordering::SeqCst);
    // 1) Stale relay-queued & upstream webhook headers -> 410 Gone + "expired" receipt in Bucket 0.
    for (idx, (hdr_name, hdr_val)) in [
        ("x-kar-relay-queued-at", (now_ms - 6 * 60_000).to_string()),
        ("svix-timestamp", ((now_ms - 10 * 60_000) / 1000).to_string()),
        (
            "x-webhook-timestamp",
            jiff::Timestamp::from_millisecond(now_ms - 7 * 60_000)
                .unwrap()
                .to_string(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let resp = signed_post(format!("stale-{idx}"), "push", Some((hdr_name, hdr_val)))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::GONE);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["outcome"], "expired");
    }
    let visible_expired = ok(host.get(format!("{manage}/deliveries"))).await;
    let exp_rows = visible_expired["deliveries"].as_array().unwrap();
    assert_eq!(exp_rows.len(), 3);
    assert!(exp_rows.iter().all(|r| r["outcome"] == "expired"));
    let relay_rec_id = exp_rows[2]["id"].as_str().unwrap();
    let relay_detail = ok(host.get(format!("{manage}/deliveries/{relay_rec_id}"))).await;
    assert_eq!(
        relay_detail["delivery"]["headers"]["x-kar-relay-queued-at"],
        "[redacted]"
    );

    // 2) Bucket 2 ("filtered"): 15 misses in one minute -> capped at 10 receipts in that minute.
    for i in 0..15 {
        let res = ok(signed_post(format!("filt-m1-{i}"), "ignore", None)).await;
        assert_eq!(res["outcome"], "filtered");
    }

    // 3) Fresh & future-skewed timestamps succeed, up to 60 accepted in a minute; 60 + 3 = 63 Bucket 0 -> capped at 50!
    for i in 0..60 {
        let stamp = if i == 0 {
            Some(("x-kar-relay-queued-at", (now_ms - 60_000).to_string()))
        } else if i == 1 {
            Some(("x-webhook-timestamp", (now_ms + 30_000).to_string()))
        } else {
            None
        };
        let res = ok(signed_post(format!("acc-{i}"), "push", stamp)).await;
        assert!(res["run_id"].is_string());
    }

    // 4) 61st..72nd valid deliveries in the same minute hit 429 rate_limited; Bucket 1 caps at 10/min!
    for i in 0..12 {
        let resp = signed_post(format!("rl-{i}"), "push", None)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["outcome"], "rate_limited");
    }

    let all_m1 = ok(host.get(format!("{manage}/deliveries?accept_any_outcome=true"))).await;
    let rows_m1 = all_m1["deliveries"].as_array().unwrap();
    let count_kind = |rows: &[Value], pred: fn(&str) -> bool| {
        rows.iter()
            .filter(|r| pred(r["outcome"].as_str().unwrap()))
            .count()
    };
    assert_eq!(
        count_kind(rows_m1, |o| matches!(o, "accepted" | "expired")),
        50
    );
    assert_eq!(count_kind(rows_m1, |o| o == "rate_limited"), 10);
    assert_eq!(count_kind(rows_m1, |o| o == "filtered"), 10);

    // 5) Advance past 60s twice and verify Bucket 2 retention caps at 20 without evicting Bucket 0 or Bucket 1.
    for (minute, offset) in [(2, 70_000), (3, 140_000)] {
        clock.0.store(now_ms + offset, Ordering::SeqCst);
        for i in 0..12 {
            let res = ok(signed_post(format!("filt-m{minute}-{i}"), "ignore", None)).await;
            assert_eq!(res["outcome"], "filtered");
        }
    }
    let all_m3 = ok(host.get(format!("{manage}/deliveries?accept_any_outcome=true"))).await;
    let rows_m3 = all_m3["deliveries"].as_array().unwrap();
    assert_eq!(
        count_kind(rows_m3, |o| matches!(o, "accepted" | "expired")),
        50
    );
    assert_eq!(count_kind(rows_m3, |o| o == "rate_limited"), 10);
    assert_eq!(count_kind(rows_m3, |o| o == "filtered"), 20);

    srv.abort();
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("app-server") => codex_chat_peer::agent(),
        Some("--version") => println!("codex 0.169.0"),
        _ => tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(120), async {
                scheduling().await;
                nested_and_webhook_backlog_scenarios().await;
                http_scenarios().await;
                source_scenarios().await;
            })
            .await
            .unwrap();
        }),
    }
}
