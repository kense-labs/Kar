#[path = "dsh_live/cli_routing.rs"]
mod cli_routing;
#[path = "support/declaration.rs"]
mod declaration;
#[path = "dsh_live/prompts.rs"]
mod prompts;
#[cfg(unix)]
#[path = "dsh_live/queue_crash.rs"]
mod queue_crash;
#[path = "dsh_live/titles.rs"]
mod titles;

use axum::{
    Json, Router,
    extract::State,
    response::{IntoResponse, Response},
};
use kar::{
    Protocol, SessionOptions,
    chat::{ChatReplica, ChatStore, Item, Role, TurnStatus},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;
#[derive(Clone, Default)]
struct Model {
    requests: Arc<Mutex<Vec<Value>>>,
    prompt_sessions: Arc<Mutex<Vec<(String, String)>>>,
    upload_requests: Arc<Mutex<Vec<(String, bool)>>>,
    child_requested: Arc<std::sync::atomic::AtomicBool>,
    policy_called: Arc<std::sync::atomic::AtomicBool>,
    mcp_called: Arc<std::sync::atomic::AtomicBool>,
    directory_tools: Arc<Mutex<std::collections::VecDeque<(&'static str, Value)>>>,
    repeated_calls: Arc<std::sync::atomic::AtomicUsize>,
    seen: Arc<tokio::sync::Notify>,
    held: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    file: PathBuf,
    goal_release: tokio::sync::watch::Sender<bool>,
    hold_model_reply: tokio::sync::watch::Sender<bool>,
    compact_entered: Arc<tokio::sync::Notify>,
}
async fn models() -> Json<Value> {
    Json(json!({"data":[{"id":"deepseek-flash","name":"DeepSeek-V4.1-Flash"}]}))
}
async fn message(
    State(model): State<Model>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if body.to_string().contains("KAR_PUBLIC_READBACK_FIXTURE") {
        return completion("confirmed", None);
    }
    let messages = body["messages"].to_string();
    if messages.contains("KAR_QUEUE_CRASH") {
        let held = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|m| m["content"].to_string().contains("KAR_QUEUE_CRASH first"));
        model.requests.lock().unwrap().push(body);
        if held {
            let mut gate = model.hold_model_reply.subscribe();
            gate.wait_for(|hold| !*hold).await.unwrap();
        }
        return completion("queue reply", None);
    }
    if messages.contains("DSH_PROMPT_") {
        let session = headers
            .get("x-deepseek-harness-session-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        model
            .prompt_sessions
            .lock()
            .unwrap()
            .push((session.to_owned(), prompts::system(&body)));
        model.requests.lock().unwrap().push(body);
        let delegate = messages.contains("DSH_PROMPT_PARENT")
            && !model
                .child_requested
                .swap(true, std::sync::atomic::Ordering::AcqRel);
        return completion("scoped prompt reply", delegate.then(|| ("subagent",
            json!({"prompt":"DSH_PROMPT_CHILD reply briefly","description":"verify child persona"}))));
    }
    if messages.contains("DSH_WRITABLE") {
        let tool = model.directory_tools.lock().unwrap().pop_front();
        model.requests.lock().unwrap().push(body);
        return completion("directory fixture reply", tool);
    }
    if messages.contains("DSH_POLICY") {
        model.requests.lock().unwrap().push(body);
        let probe = messages.contains("DSH_POLICY_PROBE")
            && !model
                .policy_called
                .swap(true, std::sync::atomic::Ordering::AcqRel);
        return completion("policy fixture reply", probe.then(|| ("bash", json!({"command":"printf ran > policy-tool-ran","description":"check scoped tool execution"}))));
    }
    if messages.contains("DSH_REPEAT_TOOLS") {
        let index = model
            .repeated_calls
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let tool = ["first", "second"].get(index).map(|value| {
            let input = json!({
                "command": format!("printf {value}"),
                "description": format!("print {value}"),
            });
            ("bash", input)
        });
        return completion("two tools finished", tool);
    }
    if messages.contains("DSH_MCP_") {
        let call = messages.contains("DSH_MCP_CALL")
            && !model
                .mcp_called
                .swap(true, std::sync::atomic::Ordering::AcqRel);
        model.requests.lock().unwrap().push(body);
        return completion(
            "MCP fixture reply",
            call.then(|| ("mcp__session__list", json!({}))),
        );
    }
    if messages.contains("DSH_LOG_") {
        let id = headers
            .get("x-deepseek-harness-session-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        model
            .upload_requests
            .lock()
            .unwrap()
            .push((id, body.get("dsh_session_log").is_some()));
        let delegate = messages.contains("DSH_LOG_ROOT")
            && !model
                .child_requested
                .swap(true, std::sync::atomic::Ordering::AcqRel);
        return completion(
            "log fixture reply",
            if delegate {
                Some((
                    "subagent",
                    json!({"prompt":"DSH_LOG_CHILD reply child-ok","description":"native child upload check"}),
                ))
            } else {
                None
            },
        );
    }
    let later = messages.contains("DSH_SECOND");
    let returning_tool = messages.contains("tool_result");
    let background = messages.contains("DSH_BACKGROUND");
    let branch = messages.contains("DSH_BRANCH");
    let keep = messages.contains("DSH_KEEP");
    let hold = messages.contains("DSH_HOLD");
    let goal = messages.contains("DSH_GOAL");
    if goal {
        let mut release = model.goal_release.subscribe();
        release.wait_for(|allow| *allow).await.unwrap();
    }
    let compact_fixture = messages.contains("DSH_COMPACT");
    if *model.hold_model_reply.borrow() && compact_fixture {
        let mut waiting = model.hold_model_reply.subscribe();
        model.compact_entered.notify_one();
        waiting.wait_for(|hold| !*hold).await.unwrap();
    }
    let tool = !compact_fixture && !goal && !later && !branch && !hold && !returning_tool;
    if hold && !keep {
        model.held.notify_one();
        model.release.notified().await;
    }
    if later {
        std::fs::write(&model.file, "second").unwrap();
    }
    model.requests.lock().unwrap().push(body);
    if returning_tool && !later && !branch && !background && !goal {
        model.seen.notified().await;
    }
    let text = if compact_fixture {
        "summary of the test"
    } else if goal {
        "native goal round answer"
    } else if keep {
        "queued answer"
    } else if hold {
        "held answer"
    } else if branch {
        "branch answer"
    } else if later {
        "dsh-second-answer"
    } else {
        "dsh-first-answer"
    };
    let arguments = if background {
        json!({"command":"sleep 30","description":"wait for the stop request","run_in_background":true})
    } else {
        json!({"command":"printf dsh-tool-output","description":"print a test marker"})
    };
    completion(text, tool.then_some(("bash", arguments)))
}

fn completion(text: &str, tool: Option<(&str, Value)>) -> Response {
    let start = if let Some((name, _)) = &tool {
        json!({"type":"tool_use","id":"dsh-tool","name":name,"input":{}})
    } else {
        json!({"type":"text","text":""})
    };
    let delta = if let Some((_, arguments)) = &tool {
        json!({"type":"input_json_delta","partial_json":arguments.to_string()})
    } else {
        json!({"type":"text_delta","text":text})
    };
    let frames = [
        json!({"type":"message_start","message":{"id":Uuid::new_v4(),"type":"message","role":"assistant","model":"deepseek-flash","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":start}),
        json!({"type":"content_block_delta","index":0,"delta":delta}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":if tool.is_some(){"tool_use"}else{"end_turn"},"stop_sequence":null},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ];
    let mut s = String::new();
    for f in frames {
        s.push_str(&format!(
            "event: {}\ndata: {f}\n\n",
            f["type"].as_str().unwrap()
        ));
    }
    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], s).into_response()
}
struct Testbed {
    dir: tempfile::TempDir,
    opts: SessionOptions,
    runtime: kar::Runtime,
    store: ChatStore,
    data: Model,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Testbed {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn testbed(protocol: Protocol) -> Testbed {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let marker = work.join("tracked.txt");
    std::fs::write(&marker, "first").unwrap();
    let data = Model {
        file: marker.clone(),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(axum::routing::get(models).post(message))
        .with_state(data.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut opts = SessionOptions::new(&work);
    for k in ["HOME", "USERPROFILE"] {
        opts.env.insert(k.into(), home.as_os_str().into());
    }
    opts.env
        .insert("DSH_HOME".into(), home.join(".dsh").into_os_string());
    opts.env
        .insert("DEEPSEEK_BASE_URL".into(), format!("http://{addr}").into());
    opts.env
        .insert("DEEPSEEK_API_KEY".into(), "sk-local-rust-test".into());
    opts.env.insert("DSH_TELEMETRY_DISABLED".into(), "1".into());
    if protocol == Protocol::Dsh {
        opts.model = Some("deepseek-official/deepseek-flash".into());
    }
    opts.approvals = kar::ApprovalMode::Host;
    let key = if protocol == Protocol::Acp {
        "KAR_DSH_ACP_BIN"
    } else {
        "KAR_DSH_BIN"
    };
    let bin = PathBuf::from(std::env::var_os(key).expect(key));
    let store = ChatStore::open(dir.path().join("store")).unwrap();
    let mut runtime = kar::runtime("dsh").unwrap();
    runtime.program = bin;
    Testbed {
        dir,
        opts,
        runtime,
        store,
        data,
        server,
    }
}

impl Testbed {
    fn partial_store(&self, id: Uuid, cut: kar::chat::ChatCursor, name: &str) -> ChatStore {
        let log_name = format!("{id}.karchat");
        let log = std::fs::read(self.dir.path().join("store").join(&log_name)).unwrap();
        let partial = self.dir.path().join(name);
        let store = ChatStore::open(&partial).unwrap();
        let offset = cut
            .to_string()
            .split_once(':')
            .unwrap()
            .1
            .parse::<usize>()
            .unwrap();
        std::fs::write(partial.join(log_name), &log[..offset]).unwrap();
        std::fs::File::create(partial.join(format!("{id}.lock"))).unwrap();
        store
    }
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness with the web transport"]
async fn missing_native_resume() {
    let fixture = testbed(Protocol::Dsh).await;
    let mut opts = fixture.opts.clone();
    let missing = Uuid::new_v4().to_string();
    opts.resume = Some(missing.clone());
    let e = fixture
        .runtime
        .open(opts)
        .await
        .err()
        .expect("missing session opened");
    assert!(
        matches!(e, kar::Error::Open(ref e) if matches!(**e,kar::OpenError::SessionNotFound {ref session_id,..} if *session_id == missing)),
        "{e}"
    );
    assert!(fixture.data.requests.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness 0.2.2-rc.5; uses an isolated home and a loopback model"]
async fn native_tool_queue_reconnect_and_resume() {
    let fixture = testbed(Protocol::Dsh).await;
    let Testbed {
        runtime,
        opts,
        data,
        store,
        ..
    } = &fixture;
    let marker = &data.file;
    let chat = store
        .create("dsh", Some(&runtime.program), opts.clone())
        .await
        .unwrap();
    assert_eq!(chat.info().protocol, Protocol::Dsh);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let reading = chat.resources().await.expect("owned DSH process");
        assert!(reading.rss > 0 && reading.processes >= 1);
        assert!(reading.scope.is_none());
    }
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    let mut first = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut second = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut events = store.follow(id, Some(first.cursor())).unwrap();
    let mut other = store.follow(id, Some(second.cursor())).unwrap();
    let first_id = Uuid::new_v4();
    let mut receipt_cut = None;
    {
        let send = chat.send(first_id, "DSH_FIRST");
        tokio::pin!(send);
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let mut saw_tool = false;
            let mut approved = std::collections::HashSet::new();
            loop {
                tokio::select! {
                    result = &mut send => break result.unwrap(),
                    next = events.recv() => {
                        first.apply(next.unwrap()).unwrap();
                        if receipt_cut.is_none() && first.turns().last().is_some_and(|t| t.id == first_id && !t.input_sent && t.native_turn_id.is_none()) {
                            receipt_cut = Some(first.cursor());
                        }
                        if !saw_tool {
                            second.apply(other.recv().await.unwrap()).unwrap();
                            if let Some(turn) = second.turns().last()
                                && turn.items.iter().any(|i| matches!(i.item, Item::Tool { .. }))
                            {
                                assert_eq!(turn.status, TurnStatus::Running);
                                saw_tool = true;
                                data.seen.notify_one();
                            }
                        }
                        for req in first.pending_requests().iter().filter(|r| approved.insert(r.id.to_string())) {
                            chat.decide(req, kar::PermissionDecision::AllowOnce).await.unwrap();
                        }
                    }
                }
            }
        }).await.unwrap();
        assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.failure);
        assert_eq!(result.items.iter().filter(|i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="dsh-first-answer")).count(),1);
    }
    let tool_cut = second.cursor();
    drop(other);
    drop(events);
    chat.close().await.unwrap();
    let queued = Uuid::new_v4();
    store.enqueue(id, queued, "DSH_SECOND").unwrap();
    let reopened = store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(reopened.info().native_id, native);
    tokio::time::timeout(Duration::from_secs(25), reopened.drain_queue())
        .await
        .unwrap()
        .unwrap();
    reopened.drain_queue().await.unwrap();
    reopened.close().await.unwrap();
    let snap = store.feed_snapshot(id).unwrap();
    let mut message_cut = None;
    for view in [&mut first, &mut second] {
        let mut f = store.follow(id, Some(view.cursor())).unwrap();
        while view.cursor() != snap.cursor {
            let e = f.recv().await.unwrap();
            assert!(view.apply(e.clone()).unwrap());
            assert!(!view.apply(e).unwrap());
            if view.turns().last().is_some_and(|t| t.id == first_id && t.items.iter().any(|i|
                matches!(&i.item, Item::Message { role: Role::Assistant, text, complete:false,.. } if !text.is_empty()))) {
                message_cut = Some(view.cursor());
            }
        }
        assert_eq!(view.turns(), snap.history.turns);
    }
    assert_eq!(first.turns().len(), 2);
    assert_eq!(first.turns()[1].id, queued);
    {
        let reqs = data.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        let history = reqs.last().unwrap()["messages"].to_string();
        for t in [
            "DSH_FIRST",
            "dsh-first-answer",
            "DSH_SECOND",
            "dsh-tool-output",
        ] {
            assert!(history.contains(t), "{t}");
        }
    }
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "second");
    // The provider now has two terminal turns. The partial host log knows only
    // the first. Rebuild it from different cuts, including before its receipt.
    for (index, cut) in [receipt_cut.unwrap(), tool_cut, message_cut.unwrap()]
        .into_iter()
        .enumerate()
    {
        let recovered_store = fixture.partial_store(id, cut, &format!("partial-{index}"));
        let mut view = ChatReplica::from_snapshot(recovered_store.feed_snapshot(id).unwrap());
        let previous = view.cursor();
        let before = data.requests.lock().unwrap().len();
        let restored = recovered_store
            .resume_with(id, |o| o.env = opts.env.clone())
            .await
            .unwrap();
        let replayed = restored.send(first_id, "DSH_FIRST").await.unwrap();
        assert_eq!(replayed.status, TurnStatus::Completed, "cut {index}");
        assert!(replayed.input_sent);
        assert_eq!(replayed.items, first.turns()[0].items, "cut {index}");
        assert_eq!(replayed.fork_anchor, first.turns()[0].fork_anchor);
        restored.close().await.unwrap();
        let end = recovered_store.feed_snapshot(id).unwrap().cursor;
        let mut replay = recovered_store.follow(id, Some(previous)).unwrap();
        while view.cursor() != end {
            let event = replay.recv().await.unwrap();
            assert!(view.apply(event.clone()).unwrap());
            assert!(!view.apply(event).unwrap());
        }
        assert_eq!(view.turns().len(), 1);
        assert_eq!(view.turns()[0].items, first.turns()[0].items);
        assert_eq!(data.requests.lock().unwrap().len(), before);
    }
    let branch = store
        .fork_at_with(id, first.turns()[0].id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_ne!(branch.info().native_id, native);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "first");
    let answer = branch.send(Uuid::new_v4(), "DSH_BRANCH").await.unwrap();
    assert_eq!(answer.status, TurnStatus::Completed);
    branch.close().await.unwrap();
    let branch_input = data.requests.lock().unwrap().last().unwrap()["messages"].to_string();
    assert!(branch_input.contains("dsh-first-answer") && !branch_input.contains("DSH_SECOND"));
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness 0.2.2-rc.5; uses an isolated home and a loopback model"]
async fn native_inbox_withdraw_and_background_job_stop() {
    let fixture = testbed(Protocol::Dsh).await;
    let Testbed {
        runtime,
        opts,
        data,
        ..
    } = &fixture;
    let controls = runtime.open(opts.clone()).await.unwrap();
    let _initial_file = controls.native_session_file().await.unwrap();
    let catalog = controls.models().await.unwrap();
    assert!(catalog["groups"].is_array());
    assert!(controls.inventory(kar::Inventory::Skills).await.unwrap()["skills"].is_array());
    assert!(
        controls
            .inventory(kar::Inventory::Commands)
            .await
            .unwrap()
            .is_array()
    );
    assert!(controls.stop_task("unknown-job").await.is_err());
    let held = controls.prompt("DSH_HOLD").await.unwrap();
    tokio::time::timeout(Duration::from_secs(8), data.held.notified())
        .await
        .unwrap();
    let (drop_id, discard) = controls.queue("DSH_REMOVE").await.unwrap();
    let (_, keep) = controls.queue("DSH_KEEP").await.unwrap();
    controls.withdraw(drop_id).await.unwrap();
    assert!(discard.report().await.is_err());
    data.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(12), held.report())
            .await
            .unwrap()
            .unwrap()
            .text,
        "held answer"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(12), keep.report())
            .await
            .unwrap()
            .unwrap()
            .text,
        "queued answer"
    );
    let file = controls
        .native_session_file()
        .await
        .unwrap()
        .expect("DSH must report the persisted session file");
    assert!(file.is_file());
    controls.close().await.unwrap();
    assert_eq!(controls.native_session_file().await.unwrap(), Some(file));
    assert!(
        data.requests
            .lock()
            .unwrap()
            .iter()
            .all(|b| !b["messages"].to_string().contains("DSH_REMOVE"))
    );
    let jobs = runtime.open(opts.clone()).await.unwrap();
    let mut events = jobs.subscribe(None).unwrap();
    let turn = jobs.prompt("DSH_BACKGROUND").await.unwrap();
    let job = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let record = events.next().await.unwrap();
            for event in &record.events {
                if let kar::Event::ApprovalRequested { id, .. } = event
                    && let Some(req) = jobs
                        .snapshot()
                        .pending_requests
                        .iter()
                        .find(|r| r.id == *id)
                {
                    jobs.decide(req, kar::PermissionDecision::AllowOnce)
                        .await
                        .unwrap();
                }
                if let kar::Event::Task {
                    id,
                    change: kar::TaskChange::Started { .. },
                } = event
                {
                    return id.clone();
                }
            }
        }
    })
    .await
    .unwrap();
    jobs.stop_task(&job).await.unwrap();
    tokio::time::timeout(Duration::from_secs(12), turn.report())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !jobs
            .tasks()
            .iter()
            .any(|t| t.id == job && t.status == kar::TaskStatus::Stopped)
        {
            events.next().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(jobs.stop_task(&job).await.is_err());
    jobs.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness 0.2.2-rc.5; uses an isolated home and a loopback model"]
async fn native_manual_compaction() {
    let fixture = testbed(Protocol::Dsh).await;
    let Testbed {
        runtime,
        opts,
        data,
        store,
        ..
    } = &fixture;
    let compact_chat = store
        .create("dsh", Some(&runtime.program), opts.clone())
        .await
        .unwrap();
    let timeout = Some(Duration::from_secs(15));
    let empty = compact_chat
        .start_compaction()
        .await
        .unwrap()
        .wait(timeout)
        .await;
    assert!(
        matches!(
            empty.status,
            kar::CompactionStatus::Failed {
                code: kar::CompactionFailure::NotCompacted,
                ..
            }
        ),
        "{empty:?}"
    );
    for n in 0..3 {
        let prompt = format!("DSH_COMPACT {n} {}", "long compactable input ".repeat(128));
        assert_eq!(
            compact_chat
                .send(Uuid::new_v4(), prompt)
                .await
                .unwrap()
                .status,
            TurnStatus::Completed
        );
    }
    data.hold_model_reply.send_replace(true);
    let cancelled = compact_chat.start_compaction().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), data.compact_entered.notified())
        .await
        .unwrap();
    let mut current =
        ChatReplica::from_snapshot(store.feed_snapshot(compact_chat.info().id).unwrap());
    if !current
        .last_compaction()
        .is_some_and(|b| b.state == "started")
    {
        let mut boundaries = store
            .follow(compact_chat.info().id, Some(current.cursor()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !current
                .last_compaction()
                .is_some_and(|b| b.state == "started")
            {
                current.apply(boundaries.recv().await.unwrap()).unwrap();
            }
        })
        .await
        .unwrap();
    }
    assert!(matches!(
        cancelled.wait(Some(Duration::from_millis(10))).await.status,
        kar::CompactionStatus::Unconfirmed {
            code: kar::CompactionUnconfirmed::Timeout
        }
    ));
    assert!(matches!(
        compact_chat.start_compaction().await,
        Err(kar::Error::Busy)
    ));
    compact_chat.abort().await.unwrap();
    let aborted = cancelled.wait(timeout).await;
    data.hold_model_reply.send_replace(false);
    assert!(
        matches!(aborted.status, kar::CompactionStatus::Aborted { .. }),
        "{aborted:?}"
    );
    let mut compact_view =
        ChatReplica::from_snapshot(store.feed_snapshot(compact_chat.info().id).unwrap());
    let op = compact_chat.start_compaction().await.unwrap();
    let report = op.wait(timeout).await;
    assert!(
        matches!(report.status, kar::CompactionStatus::Completed { .. }),
        "{report:?}"
    );
    assert!(report.response.is_some());
    let end = store.feed_snapshot(compact_chat.info().id).unwrap().cursor;
    let mut compact_feed = store
        .follow(compact_chat.info().id, Some(compact_view.cursor()))
        .unwrap();
    while compact_view.cursor() != end {
        compact_view
            .apply(compact_feed.recv().await.unwrap())
            .unwrap();
    }
    assert_eq!(compact_view.manual_compaction(), Some(&report));
    let boundary = compact_view.last_compaction().unwrap();
    assert_eq!(boundary.trigger.as_deref(), Some("manual"));
    assert!(boundary.compaction_id.is_some());
    compact_chat.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness 0.2.2-rc.5; uses an isolated home and a loopback model"]
async fn native_goal_rounds_survive_restart() {
    let fixture = testbed(Protocol::Dsh).await;
    let Testbed {
        runtime,
        data,
        store,
        ..
    } = &fixture;
    let mut opts = fixture.opts.clone();
    opts.settings.goals = true;
    let goal_chat = store
        .create("dsh", Some(&runtime.program), opts.clone())
        .await
        .unwrap();
    let goal_id = goal_chat.info().id;
    assert!(goal_chat.read_goal().await.unwrap().is_none());
    let update = |objective: &str, status: &str| kar::GoalSetOptions {
        objective: objective.into(),
        status: Some(status.into()),
        token_budget: None,
    };
    let bad_options = kar::GoalSetOptions {
        token_budget: Some(100),
        ..update("DSH_GOAL", "active")
    };
    assert!(goal_chat.set_goal(bad_options).await.is_err());
    assert!(goal_chat.read_goal().await.unwrap().is_none());
    let report = goal_chat
        .set_goal(update("DSH_GOAL", "paused"))
        .await
        .unwrap();
    assert_eq!(report.status, "paused");
    assert!(report.token_budget.is_none() && report.created_at.is_some());
    let edited = goal_chat
        .set_goal(update("DSH_GOAL edited", "paused"))
        .await
        .unwrap();
    assert_eq!(
        goal_chat.history().unwrap().goal,
        Some(Some(edited.clone()))
    );
    goal_chat.close().await.unwrap();

    let resumed = store
        .resume_with(goal_id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.read_goal().await.unwrap(), Some(edited));
    let mut view = ChatReplica::from_snapshot(store.feed_snapshot(goal_id).unwrap());
    let mut feed = store.follow(goal_id, Some(view.cursor())).unwrap();
    resumed
        .set_goal(update("DSH_GOAL edited", "active"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            view.apply(feed.recv().await.unwrap()).unwrap();
            if view
                .turns()
                .last()
                .is_some_and(|t| t.native_continuation && t.status == TurnStatus::Running)
                && view.goal().flatten().is_some_and(|g| g.status == "active")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let active_cut = view.cursor();
    resumed
        .set_goal(update("DSH_GOAL edited", "complete"))
        .await
        .unwrap();
    data.goal_release.send_replace(true);
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            view.apply(feed.recv().await.unwrap()).unwrap();
            if view.turns().last().is_some_and(|t| t.native_continuation && t.status == TurnStatus::Completed
                && t.items.iter().any(|i|matches!(&i.item, Item::Message{role:Role::Assistant,text,..} if text=="native goal round answer"))) { break; }
        }
    }).await.unwrap();
    assert_eq!(
        resumed.read_goal().await.unwrap().unwrap().status,
        "complete"
    );
    resumed.clear_goal().await.unwrap();
    assert_eq!(resumed.history().unwrap().goal, Some(None));
    let expected = resumed.history().unwrap().turns.last().unwrap().clone();
    assert!(
        expected.native_continuation
            && expected
                .checkpoint
                .as_ref()
                .is_some_and(|cp| cp.after_tree.is_some())
    );
    resumed.close().await.unwrap();
    let crashed = fixture.partial_store(goal_id, active_cut, "partial-goal");
    let count = data.requests.lock().unwrap().len();
    let repaired = crashed
        .resume_with(goal_id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    let recovered = repaired.history().unwrap().turns.pop().unwrap();
    assert!(recovered.native_continuation && !recovered.input_sent);
    assert_eq!(recovered.status, TurnStatus::Completed);
    assert_eq!(recovered.items, expected.items);
    assert_eq!(recovered.id, expected.id);
    assert!(
        recovered
            .checkpoint
            .as_ref()
            .is_none_or(|cp| cp.after_tree.is_none())
    );
    repaired.close().await.unwrap();
    assert_eq!(data.requests.lock().unwrap().len(), count);
}

#[tokio::test]
#[ignore = "requires the official earlier DSH CLI with the ACP profile"]
async fn legacy_cli_opens_using_acp() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let home = dir.path().join("home");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&home).unwrap();
    let mut opts = SessionOptions::new(&work);
    for key in ["HOME", "USERPROFILE"] {
        opts.env.insert(key.into(), home.clone().into_os_string());
    }
    opts.env
        .insert("DSH_HOME".into(), home.join(".dsh").into_os_string());
    opts.env.insert("DSH_TELEMETRY_DISABLED".into(), "1".into());
    opts.env
        .insert("DEEPSEEK_API_KEY".into(), "sk-local-test".into());
    opts.env
        .insert("DEEPSEEK_BASE_URL".into(), "http://127.0.0.1:1".into());
    let bin = PathBuf::from(std::env::var_os("KAR_DSH_ACP_BIN").expect("set KAR_DSH_ACP_BIN"));
    let mut runtime = kar::runtime("dsh").unwrap();
    runtime.program = bin.clone();
    let session = runtime.open(opts.clone()).await.unwrap();
    assert_eq!(session.protocol(), Protocol::Acp);
    assert!(session.capabilities().resume);
    session.close().await.unwrap();

    let store = ChatStore::open(dir.path().join("store")).unwrap();
    let chat = store.create("dsh", Some(&bin), opts.clone()).await.unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    chat.close().await.unwrap();
    let resumed = store
        .resume_with(id, |o| o.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    resumed.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires DSH 0.2.2-rc.5+ and its official Web profile"]
async fn native_session_log_upload() {
    check_session_log_upload(Protocol::Dsh).await;
}

#[tokio::test]
#[ignore = "requires DSH 0.2.1-rc.5 and its official ACP profile"]
async fn legacy_session_log_upload() {
    check_session_log_upload(Protocol::Acp).await;
}

async fn check_session_log_upload(protocol: Protocol) {
    let mut tb = testbed(protocol).await;
    let home = std::path::Path::new(&tb.opts.env[&std::ffi::OsString::from("DSH_HOME")]);
    std::fs::create_dir_all(home).unwrap();
    let home_patch = home.join("cordis.patch.yml");
    let original = r#"[{"id":"session-log-deepseek","config":{"enabled":true}}]"#;
    std::fs::write(&home_patch, original).unwrap();
    tb.opts.approvals = kar::ApprovalMode::Deny;
    tb.opts.handshake_timeout = Duration::from_secs(15);
    let baseline = tb.runtime.open(tb.opts.clone()).await.unwrap();
    assert_eq!(baseline.protocol(), protocol);
    let report = tokio::time::timeout(
        Duration::from_secs(25),
        baseline.prompt("DSH_LOG_BASELINE").await.unwrap().report(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.outcome, kar::Outcome::Completed);
    baseline.close().await.unwrap();
    assert!(
        tb.data
            .upload_requests
            .lock()
            .unwrap()
            .drain(..)
            .any(|(_, log)| log),
        "default uploader was not active in the baseline"
    );
    tb.opts.settings.session_log_upload = Some(kar::SessionLogUpload::Off);
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    let native = chat.info().native_id.clone();
    let id = chat.info().id;
    let first_turn = Uuid::new_v4();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(30),
            chat.send(first_turn, "DSH_LOG_ROOT delegate")
        )
        .await
        .unwrap()
        .unwrap()
        .status,
        TurnStatus::Completed
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !tb
            .data
            .upload_requests
            .lock()
            .unwrap()
            .iter()
            .any(|(session, _)| !session.is_empty() && *session != native)
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expected a separate native child model request");
    if protocol == Protocol::Dsh {
        let child_id = tb
            .data
            .upload_requests
            .lock()
            .unwrap()
            .iter()
            .find(|(session, _)| !session.is_empty() && session != &native)
            .unwrap()
            .0
            .clone();
        let mut records = chat.subscribe(None).unwrap();
        let selected = tokio::time::timeout(Duration::from_secs(8), async {
            while let Some(record) = records.next().await {
                if record.session_id.as_deref() != Some(&child_id) {
                    continue;
                }
                for event in &record.events {
                    if let kar::Event::Model { model } = event {
                        let data = &record.native["event"]["data"];
                        let (provider, id) =
                            if record.native["event"]["type"] == "subagent/descriptor" {
                                (&data["agentProvider"], &data["agentModel"])
                            } else {
                                assert_eq!(record.native["event"]["type"], "request/header");
                                (
                                    &data["header"]["config"]["provider"],
                                    &data["header"]["config"]["model"],
                                )
                            };
                        assert_eq!(record.agent_path.last(), Some(&child_id));
                        assert_eq!(
                            model,
                            &format!("{}/{}", provider.as_str().unwrap(), id.as_str().unwrap())
                        );
                        return model.clone();
                    }
                }
            }
            panic!("child stream closed before its native model was reported")
        })
        .await
        .expect("expected attributed DSH child model");
        assert_eq!(chat.snapshot().model.as_ref(), tb.opts.model.as_ref());
        eprintln!("DSH reported {selected} for its separate child session");
    }
    chat.close().await.unwrap();
    let reopened = tb
        .store
        .resume_with(id, |o| o.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(reopened.info().native_id, native);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(25),
            reopened.send(Uuid::new_v4(), "DSH_LOG_RESUMED")
        )
        .await
        .unwrap()
        .unwrap()
        .status,
        TurnStatus::Completed
    );
    reopened.close().await.unwrap();
    if protocol == Protocol::Dsh {
        let branch = tb
            .store
            .fork_at_with(id, first_turn, |o| o.env = tb.opts.env.clone())
            .await
            .unwrap();
        assert_ne!(branch.info().native_id, native);
        assert_eq!(
            branch
                .send(Uuid::new_v4(), "DSH_LOG_FORK")
                .await
                .unwrap()
                .status,
            TurnStatus::Completed
        );
        let branch_id = branch.info().native_id.clone();
        branch.close().await.unwrap();
        assert!(
            tb.data
                .upload_requests
                .lock()
                .unwrap()
                .iter()
                .any(|(session, _)| *session == branch_id)
        );
    }
    let sent = tb.data.upload_requests.lock().unwrap();
    assert!(
        sent.iter()
            .filter(|(session, _)| *session == native)
            .count()
            >= 2
    );
    assert!(
        sent.iter().all(|(_, log)| !*log),
        "DSH attached a session log under the off setting"
    );
    assert_eq!(std::fs::read_to_string(home_patch).unwrap(), original);
}

#[tokio::test]
#[ignore = "requires DSH 0.2.2-rc.5+; inspects the plugin loader before the first model request"]
async fn native_uploader_already_disabled_and_conflicting_entries() {
    let mut tb = testbed(Protocol::Dsh).await;
    tb.opts.settings.session_log_upload = Some(kar::SessionLogUpload::Off);
    let home = tb.dir.path().join("home/.dsh");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("cordis.patch.yml");
    std::fs::write(&path, r#"[{"id":"session-log-deepseek","disabled":true}]"#).unwrap();
    let first = tb.runtime.open(tb.opts.clone()).await.unwrap();
    first.close().await.unwrap();
    // The same id shadows the official row, causing Cordis to skip the guarded patch.
    let conflict = r#"[{"insert":[{"id":"session-log-deepseek","name":"@deepseek-ai/dsh-tools","disabled":true}]}]"#;
    std::fs::write(&path, conflict).unwrap();
    let error = tb
        .runtime
        .open(tb.opts.clone())
        .await
        .err()
        .expect("DSH admitted an unverified uploader");
    assert!(
        matches!(error,kar::Error::Unsupported(ref msg) if msg.contains("session_log_upload")),
        "{error}"
    );
    assert!(tb.data.requests.lock().unwrap().is_empty());
    assert!(tb.data.upload_requests.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), conflict);
}

fn crew_server() -> kar::McpServer {
    kar::McpServer::Stdio {
        command: env!("CARGO_BIN_EXE_kar").into(),
        args: vec!["mcp".into(), "--runtimes".into(), "codex".into()],
        env: Default::default(),
    }
}

#[tokio::test]
#[ignore = "requires DSH with the Web profile; calls a real Rust stdio MCP server"]
async fn native_session_mcp() {
    check_session_mcp(Protocol::Dsh).await;
}

#[tokio::test]
#[ignore = "requires DSH with the ACP profile; calls a real Rust stdio MCP server"]
async fn legacy_session_mcp() {
    check_session_mcp(Protocol::Acp).await;
}

#[cfg(unix)]
#[path = "support/mcp_process.rs"]
mod mcp_process;
#[cfg(unix)]
use mcp_process::{assert_mcp_stopped, tracked_mcp_running};

async fn check_session_mcp(protocol: Protocol) {
    let mut tb = testbed(protocol).await;
    let home = tb.dir.path().join("home/.dsh");
    std::fs::create_dir_all(&home).unwrap();
    let patch = home.join("cordis.patch.yml");
    let original=json!([{"insert":[{"id":"configured-rust-mcp","name":"@deepseek-ai/dsh-mcp-client","config":{"transport":"stdio","serverName":"configured","command":env!("CARGO_BIN_EXE_kar"),"args":["mcp"],"cwd":tb.opts.cwd,"failOnStartupError":true}}]}]).to_string();
    std::fs::write(&patch, &original).unwrap();
    tb.opts.settings.session_log_upload = Some(kar::SessionLogUpload::Off);
    // The Rust MCP server canonicalizes --cwd before its handshake. This
    // relative argument only works from the chat workspace with literal argv.
    let marker = format!("mcp $HOME {}", Uuid::new_v4());
    std::fs::create_dir(tb.opts.cwd.join(&marker)).unwrap();
    let mut tracked = crew_server();
    if let kar::McpServer::Stdio { args, .. } = &mut tracked {
        args.extend(["--cwd".into(), marker.clone()]);
    }
    tb.opts.mcp_servers.insert("session".into(), tracked);
    let declared = declaration::for_installation(&tb.runtime, &tb.opts).await;
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    declaration::check(&declared, chat.capabilities());
    assert!(chat.capabilities().session_mcp);
    assert_eq!(
        chat.capabilities().mcp_transports,
        Some([kar::McpTransport::Stdio].as_slice())
    );
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    {
        let send = chat.send(Uuid::new_v4(), "DSH_MCP_CALL");
        tokio::pin!(send);
        let mut approvals = std::collections::HashSet::new();
        tokio::time::timeout(Duration::from_secs(30),async {
            loop {
                tokio::select! {
                    turn=&mut send => {assert_eq!(turn.unwrap().status,TurnStatus::Completed); break;}
                    _=tokio::time::sleep(Duration::from_millis(30)) => {
                        for req in chat.history().unwrap().pending_requests.iter().filter(|r|approvals.insert(r.id.to_string())) {
                            chat.decide(req,kar::PermissionDecision::AllowOnce).await.unwrap();
                        }
                    }
                }
            }
        }).await.unwrap();
    }
    let names = |state: &Model| -> Vec<String> {
        let requests = state.requests.lock().unwrap();
        requests.last().unwrap()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .filter(|n| n.starts_with("mcp__") && n.ends_with("__list"))
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    assert_eq!(
        names(&tb.data),
        ["mcp__configured__list", "mcp__session__list"]
    );
    {
        let seen = tb.data.requests.lock().unwrap();
        let result = seen.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .find(|b| b["type"] == "tool_result")
            .expect("native MCP tool result");
        assert_ne!(result["is_error"], true, "{result}");
        let answer = result["content"].to_string();
        assert!(
            answer.contains("agents") && answer.contains("unread"),
            "{answer}"
        );
    }
    let mut plain = tb.opts.clone();
    plain.mcp_servers.clear();
    let concurrent = tb.runtime.open(plain.clone()).await.unwrap();
    assert_eq!(
        concurrent
            .prompt("DSH_MCP_PLAIN")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        kar::Outcome::Completed
    );
    assert_eq!(names(&tb.data), ["mcp__configured__list"]);
    concurrent.close().await.unwrap();
    #[cfg(unix)]
    assert!(tracked_mcp_running(&marker).await);
    chat.close().await.unwrap();
    #[cfg(unix)]
    assert_mcp_stopped(&marker).await;
    let resumed = tb
        .store
        .resume_with(id, |o| {
            o.env = tb.opts.env.clone();
            o.mcp_servers.insert("fresh".into(), crew_server());
        })
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    assert_eq!(
        resumed
            .send(Uuid::new_v4(), "DSH_MCP_RESUPPLIED")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    assert_eq!(
        names(&tb.data),
        ["mcp__configured__list", "mcp__fresh__list"]
    );
    resumed.close().await.unwrap();
    let omitted = tb
        .store
        .resume_with(id, |o| o.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(
        omitted
            .send(Uuid::new_v4(), "DSH_MCP_OMITTED")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    assert_eq!(names(&tb.data), ["mcp__configured__list"]);
    omitted.close().await.unwrap();
    tb.data.requests.lock().unwrap().clear();
    for (name, server) in [
        ("configured", crew_server()),
        (
            "broken",
            kar::McpServer::Stdio {
                command: tb
                    .dir
                    .path()
                    .join("missing-server")
                    .to_str()
                    .unwrap()
                    .to_owned(),
                args: vec![],
                env: Default::default(),
            },
        ),
    ] {
        let mut opts = tb.opts.clone();
        opts.mcp_servers.insert(name.into(), server);
        assert!(
            tb.runtime.open(opts).await.is_err(),
            "admitted invalid MCP service {name}"
        );
        assert!(tb.data.requests.lock().unwrap().is_empty());
        #[cfg(unix)]
        assert_mcp_stopped(&marker).await;
    }
    assert_eq!(std::fs::read_to_string(patch).unwrap(), original);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires DSH Web with the native bash tool and a loopback model"]
async fn native_reused_tool_id_is_distinct_across_stream_replay_and_recovery() {
    let tb = testbed(Protocol::Dsh).await;
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let before = tb.store.feed_snapshot(id).unwrap();
    let turn_id = Uuid::new_v4();
    let mut live = ChatReplica::from_snapshot(tb.store.feed_snapshot(id).unwrap());
    let mut feed = tb.store.follow(id, Some(live.cursor())).unwrap();
    let finished = {
        let send = chat.send(turn_id, "DSH_REPEAT_TOOLS");
        tokio::pin!(send);
        let mut approved = std::collections::HashSet::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    result = &mut send => break result.unwrap(),
                    event = feed.recv() => {
                        live.apply(event.unwrap()).unwrap();
                        for request in live.pending_requests() {
                            if approved.insert(request.id.to_string()) {
                                chat.decide(request, kar::PermissionDecision::AllowOnce).await.unwrap();
                            }
                        }
                    }
                }
            }
        })
        .await
        .unwrap()
    };
    assert_eq!(finished.status, TurnStatus::Completed);
    let cards = |turn: &kar::chat::ChatTurn| {
        turn.items
            .iter()
            .filter_map(|item| match &item.item {
                Item::Tool { id, activity } => Some((id.clone(), activity.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let tools = cards(&finished);
    assert_eq!(tools.len(), 2);
    for ((id, tool), expected) in tools.iter().zip(["first", "second"]) {
        assert_eq!(id, "dsh-tool");
        assert_eq!(
            tool.input.as_ref().unwrap()["command"],
            format!("printf {expected}")
        );
        let output = tool.result.as_ref().unwrap().text().unwrap();
        // DSH appends labelled sandbox stderr on Linux kernels with an older Landlock ABI.
        // Compare stdout here. The replica and restart assertions below compare the full result.
        let stdout = output
            .split_once("\n[stderr]\n")
            .map_or(output.as_str(), |(stdout, _)| stdout);
        assert_eq!(stdout, expected);
    }
    chat.close().await.unwrap();
    let tip = tb.store.feed_snapshot(id).unwrap().cursor;
    let mut second = ChatReplica::from_snapshot(before);
    let mut feed = tb.store.follow(id, Some(second.cursor())).unwrap();
    let mut cut = None;
    while second.cursor() != tip {
        let event = feed.recv().await.unwrap();
        assert!(second.apply(event.clone()).unwrap());
        assert!(!second.apply(event).unwrap());
        if let Some(turn) = second.turns().last() {
            let tools = cards(turn);
            if tools.len() == 2 && tools[1].1.input_partial {
                cut = Some(second.cursor());
            }
        }
    }
    assert_eq!(second.turns()[0].items, finished.items);
    let store = tb.partial_store(id, cut.expect("second streamed input"), "reused-call");
    let recovered = store
        .resume_with(id, |o| o.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(
        recovered
            .send(turn_id, "DSH_REPEAT_TOOLS")
            .await
            .unwrap()
            .items,
        finished.items
    );
    recovered.close().await.unwrap();
    assert_eq!(
        tb.data
            .repeated_calls
            .load(std::sync::atomic::Ordering::Acquire),
        3
    );
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness with the web transport"]
async fn native_scoped_tool_policy() {
    check_scoped_tool_policy(Protocol::Dsh).await;
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness with the ACP transport"]
async fn legacy_scoped_tool_policy() {
    check_scoped_tool_policy(Protocol::Acp).await;
}

async fn check_scoped_tool_policy(protocol: Protocol) {
    let tb = testbed(protocol).await;
    let mut opts = tb.opts.clone();
    opts.settings.disallowed_tools.push("bash".into());
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    let other = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    let check = |marker: &str, expected: bool| {
        let reqs = tb.data.requests.lock().unwrap();
        let request = reqs.last().unwrap();
        assert!(request["messages"].to_string().contains(marker));
        let names: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(!names.is_empty());
        assert_eq!(names.contains(&"bash"), expected, "{names:?}");
    };
    let first = Uuid::new_v4();
    assert_eq!(
        chat.send(first, "DSH_POLICY_FIRST").await.unwrap().status,
        TurnStatus::Completed
    );
    check("DSH_POLICY_FIRST", false);
    // Even a provider that names an unadvertised native tool cannot execute it.
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(15),
            chat.send(Uuid::new_v4(), "DSH_POLICY_PROBE")
        )
        .await
        .unwrap()
        .unwrap()
        .status,
        TurnStatus::Completed
    );
    assert!(
        tb.data
            .policy_called
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(!tb.opts.cwd.join("policy-tool-ran").exists());
    assert_eq!(
        other
            .send(Uuid::new_v4(), "DSH_POLICY_OTHER")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    check("DSH_POLICY_OTHER", true);
    other.close().await.unwrap();
    chat.close().await.unwrap();
    let reopened = ChatStore::open(tb.dir.path().join("store")).unwrap();
    let resumed = reopened
        .resume_with(id, |o| o.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    assert_eq!(
        resumed
            .send(Uuid::new_v4(), "DSH_POLICY_RESUMED")
            .await
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    check("DSH_POLICY_RESUMED", false);
    resumed.close().await.unwrap();
    if protocol == Protocol::Dsh {
        let child = reopened
            .fork_at_with(id, first, |o| {
                o.env = tb.opts.env.clone();
                o.settings.disallowed_tools = vec!["write".into()];
            })
            .await
            .unwrap();
        assert_ne!(child.info().native_id, native);
        assert_eq!(
            child
                .send(Uuid::new_v4(), "DSH_POLICY_CHILD")
                .await
                .unwrap()
                .status,
            TurnStatus::Completed
        );
        check("DSH_POLICY_CHILD", true);
        {
            let reqs = tb.data.requests.lock().unwrap();
            assert!(
                reqs.last().unwrap()["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|t| t["name"] != "write")
            );
        }
        child.close().await.unwrap();
    }
    let before = tb.data.requests.lock().unwrap().len();
    opts.settings.disallowed_tools = vec!["no_such_native_tool".into()];
    assert!(matches!(
        tb.store
            .create("dsh", Some(&tb.runtime.program), opts)
            .await,
        Err(kar::Error::Unsupported(_))
    ));
    assert_eq!(tb.data.requests.lock().unwrap().len(), before);
    tb.server.abort();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
#[ignore = "requires official DeepSeek Harness with a working native process sandbox"]
async fn native_scoped_writable_directories() {
    check_scoped_writable_directories(Protocol::Dsh).await;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
#[ignore = "requires official DeepSeek Harness 0.2.2-rc.3 with a working native process sandbox"]
async fn acp_scoped_writable_directories() {
    check_scoped_writable_directories(Protocol::Acp).await;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn check_scoped_writable_directories(protocol: Protocol) {
    let mut tb = testbed(protocol).await;
    if protocol == Protocol::Acp {
        tb.runtime.program = std::env::var_os("KAR_DSH_ACP_ROOTS_BIN")
            .expect("KAR_DSH_ACP_ROOTS_BIN")
            .into();
    }
    // The native process sandbox may allow its temporary directory globally.
    let extra = tempfile::Builder::new()
        .prefix(".kar-dsh-roots-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let home = tb.dir.path().join("home/.dsh");
    std::fs::create_dir_all(&home).unwrap();
    let original = json!([{"id":"sandbox-policy","config":{"mode":"workspace-write"}}]).to_string();
    let patch = home.join("cordis.patch.yml");
    std::fs::write(&patch, &original).unwrap();
    tb.opts.approvals = kar::ApprovalMode::Deny;
    let mut opts = tb.opts.clone();
    opts.settings.additional_directories = vec![extra.path().into()];
    let stage = |marker: &str| {
        let file = extra.path().join(format!("{marker}-file"));
        let shell = extra.path().join(format!("{marker}-shell"));
        let mut script = tb.data.directory_tools.lock().unwrap();
        assert!(script.is_empty());
        script.push_back(("write", json!({"file_path":file,"content":marker})));
        let quoted = shell.to_str().unwrap().replace(char::from(39), r"'\''");
        script.push_back(("bash", json!({"command":format!("printf %s {marker} > '{quoted}'"),"description":"write directory fixture"})));
        (file, shell)
    };
    let run = async |chat: &kar::chat::ChatSession, marker: &str, granted: bool| {
        let targets = stage(marker);
        let prompt = format!("DSH_WRITABLE {marker}");
        let send = chat.send(Uuid::new_v4(), prompt);
        let result = tokio::time::timeout(Duration::from_secs(30), send)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, TurnStatus::Completed);
        for file in [targets.0, targets.1] {
            if granted {
                assert_eq!(
                    std::fs::read_to_string(&file)
                        .unwrap_or_else(|err| panic!("{file:?}: {err}. Tools: {result:?}")),
                    marker
                );
            } else {
                assert!(!file.exists(), "{file:?} escaped the session workspace");
            }
        }
    };
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), opts)
        .await
        .unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    run(&chat, "granted", true).await;
    let other = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    run(&other, "other", false).await;
    other.close().await.unwrap();
    chat.close().await.unwrap();
    let store = ChatStore::open(tb.dir.path().join("store")).unwrap();
    let chat = store
        .resume_with(id, |o| o.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(chat.info().native_id, native);
    run(&chat, "resumed", true).await;
    chat.close().await.unwrap();
    assert_eq!(std::fs::read_to_string(patch).unwrap(), original);
    tb.server.abort();
}

#[tokio::test]
#[ignore = "requires official DeepSeek Harness 0.2.1-rc.5"]
async fn legacy_refuses_scoped_directories_before_prompt() {
    let mut tb = testbed(Protocol::Acp).await;
    tb.opts.settings.additional_directories = vec![tb.dir.path().into()];
    assert!(matches!(
        tb.runtime.open(tb.opts.clone()).await,
        Err(kar::Error::Unsupported(_))
    ));
    assert!(tb.data.requests.lock().unwrap().is_empty());
    tb.server.abort();
}

#[path = "support/public_readbacks.rs"]
mod public_readbacks;

#[tokio::test]
#[ignore = "real Web and ACP DSH with loopback models"]
async fn public_prompt_readbacks() {
    for protocol in [Protocol::Dsh, Protocol::Acp] {
        let f = testbed(protocol).await;
        let mut options = f.opts.clone();
        options.settings.append_system_prompt = Some("KAR_PUBLIC_READBACK_FIXTURE".into());
        let session = f.runtime.open(options).await.unwrap();
        public_readbacks::check(&session).await.unwrap();
        public_readbacks::stream::check(&session).await.unwrap();
        session.close().await.unwrap();
    }
}
