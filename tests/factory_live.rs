#[path = "support/declaration.rs"]
mod declaration;

use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn query(home: &Path, work: &Path, bin: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(args)
        .arg("--bin")
        .arg(bin)
        .arg("--cwd")
        .arg(work)
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TMPDIR", home.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .env("DISABLE_TELEMETRY", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn has_session_file(dir: &Path) -> bool {
    dir.is_dir()
        && fs::read_dir(dir).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            path.is_file() || has_session_file(&path)
        })
}

#[test]
#[ignore = "requires the official Factory Droid CLI"]
fn model_and_skill_catalogs_before_session_creation() {
    let bin = fs::canonicalize(std::env::var_os("KAR_FACTORY_BIN").expect("set KAR_FACTORY_BIN"))
        .unwrap();
    let isolated = tempfile::tempdir().unwrap();
    let home = isolated.path().join("home");
    let work = isolated.path().join("work");
    fs::create_dir_all(home.join("tmp")).unwrap();
    fs::create_dir_all(home.join(".factory")).unwrap();
    let skill = work.join(".factory/skills/kar-catalog");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: kar-catalog\ndescription: Kar project fixture\n---\nPRIVATE_SKILL_CONTENT\n",
    )
    .unwrap();
    let settings = json!({"customModels":[{"model":"fixture","displayName":"CatalogFixture","baseUrl":"http://127.0.0.1:1/v1","apiKey":"PRIVATE_FIXTURE_KEY","provider":"generic-chat-completion-api","maxOutputTokens":4096}]});
    fs::write(home.join(".factory/settings.json"), settings.to_string()).unwrap();

    let models = query(&home, &work, &bin, &["models", "factory"]);
    let custom = models
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "custom:CatalogFixture-0")
        .expect("native custom model");
    assert_eq!(custom["displayName"], "CatalogFixture");
    assert_eq!(custom["isCustom"], true);
    assert!(custom["supportedReasoningEfforts"].is_array());
    assert!(!models.to_string().contains("PRIVATE_FIXTURE_KEY"));

    let report = query(&home, &work, &bin, &["inventory", "factory", "skills"]);
    assert_eq!(report["projectAvailable"], true);
    let entry = report["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "kar-catalog")
        .expect("workspace skill");
    assert_eq!(entry["location"], "project");
    assert_eq!(entry["description"], "Kar project fixture");
    assert!(!report.to_string().contains("PRIVATE_SKILL_CONTENT"));
    assert!(!has_session_file(&home.join(".factory/sessions")));
}

use axum::{Json, Router, extract::State, response::IntoResponse};
use kar::{
    Protocol, SessionOptions,
    chat::{ChatReplica, ChatStore, Item, Role, TurnStatus},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone)]
struct Model {
    requests: Arc<Mutex<Vec<Value>>>,
    file: PathBuf,
    held: Arc<tokio::sync::Notify>,
    release: tokio::sync::watch::Sender<bool>,
    tool_seen: tokio::sync::watch::Sender<bool>,
}
async fn completion(State(model): State<Model>, Json(body): Json<Value>) -> impl IntoResponse {
    let history = body["messages"].to_string();
    let main = body["tools"].as_array().is_some_and(|t| !t.is_empty());
    if main {
        model.requests.lock().unwrap().push(body.clone());
    }
    let mut reason = "stop";
    let mcp_prompt = body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    let delta = if main && mcp_prompt.contains("KAR_FACTORY_MCP") {
        if body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .is_some_and(|m| m["role"] == "user")
        {
            let (name, arguments) = if mcp_prompt.contains("KAR_FACTORY_MCP_HTTP") {
                ("host___echo", json!({"text":"first round"}))
            } else {
                ("local___list", json!({}))
            };
            reason = "tool_calls";
            json!({"tool_calls":[{"index":0,"id":"fixture-mcp-call","type":"function","function":{"name":name,"arguments":arguments.to_string()}}]})
        } else {
            json!({"content":"MCP round complete"})
        }
    } else if main && history.contains("FACTORY_QUESTION") {
        if body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
        {
            json!({"content":"question-finished"})
        } else {
            reason = "tool_calls";
            let questionnaire = "1. [question] Which colors? (multi)\n[topic] Palette\n[context] Choose accent colors.\n[option] Gold\n[option] Blue\n\n2. [question] Which shape?\n[topic] Shape\n[option] Round\n[option] Square";
            let args = json!({"questionnaire":questionnaire});
            json!({"tool_calls":[{"index":0,"id":"call-question","type":"function","function":{"name":"AskUser","arguments":args.to_string()}}]})
        }
    } else if main && history.contains("FACTORY_APPROVAL") {
        if body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
        {
            json!({"content":"approval-finished"})
        } else {
            reason = "tool_calls";
            let args = json!({"command":"echo kar-approval-marker","summary":"Print a fixture marker","riskLevel":"high","riskLevelReason":"Verify approval routing"});
            json!({"tool_calls":[{"index":0,"id":"call-approval","type":"function","function":{"name":"Execute","arguments":args.to_string()}}]})
        }
    } else if main && history.contains("FACTORY_HOLD") {
        let mut released = model.release.subscribe();
        model.held.notify_one();
        released.wait_for(|allow| *allow).await.unwrap();
        json!({"content":"held-answer"})
    } else if main && history.contains("FACTORY_SECOND") {
        json!({"content":"second-answer"})
    } else if main && history.contains("file-marker-kar") {
        let mut released = model.tool_seen.subscribe();
        released.wait_for(|seen| *seen).await.unwrap();
        json!({"content":"first-answer"})
    } else if main {
        reason = "tool_calls";
        json!({"tool_calls":[{"index":0,"id":"call-read-fixture","type":"function","function":{"name":"Read","arguments":json!({"file_path":model.file}).to_string()}}]})
    } else {
        json!({"content":"Fixture title"})
    };
    let mut payload = String::new();
    for (delta, finish) in [
        (json!({"role":"assistant"}), None),
        (delta, None),
        (json!({}), Some(reason)),
    ] {
        let chunk = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":5,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        payload.push_str(&format!("data: {chunk}\n\n"));
    }
    let usage = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":5,"model":"fixture","choices":[],"usage":{"prompt_tokens":22,"completion_tokens":7,"total_tokens":29}});
    payload.push_str(&format!("data: {usage}\n\n"));
    payload.push_str("data: [DONE]\n\n");
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        payload,
    )
}

struct Fixture {
    _isolated: tempfile::TempDir,
    opts: SessionOptions,
    runtime: kar::Runtime,
    store: ChatStore,
    model: Model,
}

async fn fixture() -> Fixture {
    let bin = fs::canonicalize(std::env::var_os("KAR_FACTORY_BIN").expect("set KAR_FACTORY_BIN"))
        .unwrap();
    let isolated = tempfile::tempdir().unwrap();
    let home = isolated.path().join("home");
    let work = isolated.path().join("work");
    fs::create_dir_all(home.join(".factory")).unwrap();
    fs::create_dir_all(home.join("tmp")).unwrap();
    fs::create_dir(&work).unwrap();
    let file = work.join("marker.txt");
    fs::write(&file, "file-marker-kar").unwrap();
    let model = Model {
        requests: Default::default(),
        file,
        held: Default::default(),
        release: tokio::sync::watch::channel(false).0,
        tool_seen: tokio::sync::watch::channel(false).0,
    };
    let router = Router::new()
        .route("/v1/chat/completions", axum::routing::post(completion))
        .with_state(model.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let configs: Vec<_> = ["Fixture", "Alternate"].iter().map(|name| json!({"model":"fixture","displayName":name,"baseUrl":format!("http://{addr}/v1"),"provider":"generic-chat-completion-api","apiKey":"kar-test-only","maxOutputTokens":4096})).collect();
    let config = json!({"customModels":configs});
    fs::write(home.join(".factory/settings.json"), config.to_string()).unwrap();
    let mut opts = SessionOptions::new(&work);
    for (k, v) in [
        ("HOME", home.clone()),
        ("USERPROFILE", home.clone()),
        ("TMPDIR", home.join("tmp")),
    ] {
        opts.env.insert(k.into(), v.into_os_string());
    }
    opts.env.insert(
        "FACTORY_HOME_OVERRIDE".into(),
        home.clone().into_os_string(),
    );
    opts.env.insert("DISABLE_TELEMETRY".into(), "1".into());
    opts.env.insert("FACTORY_API_KEY".into(), "".into());
    opts.env.insert("PATH".into(), "/usr/bin:/bin".into());
    opts.model = Some("custom:Fixture-0".into());
    opts.settings.effort = Some("none".into());
    let store = ChatStore::open(isolated.path().join("chats")).unwrap();
    let mut runtime = kar::runtime("factory").unwrap();
    runtime.program = bin;
    Fixture {
        _isolated: isolated,
        opts,
        runtime,
        store,
        model,
    }
}
#[tokio::test]
#[ignore = "requires the official Factory Droid CLI; uses a loopback model"]
async fn native_tool_feed_and_queued_resume() {
    let Fixture {
        _isolated,
        mut opts,
        runtime,
        store,
        model,
    } = fixture().await;
    opts.settings.append_system_prompt = Some("FACTORY_SAVED_RULE".into());
    let chat = store
        .create("factory", Some(&runtime.program), opts.clone())
        .await
        .unwrap();
    assert_eq!(chat.info().protocol, Protocol::Factory);
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    let mut first = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut second = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut events = store.follow(id, Some(first.cursor())).unwrap();
    let mut other = store.follow(id, Some(second.cursor())).unwrap();
    let first_id = Uuid::new_v4();
    let mut pre_receipt = None;
    {
        let send = chat.send(first_id, "FACTORY_FIRST");
        tokio::pin!(send);
        let turn = tokio::time::timeout(Duration::from_secs(40),async {
            loop {
                tokio::select! {
                    result = &mut send => break result.unwrap(),
                    next = events.recv() => {
                        first.apply(next.unwrap()).unwrap();
                        if pre_receipt.is_none() && first.turns().last().is_some_and(|t| t.id == first_id && !t.input_sent && t.native_turn_id.is_none()) {
                            pre_receipt=Some(first.cursor());
                        }
                        if !*model.tool_seen.borrow() {
                            second.apply(other.recv().await.unwrap()).unwrap();
                            if let Some(turn) = second.turns().last() && turn.items.iter().any(|i| matches!(i.item, Item::Tool { .. })) {
                                assert_eq!(turn.status,TurnStatus::Running);
                                assert_eq!(first.turns(),second.turns());
                                model.tool_seen.send_replace(true);
                            }
                        }
                    }
                }
            }
        }).await.unwrap();
        assert_eq!(turn.status, TurnStatus::Completed, "{:?}", turn.failure);
        assert_eq!(
            turn.native_turn_id.as_deref(),
            Some(first_id.to_string().as_str())
        );
        assert_eq!(turn.items.iter().filter(|i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text == "first-answer")).count(),1);
        assert_eq!(
            turn.items
                .iter()
                .filter(|i| matches!(&i.item, Item::Tool { .. }))
                .count(),
            1
        );
        assert!(
            serde_json::to_string(&turn.items)
                .unwrap()
                .contains("file-marker-kar")
        );
    }
    let after_tool = second.cursor();
    drop(other);
    drop(events);
    chat.close().await.unwrap();
    let queued = Uuid::new_v4();
    store.enqueue(id, queued, "FACTORY_SECOND").unwrap();
    let reopened = store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(reopened.info().native_id, native);
    tokio::time::timeout(Duration::from_secs(30), reopened.drain_queue())
        .await
        .unwrap()
        .unwrap();
    let duplicate = reopened.send(queued, "FACTORY_SECOND").await.unwrap();
    assert_eq!(duplicate.status, TurnStatus::Completed);
    reopened.close().await.unwrap();
    let snap = store.feed_snapshot(id).unwrap();
    let totals = snap.history.usage.last().unwrap().usage.total.unwrap();
    assert!(totals.input >= 22 && totals.output >= 7, "{totals:?}");
    let context = snap
        .history
        .context_usage
        .as_ref()
        .expect("native context at turn completion");
    assert!(context.tokens.is_some() && context.context_window.is_some());
    for view in [&mut first, &mut second] {
        let mut feed = store.follow(id, Some(view.cursor())).unwrap();
        while view.cursor() != snap.cursor {
            let item = feed.recv().await.unwrap();
            assert!(view.apply(item.clone()).unwrap());
            assert!(!view.apply(item).unwrap());
        }
        assert_eq!(view.turns(), snap.history.turns);
    }
    assert_eq!(first.turns().len(), 2);
    assert_eq!(
        first.turns()[1].native_turn_id.as_deref(),
        Some(queued.to_string().as_str())
    );
    {
        let reqs = model.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        let last = reqs.last().unwrap()["messages"].to_string();
        for part in [
            "FACTORY_FIRST",
            "first-answer",
            "FACTORY_SECOND",
            "FACTORY_SAVED_RULE",
        ] {
            assert!(last.contains(part), "{part}");
        }
    }
    let file = format!("{id}.karchat");
    let full = fs::read(_isolated.path().join("chats").join(&file)).unwrap();
    for (label, cut) in [
        ("before-receipt", pre_receipt.unwrap()),
        ("after-tool", after_tool),
    ] {
        let dir = _isolated.path().join(label);
        let partial = ChatStore::open(&dir).unwrap();
        let offset: usize = cut.to_string().split_once(':').unwrap().1.parse().unwrap();
        fs::write(dir.join(&file), &full[..offset]).unwrap();
        fs::File::create(dir.join(format!("{id}.lock"))).unwrap();
        let recovered = partial
            .resume_with(id, |o| o.env = opts.env.clone())
            .await
            .unwrap();
        let turn = recovered.send(first_id, "FACTORY_FIRST").await.unwrap();
        assert_eq!(
            turn.status,
            TurnStatus::Completed,
            "{label}: {:?}",
            turn.items
        );
        assert!(turn.input_sent);
        assert_eq!(
            turn.items
                .iter()
                .filter(|i| matches!(&i.item, Item::Tool { .. }))
                .count(),
            1
        );
        assert_eq!(turn.items.iter().filter(|i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="first-answer")).count(),1);
        assert!(
            serde_json::to_string(&turn.items)
                .unwrap()
                .contains("file-marker-kar")
        );
        assert!(
            !turn
                .items
                .iter()
                .any(|i| matches!(&i.item,Item::Message{text,..} if text=="second-answer"))
        );
        assert_eq!(partial.history(id).unwrap().turns.len(), 1);
        recovered.close().await.unwrap();
        assert_eq!(model.requests.lock().unwrap().len(), 3);
    }
}

#[tokio::test]
#[ignore = "requires the official Factory Droid CLI; uses a loopback model"]
async fn native_control_receipts_and_settings() {
    use kar::{CompactionStatus, Event, Outcome};
    let f = fixture().await;
    let mut unknown = f.opts.clone();
    unknown.model = Some("model-not-in-this-droid".into());
    let failure = f
        .runtime
        .open(unknown)
        .await
        .err()
        .expect("Droid accepted an unconfirmed model");
    assert!(
        matches!(&failure, kar::Error::Open(e) if matches!(e.as_ref(), kar::OpenError::ModelConfirmation{requested,reported,catalog_selector_ids:None,..} if requested=="model-not-in-this-droid" && reported.as_deref()!=Some(requested))),
        "{failure}"
    );
    let session = f.runtime.open(f.opts.clone()).await.unwrap();
    assert_eq!(session.model().as_deref(), Some("custom:Fixture-0"));
    assert_eq!(session.effort().as_deref(), Some("none"));
    assert!(session.capabilities().manual_compaction);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let reading = session.resources().await.expect("owned Droid process");
        assert!(reading.rss > 0 && reading.processes >= 1);
        assert_eq!(reading.scope, None);
    }
    let native = session.id().to_owned();
    let refused = session
        .start_compaction()
        .await
        .unwrap()
        .wait(Some(Duration::from_secs(15)))
        .await;
    assert!(
        matches!(refused.status, CompactionStatus::Rejected { .. }),
        "{:?}",
        refused.status
    );

    let held = session.prompt("FACTORY_HOLD").await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), f.model.held.notified())
        .await
        .unwrap();
    session.steer("FACTORY_DISCARD").await.unwrap();
    session.abort().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(15), held.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Outcome::Aborted);
    let file = session.native_session_file().await.unwrap().unwrap();
    assert_eq!(file.file_stem().unwrap().to_str(), Some(session.id()));
    f.model.release.send_replace(true);
    assert!(
        session
            .records()
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e, Event::InputDropped { .. }))
    );
    session.close().await.unwrap();

    assert_eq!(session.resources().await, None);
    let mut opts = f.opts.clone();
    opts.resume = Some(native.clone());
    opts.cwd = f._isolated.path().to_owned();
    assert!(matches!(
        f.runtime.open(opts).await,
        Err(kar::Error::Open(e)) if matches!(*e, kar::OpenError::ResumeCwd {reason:kar::ResumeCwdReason::OtherCwd {..},..})
    ));
    let mut missing = f.opts.clone();
    let id = uuid::Uuid::new_v4().to_string();
    missing.resume = Some(id.clone());
    let e = f
        .runtime
        .open(missing)
        .await
        .err()
        .expect("missing session opened");
    assert!(
        matches!(&e, kar::Error::Open(e) if matches!(e.as_ref(),kar::OpenError::SessionNotFound {session_id,cause,..} if session_id==&id && cause.method=="droid.load_session")),
        "{e}"
    );
    let mut changed = f.opts.clone();
    changed.resume = Some(native.clone());
    changed.model = Some("custom:Alternate-1".into());
    let changed = f.runtime.open(changed).await.unwrap();
    assert_eq!(changed.id(), native);
    assert_eq!(changed.model().as_deref(), Some("custom:Alternate-1"));
    changed.close().await.unwrap();

    let mut opts = f.opts.clone();
    opts.settings.effort = Some("high".into());
    assert!(matches!(
        f.runtime.open(opts).await,
        Err(kar::Error::Refused(_))
    ));

    let chat = f
        .store
        .create("factory", Some(&f.runtime.program), f.opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    for _ in 0..2 {
        let turn = chat.send(Uuid::new_v4(), "FACTORY_SECOND").await.unwrap();
        assert_eq!(turn.status, TurnStatus::Completed);
    }
    let report = chat
        .start_compaction()
        .await
        .unwrap()
        .wait(Some(Duration::from_secs(35)))
        .await;
    assert!(
        matches!(report.status, CompactionStatus::Completed { .. }),
        "{:?}",
        report.status
    );
    assert!(report.response.is_some());
    chat.close().await.unwrap();
    let snap = f.store.feed_snapshot(id).unwrap();
    assert_eq!(snap.history.manual_compaction, Some(report));
    assert!(
        snap.history
            .last_compaction
            .as_ref()
            .unwrap()
            .compaction_id
            .is_some()
    );
}

#[tokio::test]
#[ignore = "requires the official Factory Droid CLI; uses a loopback model"]
async fn native_tool_approval_reaches_both_clients() {
    let mut f = fixture().await;
    f.opts.approvals = kar::ApprovalMode::Host;
    let chat = f
        .store
        .create("factory", Some(&f.runtime.program), f.opts)
        .await
        .unwrap();
    let id = chat.info().id;
    let mut view = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
    let mut feed = f.store.follow(id, Some(view.cursor())).unwrap();
    let mut answered = None;
    {
        let send = chat.send(Uuid::new_v4(), "FACTORY_APPROVAL");
        tokio::pin!(send);
        let turn = tokio::time::timeout(Duration::from_secs(30),async {
            loop {
                tokio::select!{
                    result = &mut send => break result.unwrap(),
                    e = feed.recv() => {
                        view.apply(e.unwrap()).unwrap();
                        if answered.is_none() && let Some(req) = view.pending_requests().first() {
                            assert_eq!(req.method,"droid.request_permission");
                            assert!(req.decisions().contains(&kar::PermissionDecision::AllowOnce));
                            let second = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
                            assert_eq!(second.pending_requests(),view.pending_requests());
                            chat.decide(req, kar::PermissionDecision::AllowOnce).await.unwrap();
                            answered=Some(req.clone());
                        }
                    }
                }
            }
        }).await.unwrap();
        assert_eq!(turn.status, TurnStatus::Completed);
        assert!(turn.items.iter().any(|i| matches!(&i.item,Item::Tool{activity,..} if activity.result.as_ref().and_then(|r| r.text()).is_some_and(|s| s.contains("kar-approval-marker")))));
    }
    assert!(
        chat.decide(
            answered.as_ref().unwrap(),
            kar::PermissionDecision::AllowOnce
        )
        .await
        .is_err()
    );
    assert!(
        f.store
            .feed_snapshot(id)
            .unwrap()
            .history
            .pending_requests
            .is_empty()
    );
    chat.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the official Factory Droid CLI; uses a loopback model"]
async fn concurrent_question_answers_reconcile_on_reconnect() {
    let mut f = fixture().await;
    f.opts.approvals = kar::ApprovalMode::Host;
    let chat = f
        .store
        .create("factory", Some(&f.runtime.program), f.opts)
        .await
        .unwrap();
    let id = chat.info().id;
    let mut a = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
    let mut b = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
    let mut feed = f.store.follow(id, Some(a.cursor())).unwrap();
    let send = chat.send(Uuid::new_v4(), "FACTORY_QUESTION");
    let respond = async {
        while a.pending_requests().is_empty() {
            a.apply(feed.recv().await.unwrap()).unwrap();
        }
        let req = a.pending_requests()[0].clone();
        assert_eq!(req.method, "droid.ask_user");
        let fields = req.question_form().unwrap().fields;
        assert_eq!(fields[0].id, "1");
        assert_eq!(
            fields[0].description.as_deref(),
            Some("Choose accent colors.")
        );
        assert_eq!(fields[0].kind, kar::FieldKind::MultiSelect);
        assert_eq!(fields[1].id, "2");
        assert!(fields[1].allow_other);
        // The second client catches up from its older cursor, then races the first.
        let mut second = f.store.follow(id, Some(b.cursor())).unwrap();
        while b.pending_requests().is_empty() {
            b.apply(second.recv().await.unwrap()).unwrap();
        }
        assert_eq!(b.pending_requests(), a.pending_requests());
        let reply = kar::QuestionReply::Submit {
            values: json!({"1":["Blue","Gold"],"2":"Custom"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let invalid = kar::QuestionReply::Submit {
            values: json!({"1":["Blue","Blue"],"2":"Custom"})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(chat.answer_question(&req, invalid).await.is_err());
        let (one, two) = tokio::join!(
            chat.answer_question(&req, reply.clone()),
            chat.answer_question(&b.pending_requests()[0], reply)
        );
        assert_ne!(one.is_ok(), two.is_ok());
    };
    let (turn, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(send, respond)
    })
    .await
    .unwrap();
    assert_eq!(turn.unwrap().status, TurnStatus::Completed);
    chat.close().await.unwrap();
    let end = f.store.feed_snapshot(id).unwrap();
    for replica in [&mut a, &mut b] {
        let mut catchup = f.store.follow(id, Some(replica.cursor())).unwrap();
        while replica.cursor() != end.cursor {
            let event = catchup.recv().await.unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert!(replica.pending_requests().is_empty());
        assert_eq!(replica.turns(), end.history.turns);
    }
    let requests = f.model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let output = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert_eq!(
        output,
        "1. [question] Which colors?\n[answer] Blue, Gold\n\n2. [question] Which shape?\n[answer] Custom"
    );
}

#[tokio::test]
#[ignore = "requires KAR_FACTORY_BIN; a loopback model calls Rust HTTP and stdio MCP services"]
async fn native_mcp_transports_on_create_and_resume() {
    use kar::host_tools::HostTool;
    let mut f = fixture().await;
    let mut tool = HostTool::new(
        "echo",
        json!({"type":"object","properties":{"text":{"type":"string"}}})
            .as_object()
            .unwrap()
            .clone(),
        |args, _| async move {
            let text = args["text"].as_str().ok_or("missing text")?;
            Ok(format!("factory-host-receipt {text}").into())
        },
    );
    tool.description = Some("Return a test receipt".into());
    let host = kar::serve_mcp_tools("host", vec![tool], None)
        .await
        .unwrap();
    let stdio = kar::McpServer::Stdio {
        command: env!("CARGO_BIN_EXE_kar").into(),
        args: vec!["mcp".into()],
        env: Default::default(),
    };
    f.opts
        .mcp_servers
        .extend([host.entry(), ("local".into(), stdio)]);
    f.opts.approvals = kar::ApprovalMode::Host;
    f.opts.remove_env("KAR_FACTORY_BIN").unwrap();
    let path = std::env::join_paths([
        f.runtime.program.parent().unwrap(),
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .unwrap();
    f.opts.env.insert("PATH".into(), path);
    let declared = declaration::for_installation(&f.runtime, &f.opts).await;
    let opened = f
        .store
        .create("factory", None, f.opts.clone())
        .await
        .unwrap();
    declaration::check(&declared, opened.capabilities());
    assert_eq!(
        opened.capabilities().mcp_transports,
        Some([kar::McpTransport::Stdio, kar::McpTransport::Http].as_slice())
    );
    let id = opened.info().id;
    let mut current = Some(opened);
    for (prompt, receipt) in [
        ("KAR_FACTORY_MCP_HTTP", "factory-host-receipt first round"),
        ("KAR_FACTORY_MCP_STDIO", "unread"),
    ] {
        let chat = match current.take() {
            Some(chat) => chat,
            None => f
                .store
                .resume_with(id, |o| {
                    o.env.clone_from(&f.opts.env);
                    o.mcp_servers.clone_from(&f.opts.mcp_servers);
                })
                .await
                .unwrap(),
        };
        let baseline = f.model.requests.lock().unwrap().len();
        let mut replica = ChatReplica::from_snapshot(f.store.feed_snapshot(id).unwrap());
        let mut feed = f.store.follow(id, Some(replica.cursor())).unwrap();
        let turn = {
            let send = chat.send(Uuid::new_v4(), prompt);
            tokio::pin!(send);
            let mut answered = std::collections::HashSet::new();
            tokio::time::timeout(Duration::from_secs(40), async {
                loop {
                    tokio::select! {
                        turn = &mut send => break turn.unwrap(),
                        event = feed.recv() => {
                            replica.apply(event.unwrap()).unwrap();
                            for request in replica.pending_requests() {
                                if answered.insert((request.cursor.stream_id, request.cursor.after_seq)) {
                                    chat.decide(request, kar::PermissionDecision::AllowOnce).await.unwrap();
                                }
                            }
                        }
                    }
                }
            }).await.unwrap()
        };
        assert_eq!(turn.status, TurnStatus::Completed, "{:?}", turn.items);
        assert!(
            f.model.requests.lock().unwrap()[baseline..]
                .iter()
                .any(|r| r["messages"].as_array().is_some_and(|m| m
                    .iter()
                    .any(|m| m["role"] == "tool" && m["content"].to_string().contains(receipt)))),
            "missing MCP tool result for {prompt}: {:?}",
            turn.items
        );
        chat.close().await.unwrap();
    }
    host.close().await;
}
