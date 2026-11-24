use super::emit;
use kar::{
    Event, FailureClass, InputReceipt, SessionOptions,
    chat::{ChatReplica, ChatStore, HttpChatFeed, InputStatus, Item, Role, TurnStatus},
};
use serde_json::{Value, json};
use std::io::BufRead;
use uuid::Uuid;

pub(super) fn peer(args: &[String]) {
    if args.iter().any(|s| s == "--version") {
        println!("2.1.330 (Claude Code)");
        return;
    }
    assert!(args.iter().any(|s| s == "--replay-user-messages"));
    let session = args
        .windows(2)
        .find(|pair| matches!(pair[0].as_str(), "--session-id" | "--resume"))
        .unwrap()[1]
        .as_str();
    let lifecycle = |id: &str, state: &str| {
        emit(
            json!({"type":"command_lifecycle","session_id":session,"command_uuid":id,"state":state}),
        )
    };
    let reply = |id: &Value| {
        emit(
            json!({"type":"control_response","response":{"request_id":id,"subtype":"success","response":{}}}),
        )
    };
    let init = |advertise: bool| {
        emit(
            json!({"type":"system","session_id":session,"subtype":"init","capabilities": if advertise { vec!["interrupt_cancel_queued_v1"] } else { vec![] }}),
        )
    };
    let output = |text: &str| {
        emit(
            json!({"type":"assistant","session_id":session,"uuid":Uuid::new_v4(),"message":{"content":[{"type":"text","text":text}]}}),
        )
    };
    let end = |id: &str| {
        emit(
            json!({"type":"result","session_id":session,"user_message_uuids":[Uuid::new_v4(),id],"is_error":false,"subtype":"success"}),
        )
    };
    let mut waiting = String::new();
    let mut mode = String::new();
    let mut late_reply = Value::Null;
    let mut previous = Value::Null;
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let packet: Value = serde_json::from_str(&line).unwrap();
        if packet["type"] == "control_request" {
            if packet["request"]["subtype"] == "interrupt" {
                assert_eq!(
                    packet["request"].get("cancel_queued"),
                    matches!(mode.as_str(), "late abort" | "crash window" | "held steer")
                        .then_some(&Value::Bool(true))
                );
                if mode == "late abort" {
                    lifecycle(&waiting, "cancelled");
                    late_reply = packet["request_id"].clone();
                } else {
                    reply(&packet["request_id"]);
                    if mode == "idle notice" {
                        previous = json!({"type":"result","session_id":session,"subtype":"success","uuid":Uuid::new_v4(),"is_error":false});
                        emit(previous.clone());
                    } else if mode == "interrupt notification" {
                        emit(
                            json!({"type":"result","session_id":session,"is_error":false,"subtype":"success"}),
                        );
                        lifecycle(&waiting, "started");
                        output("host text");
                        end(&waiting);
                    } else if mode == "abort started" {
                        end(&waiting);
                    } else {
                        lifecycle(&waiting, "cancelled");
                        if mode == "crash window" {
                            emit(
                                json!({"type":"result","session_id":session,"is_error":false,"subtype":"success"}),
                            );
                        }
                    }
                }
            } else if packet["request"]["subtype"] == "initialize" {
                emit(
                    json!({"type":"control_response","response":{"request_id":packet["request_id"],"subtype":"success","response":{"commands":[{"name":"compact","builtin":true}]}}}),
                );
            } else if packet["request"]["subtype"] == "get_status" {
                emit(
                    json!({"type":"control_response","response":{"request_id":packet["request_id"],"subtype":"success","response":{"sections":[{"rows":[{"label":"Session ID","value":session}]}]}}}),
                );
            } else {
                reply(&packet["request_id"]);
            }
            continue;
        }
        assert_eq!(packet["type"], "user");
        let id = packet["uuid"].as_str().unwrap();
        mode = packet["message"]["content"].as_str().unwrap().to_owned();
        if !late_reply.is_null() {
            reply(&late_reply);
            late_reply = Value::Null;
        }
        // A child's receipt and a different root input never settle this write.
        emit(
            json!({"type":"command_lifecycle","command_uuid":id,"state":"completed","parent_tool_use_id":"child","session_id":session}),
        );
        lifecycle(&Uuid::new_v4().to_string(), "cancelled");
        match mode.as_str() {
            "duplicate replay" => {
                emit(
                    json!({"type":"user","session_id":session,"uuid":id,"isReplay":true,"message":{"role":"user","content":"duplicate replay"}}),
                );
                lifecycle(id, "completed");
            }
            "duplicate completed" => lifecycle(id, "completed"),
            "steer owner" | "boundary owner" => {
                lifecycle(id, "started");
                init(true);
                waiting = id.to_owned();
                output(if mode == "steer owner" {
                    "ready for steering"
                } else {
                    "ready for boundary"
                });
            }
            "delayed start receipt" => {
                lifecycle(id, "queued");
                end(&waiting);
                let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !std::path::Path::new("../original-checkpointed").is_file() {
                    assert!(std::time::Instant::now() < until);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                // The CLI changes the workspace before its next lifecycle packet reaches Rust.
                std::fs::write("between-native-frames.txt", "follow-up").unwrap();
                lifecycle(id, "started");
                output("follow-up complete");
                end(id);
            }
            "refused steer" => {
                emit(
                    json!({"type":"command_lifecycle","command_uuid":id,"state":"started","session_id":Uuid::new_v4()}),
                );
                lifecycle(id, "completed");
            }
            "held steer" => {
                lifecycle(id, "queued");
                end(&waiting);
                waiting = id.to_owned();
            }
            "sweep" => {
                lifecycle(id, "queued");
                lifecycle(id, "cancelled");
            }
            "late abort" | "abort unsupported" | "abort started" => {
                init(mode != "abort unsupported");
                lifecycle(id, "queued");
                if mode == "abort started" {
                    lifecycle(id, "started");
                }
                waiting = id.to_owned();
                // Clear the independent native init that preceded queued.
                if mode != "abort started" {
                    emit(
                        json!({"type":"result","session_id":session,"is_error":false,"subtype":"success"}),
                    );
                }
            }
            "interrupt notification" => {
                lifecycle(id, "queued");
                init(false);
                output("separate pending notification");
                waiting = id.to_owned();
            }
            "crash window" => {
                lifecycle(id, "queued");
                init(true);
                output("notification in progress");
                waiting = id.to_owned();
            }
            "notification" | "joined" | "joined by model" => {
                lifecycle(id, "queued");
                init(true);
                output("notification text");
                emit(
                    json!({"type":"assistant","session_id":session,"message":{"content":[{"type":"tool_use","id":"read-1","name":"Read","input":{"file_path":"fixture"}}]}}),
                );
                emit(
                    json!({"type":"user","session_id":session,"message":{"content":[{"type":"tool_result","tool_use_id":"read-1","content":"tool result"}]}}),
                );
                emit(
                    json!({"type":"assistant","parent_tool_use_id":"read-1","session_id":session,"message":{"content":[{"type":"text","text":"child notification"}]}}),
                );
                if mode == "notification" {
                    emit(
                        json!({"type":"result","session_id":session,"is_error":true,"subtype":"error_during_execution","errors":["notification failed"]}),
                    );
                }
                if mode == "joined by model" {
                    emit(
                        json!({"type":"stream_event","session_id":session,"user_message_uuids":[id],"event":{"type":"message_start","message":{"id":"host-response"}}}),
                    );
                    output("host text");
                    lifecycle(id, "started");
                } else {
                    lifecycle(id, "started");
                    output("host text");
                }
                end(id);
            }
            _ => {
                if matches!(mode.as_str(), "replayed result" | "after idle notice") {
                    emit(previous.clone());
                }
                if mode == "same input different result" {
                    let mut separate = previous.clone();
                    separate["uuid"] = json!(Uuid::new_v4());
                    emit(separate);
                }
                if mode == "replay after reopen" {
                    let mut stored: Value =
                        serde_json::from_slice(&std::fs::read("native-result.json").unwrap())
                            .unwrap();
                    stored.as_object_mut().unwrap().remove("user_message_uuid");
                    emit(stored);
                }
                lifecycle(id, "queued");
                lifecycle(id, "started");
                emit(
                    json!({"type":"user","uuid":id,"session_id":session,"isReplay":true,"message":{"role":"user","content":mode}}),
                );
                lifecycle(id, "completed");
                output("host text");
                previous = json!({"type":"result","session_id":session,"uuid":Uuid::new_v4(),"user_message_uuid":id,"is_error":false,"subtype":"success"});
                emit(previous.clone());
                std::fs::write("native-result.json", serde_json::to_vec(&previous).unwrap())
                    .unwrap();
                if mode == "idle notice" {
                    init(true);
                    output("independent idle reply");
                    emit(
                        json!({"type":"assistant","session_id":session,"message":{"content":[{"type":"tool_use","id":"idle-read","name":"Read","input":{}}]}}),
                    );
                    emit(
                        json!({"type":"user","session_id":session,"message":{"content":[{"type":"tool_result","tool_use_id":"idle-read","content":"idle result"}]}}),
                    );
                }
            }
        }
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let temp = tempfile::tempdir()?;
    let work = temp.path().join("work");
    std::fs::create_dir(&work)?;
    let store = ChatStore::open(temp.path().join("chats"))?;
    let mut opts = SessionOptions::new(&work);
    opts.env.insert("KAR_TEST_CLAUDE_INPUT".into(), "1".into());
    let chat = store
        .create("claude", Some(&std::env::current_exe()?), opts.clone())
        .await?;
    let id = chat.info().id;
    let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listen.local_addr()?);
    let router = kar::chat::event_router(store.clone(), id);
    let http = tokio::spawn(async move { axum::serve(listen, router).await.unwrap() });
    let mut feed = HttpChatFeed::new(&url)?;
    let mut other = Some(HttpChatFeed::new(&url)?);
    let mut a = ChatReplica::from_snapshot(feed.snapshot().await?);
    let mut b = ChatReplica::from_snapshot(other.as_mut().unwrap().snapshot().await?);
    for (mode, status, receipt) in [
        (
            "duplicate replay",
            TurnStatus::Failed,
            InputReceipt::Duplicate,
        ),
        (
            "duplicate completed",
            TurnStatus::Failed,
            InputReceipt::Duplicate,
        ),
        ("sweep", TurnStatus::Aborted, InputReceipt::Cancelled),
        ("normal", TurnStatus::Completed, InputReceipt::Started),
        (
            "replayed result",
            TurnStatus::Completed,
            InputReceipt::Started,
        ),
        (
            "same input different result",
            TurnStatus::Completed,
            InputReceipt::Started,
        ),
        ("notification", TurnStatus::Completed, InputReceipt::Started),
        ("joined", TurnStatus::Completed, InputReceipt::Started),
        (
            "joined by model",
            TurnStatus::Completed,
            InputReceipt::Started,
        ),
    ] {
        let input = Uuid::new_v4();
        store.enqueue(id, input, mode)?;
        chat.drain_queue().await?;
        let snapshot = store.feed_snapshot(id)?;
        while a.cursor() != snapshot.cursor {
            assert!(a.apply(feed.recv(a.cursor()).await?)?);
            if let Some(mut client) = other.take() {
                assert!(b.apply(client.recv(b.cursor()).await?)?);
            }
        }
        if mode == "replayed result" {
            assert!(a.notifications().is_empty());
        }
        if mode == "same input different result" {
            assert_eq!(a.notifications().len(), 1);
        }
        let turn = a.turns().last().unwrap();
        assert_eq!(turn.id, input);
        assert_eq!(turn.status, status, "{mode}");
        assert_eq!(turn.native_input_receipt, Some(receipt));
        assert_eq!(
            turn.failure.as_ref().map(|f| f.class),
            (receipt == InputReceipt::Duplicate).then_some(FailureClass::InvalidRequest)
        );
        assert_eq!(turn.items.iter().filter(|i| matches!(&i.item,Item::Message{role: Role::Assistant,text,..} if text == "host text")).count(), usize::from(status == TurnStatus::Completed));
        assert!(!turn.items.iter().any(
            |i| matches!(&i.item,Item::Message{text,..} if text == "notification text")
                || matches!(&i.item, Item::Tool { .. })
        ));
        let delivery = store.queued_inputs(id)?.last().unwrap().status;
        assert_eq!(
            delivery,
            match receipt {
                InputReceipt::Duplicate => InputStatus::Duplicate,
                InputReceipt::Cancelled => InputStatus::Cancelled,
                _ => InputStatus::Sent,
            }
        );
        if matches!(mode, "notification" | "joined" | "joined by model") {
            let notice = a.notifications().last().unwrap();
            assert!(notice.activity.items.iter().any(|i| !i.source.path.is_empty() && matches!(&i.item,Item::Message{text,..} if text == "child notification")));
            assert_eq!(
                notice.joined_input,
                (mode != "notification").then_some(input)
            );
            assert_eq!(
                notice.activity.status,
                if mode != "notification" {
                    TurnStatus::Completed
                } else {
                    TurnStatus::Failed
                }
            );
            assert!(
                notice
                    .activity
                    .items
                    .iter()
                    .any(|i| matches!(&i.item, Item::Tool{activity,..} if activity.finished))
            );
        }
    }
    let mut raw = chat.subscribe(None)?;
    for mode in [
        "late abort",
        "abort unsupported",
        "abort started",
        "interrupt notification",
    ] {
        let input = Uuid::new_v4();
        let cancel = async {
            let awaited = if mode == "abort started" {
                InputReceipt::Started
            } else {
                InputReceipt::Queued
            };
            while let Some(frame) = raw.next().await {
                if mode == "interrupt notification" {
                    if frame.events.contains(&Event::Message {
                        text: "separate pending notification".into(),
                    }) {
                        break;
                    }
                    continue;
                }
                if frame.events.contains(&Event::NativeInput {
                    input_id: input,
                    receipt: awaited,
                }) {
                    break;
                }
            }
            chat.abort().await
        };
        let settle = async {
            let turn = chat.send(input, mode).await?;
            if mode == "interrupt notification" {
                assert_eq!(turn.status, TurnStatus::Completed);
                assert_eq!(
                    chat.history()?
                        .notifications
                        .last()
                        .unwrap()
                        .activity
                        .status,
                    TurnStatus::Aborted
                );
            } else {
                assert_eq!(turn.status, TurnStatus::Aborted, "{mode}");
            }
            if mode == "late abort" {
                let next = chat.send(Uuid::new_v4(), "next").await?;
                assert_eq!(next.status, TurnStatus::Completed);
            }
            kar::Result::Ok(())
        };
        tokio::try_join!(settle, cancel)?;
    }
    let input = Uuid::new_v4();
    let interrupt = async {
        while let Some(frame) = raw.next().await {
            if frame.events.contains(&Event::Message {
                text: "notification in progress".into(),
            }) {
                break;
            }
        }
        let initial = store.feed_snapshot(id)?;
        assert_eq!(
            initial
                .history
                .notifications
                .last()
                .unwrap()
                .activity
                .status,
            TurnStatus::Running
        );
        let path = temp.path().join("crashed");
        let crashed = ChatStore::open(&path)?;
        for suffix in ["karchat", "lock"] {
            let file = format!("{id}.{suffix}");
            std::fs::copy(temp.path().join("chats").join(&file), path.join(&file))?;
        }
        let mut offline = ChatReplica::from_snapshot(initial);
        let mut updates = crashed.follow(id, Some(offline.cursor()))?;
        let saved = crashed.feed_snapshot(id)?;
        assert_eq!(
            saved.history.turns.last().unwrap().status,
            TurnStatus::Interrupted
        );
        assert_eq!(
            saved.history.notifications.last().unwrap().activity.status,
            TurnStatus::Interrupted
        );
        while offline.cursor() != saved.cursor {
            assert!(offline.apply(updates.recv().await?)?);
        }
        assert_eq!(offline.notifications(), saved.history.notifications);
        assert_eq!(crashed.feed_snapshot(id)?.cursor, saved.cursor);
        chat.abort().await
    };
    let (cancelled, ()) = tokio::try_join!(chat.send(input, "crash window"), interrupt)?;
    assert_eq!(
        cancelled.native_input_receipt,
        Some(InputReceipt::Cancelled)
    );
    let next = Uuid::new_v4();
    store.enqueue(id, next, "after notification")?;
    chat.drain_queue().await?;
    let history = chat.history()?;
    let continued = history.turns.last().unwrap();
    assert_eq!(
        (continued.id, continued.status),
        (next, TurnStatus::Completed)
    );
    assert_eq!(
        history.notifications.last().unwrap().activity.status,
        TurnStatus::Aborted
    );
    let owner = Uuid::new_v4();
    let next = Uuid::new_v4();
    let control = async {
        while let Some(r) = raw.next().await {
            if r.events.contains(&Event::Message {
                text: "ready for steering".into(),
            }) {
                break;
            }
        }
        chat.enqueue(next, "after cancelled steer")?;
        chat.steer("refused steer").await?;
        chat.steer("held steer").await?;
        while let Some(r) = raw.next().await {
            if r.span_id == Some(owner.to_string())
                && r.events
                    .iter()
                    .any(|e| matches!(e, Event::TurnEnded { .. }))
            {
                break;
            }
        }
        assert!(matches!(
            chat.steer("after the end").await,
            Err(kar::Error::Refused(_))
        ));
        assert_eq!(
            store.queued_inputs(id)?.last().unwrap().status,
            InputStatus::Pending
        );
        assert!(matches!(
            chat.rollback(
                kar::chat::CheckpointPoint {
                    turn: owner,
                    before: false
                },
                false
            )
            .await,
            Err(kar::Error::Busy)
        ));
        assert!(matches!(chat.fork_at(owner).await, Err(kar::Error::Busy)));
        let checkpoint = store.feed_snapshot(id)?;
        let path = temp.path().join("held-steer-crash");
        let crashed = ChatStore::open(&path)?;
        for ext in ["karchat", "lock"] {
            let file = format!("{id}.{ext}");
            std::fs::copy(temp.path().join("chats").join(&file), path.join(file))?;
        }
        let mut stale = ChatReplica::from_snapshot(checkpoint);
        let mut changes = crashed.follow(id, Some(stale.cursor()))?;
        assert_eq!(stale.turns().last().unwrap().status, TurnStatus::Completed);
        assert!(
            stale
                .turns()
                .last()
                .unwrap()
                .steered_inputs
                .values()
                .any(|s| *s == kar::SteerStatus::Queued)
        );
        assert!(stale.apply(changes.recv().await?)?);
        let settled = crashed.feed_snapshot(id)?;
        assert_eq!(stale.cursor(), settled.cursor);
        assert_eq!(stale.turns(), settled.history.turns);
        assert!(
            stale
                .turns()
                .last()
                .unwrap()
                .steered_inputs
                .values()
                .any(|s| *s == kar::SteerStatus::Unconfirmed)
        );
        chat.abort().await?;
        chat.drain_queue().await
    };
    let (original, ()) = tokio::try_join!(chat.send(owner, "steer owner"), control)?;
    assert_eq!(original.status, TurnStatus::Completed);
    let history = chat.history()?;
    let original = history.turns.iter().find(|t| t.id == owner).unwrap();
    assert_eq!(original.steered_inputs.len(), 2);
    assert!(
        original
            .steered_inputs
            .values()
            .any(|s| *s == kar::SteerStatus::Cancelled)
    );
    assert!(
        original
            .steered_inputs
            .values()
            .any(|s| *s == kar::SteerStatus::Refused)
    );
    assert_eq!(history.turns.last().unwrap().id, next);
    assert_eq!(history.turns.last().unwrap().status, TurnStatus::Completed);
    let steer = async {
        while let Some(r) = raw.next().await {
            if r.events.contains(&Event::Message {
                text: "ready for boundary".into(),
            }) {
                break;
            }
        }
        chat.steer("delayed start receipt").await
    };
    let (boundary, ()) = tokio::try_join!(chat.send(Uuid::new_v4(), "boundary owner"), steer)?;
    std::fs::write(temp.path().join("original-checkpointed"), "")?;
    let delayed = *boundary.steered_inputs.keys().next().unwrap();
    while let Some(r) = raw.next().await {
        if r.span_id.as_deref() == Some(&delayed.to_string())
            && r.events
                .iter()
                .any(|e| matches!(e, Event::TurnEnded { .. }))
        {
            break;
        }
    }
    let history = chat.history()?;
    let completed = history.turns.last().unwrap();
    assert_eq!(completed.id, delayed);
    let after = completed.checkpoint.as_ref().unwrap();
    assert_eq!(
        Some(&after.before_tree),
        boundary.checkpoint.as_ref().unwrap().after_tree.as_ref()
    );
    assert!(
        after
            .files
            .iter()
            .any(|file| file.path == std::path::Path::new("between-native-frames.txt"))
    );
    let notice_count = history.notifications.len();
    let prior = chat.send(Uuid::new_v4(), "idle notice").await?;
    assert_eq!(prior.status, TurnStatus::Completed);
    loop {
        let update = feed.recv(a.cursor()).await?;
        a.apply(update)?;
        let finished = a.notifications().get(notice_count).is_some_and(|n| {
            n.activity.items.iter().any(|item| {
                matches!(&item.item, Item::Tool { id, activity, .. } if id == "idle-read" && activity.finished)
            })
        });
        if finished {
            break;
        }
    }
    let idle = &a.notifications()[notice_count];
    assert_eq!(idle.activity.status, TurnStatus::Running);
    assert_eq!(idle.joined_input, None);
    assert!(idle.activity.items.iter().any(
        |item| matches!(&item.item, Item::Message{text,..} if text == "independent idle reply")
    ));
    assert_eq!(a.turns().last().unwrap().id, prior.id);
    assert!(matches!(
        chat.send(Uuid::new_v4(), "wait for notice").await,
        Err(kar::Error::Busy)
    ));
    assert!(matches!(
        chat.start_compaction().await,
        Err(kar::Error::Busy)
    ));
    let next = Uuid::new_v4();
    assert_eq!(
        chat.enqueue(next, "after idle notice")?.status,
        InputStatus::Pending
    );
    chat.abort().await?;
    chat.drain_queue().await?;
    let history = chat.history()?;
    assert_eq!(
        history.notifications.last().unwrap().activity.status,
        TurnStatus::Aborted
    );
    assert_eq!(history.notifications.len(), notice_count + 1);
    assert!(
        history
            .notifications
            .last()
            .unwrap()
            .activity
            .native_result_id
            .is_some()
    );
    let delivered = history.turns.last().unwrap();
    assert_eq!(delivered.id, next);
    assert_eq!(delivered.status, TurnStatus::Completed);
    assert!(
        delivered
            .items
            .iter()
            .any(|item| matches!(&item.item, Item::Message{text,..} if text == "host text"))
    );
    chat.close().await?;
    let old_notices = store.feed_snapshot(id)?.history.notifications.len();
    let reopened = store.resume_with(id, |o| o.env = opts.env.clone()).await?;
    let again = reopened.send(Uuid::new_v4(), "replay after reopen").await?;
    assert_eq!(again.status, TurnStatus::Completed);
    assert!(again.items.iter().any(
        |i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text == "host text")
    ));
    reopened.close().await?;
    let snapshot = store.feed_snapshot(id)?;
    assert_eq!(snapshot.history.notifications.len(), old_notices);
    let mut resumed = HttpChatFeed::new(&url)?;
    while b.cursor() != snapshot.cursor {
        let e = resumed.recv(b.cursor()).await?;
        assert!(b.apply(e.clone())?);
        assert!(!b.apply(e)?);
    }
    assert_eq!(b.notifications(), snapshot.history.notifications);
    assert_eq!(b.turns(), snapshot.history.turns);
    let history = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .env("KAR_CHAT_DIR", temp.path().join("chats"))
        .args(["chat", "history", "--chat", &id.to_string()])
        .output()
        .await?;
    assert!(history.status.success());
    let history = String::from_utf8(history.stdout).unwrap();
    assert!(history.contains("Native input Duplicate"));
    assert!(history.contains("Native notification joined input"));
    assert!(history.contains("child notification"));
    http.abort();
    Ok(())
}
