use super::emit;
use kar::{
    Delivery, Error, Event, Outcome, SessionOptions,
    chat::{ChatReplica, ChatStore, Item, Role, TurnStatus},
};
use serde_json::{Value, json};
use std::{io::BufRead, sync::Arc};
use uuid::Uuid;

pub(super) fn peer(mode: &str, args: &[String]) {
    let version = if mode == "legacy" { "1.19.5" } else { "2.2.7" };
    if args.get(1).is_some_and(|a| a == "--version") {
        println!("{version}");
        return;
    }
    let mut prompt = Value::Null;
    let mut late = Value::Null;
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = &v["id"];
        let reply = |id: &Value, data: Value| emit(json!({"jsonrpc":"2.0","id":id,"result":data}));
        match v["method"].as_str().unwrap() {
            "initialize" => reply(
                id,
                json!({"protocolVersion":1,"agentInfo":{"name":"opencode","version": version},
                "_meta":{"steering":{"supported": mode=="enabled"}},"agentCapabilities":{"loadSession":true,
                "promptCapabilities":{"image":mode!="disabled"}}}),
            ),
            "session/new" => reply(id, json!({"sessionId":"steering-session"})),
            "session/prompt" => {
                if mode == "legacy" && v["params"]["prompt"][0]["text"] == "reject late" {
                    reply(&prompt, json!({"stopReason":"end_turn"}));
                    emit(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"rejected steer"}}),
                    );
                    continue;
                }
                prompt = id.clone();
                if !late.is_null() {
                    reply(&late, json!({"outcome":"injected"}));
                    late = Value::Null;
                }
                if v["params"]["prompt"][0]["text"] == "fallback" {
                    reply(id, json!({"stopReason":"end_turn"}));
                } else {
                    emit(
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"steering-session",
                        "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"waiting"}}}}),
                    );
                }
            }
            "_session/steering" => {
                assert_eq!(mode, "enabled");
                assert_eq!(v["params"].as_object().unwrap().len(), 2);
                assert_eq!(v["params"]["sessionId"], "steering-session");
                if v["params"]["prompt"][0]["type"] == "image" {
                    assert_eq!(
                        v["params"]["prompt"],
                        json!([{"type":"image","mimeType":"image/gif","data":"R0lGODlh"}])
                    );
                    reply(id, json!({"outcome":"injected"}));
                    continue;
                }
                match v["params"]["prompt"][0]["text"].as_str().unwrap() {
                    "unknown" => reply(id, json!({"outcome":"future"})),
                    "error" => emit(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"refused"}}),
                    ),
                    "fallback" => reply(
                        id,
                        json!({"outcome":"promptRequired","reason":"noRunningTurn"}),
                    ),
                    "late" => {
                        late = id.clone();
                        reply(&prompt, json!({"stopReason":"end_turn"}));
                    }
                    _ => reply(id, json!({"outcome":"injected"})),
                }
            }
            "session/cancel" => reply(&prompt, json!({"stopReason":"cancelled"})),
            other => panic!("unexpected {other}"),
        }
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let dir = tempfile::tempdir()?;
    let work = dir.path().join("work");
    std::fs::create_dir(&work)?;
    let mut opts = SessionOptions::new(work);
    opts.env
        .insert("OAR_TEST_OPENCODE_STEERING".into(), "enabled".into());
    opts.env
        .insert("XDG_DATA_HOME".into(), dir.path().join("data").into());
    let mut rt = kar::runtime("opencode")?;
    rt.program = std::env::current_exe()?;
    let s = rt.open(opts.clone()).await?;
    assert!(s.capabilities().steering);
    assert!(matches!(s.steer("idle").await, Err(Error::Refused(_))));
    let first = s.prompt("hold").await?;
    assert!(matches!(s.deliver("accepted").await?, Delivery::Steered));
    s.steer_images("", vec![kar::InputImage::from_bytes(b"GIF89a")?])
        .await?;
    assert!(matches!(s.steer("error").await, Err(Error::Refused(_))));
    assert!(matches!(s.steer("unknown").await, Err(Error::Protocol(_))));
    let Delivery::Queued { turn: fallback, .. } = s.deliver("fallback").await? else {
        panic!("not queued")
    };
    s.abort().await?;
    assert_eq!(first.wait().await?, Outcome::Aborted);
    assert_eq!(fallback.wait().await?, Outcome::Completed);
    assert_eq!(
        s.records()
            .iter()
            .flat_map(|r| &r.events)
            .filter(|e| matches!(
                e,
                Event::SteeringInput {
                    status: kar::SteerStatus::Accepted,
                    ..
                }
            ))
            .count(),
        2
    );
    s.close().await?;

    let store = ChatStore::open(dir.path().join("chats"))?;
    let chat = Arc::new(
        store
            .create("opencode", Some(&rt.program), opts.clone())
            .await?,
    );
    let id = chat.info().id;
    let snapshot = store.feed_snapshot(id)?;
    let mut feed = store.follow(id, Some(snapshot.cursor))?;
    let mut replica = ChatReplica::from_snapshot(snapshot);
    let start = |text: &'static str| {
        let chat = chat.clone();
        tokio::spawn(async move { chat.send(Uuid::new_v4(), text).await })
    };
    let first = start("same");
    loop {
        let event = feed.recv().await?;
        replica.apply(event)?;
        if replica.turns().last().is_some_and(|t| {
            t.items.iter().any(|i| {
                matches!(
                    i.item,
                    Item::Message {
                        role: Role::Assistant,
                        ..
                    }
                )
            })
        }) {
            break;
        }
    }
    chat.steer("same").await?;
    let saved = serde_json::to_vec(&store.feed_snapshot(id)?)?;
    let sending = chat.clone();
    let late = tokio::spawn(async move { sending.steer("late").await });
    assert_eq!(first.await.unwrap()?.status, TurnStatus::Completed);
    let next = start("next");
    late.await.unwrap()?;
    chat.abort().await?;
    assert_eq!(next.await.unwrap()?.status, TurnStatus::Aborted);
    let tip = store.feed_snapshot(id)?;
    while replica.cursor() != tip.cursor {
        let event = feed.recv().await?;
        assert!(replica.apply(event.clone())?);
        assert!(!replica.apply(event)?);
    }
    assert_eq!(replica.turns(), tip.history.turns);
    let users = |n: usize| {
        replica.turns()[n]
            .items
            .iter()
            .filter_map(|i| match &i.item {
                Item::Message {
                    role: Role::User,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(users(0), ["same", "same", "late"]);
    assert_eq!(users(1), ["next"]);
    assert_eq!(replica.turns()[0].steered_inputs.len(), 2);
    assert!(
        replica.turns()[0]
            .steered_inputs
            .values()
            .all(|s| *s == kar::SteerStatus::Accepted)
    );
    Arc::try_unwrap(chat).ok().unwrap().close().await?;
    let reopened = ChatStore::open(dir.path().join("chats"))?;
    let mut second = ChatReplica::from_snapshot(serde_json::from_slice(&saved)?);
    let mut resumed = reopened.follow(id, Some(second.cursor()))?;
    let current = reopened.feed_snapshot(id)?;
    while second.cursor() != current.cursor {
        let event = resumed.recv().await?;
        assert!(second.apply(event.clone())?);
        assert!(!second.apply(event)?);
    }
    assert_eq!(second.turns(), replica.turns());

    opts.env
        .insert("OAR_TEST_OPENCODE_STEERING".into(), "disabled".into());
    let disabled = rt.open(opts.clone()).await?;
    assert!(!disabled.capabilities().steering);
    let held = disabled.prompt("hold").await?;
    assert!(matches!(
        disabled
            .steer_images("unsupported", vec![kar::InputImage::from_bytes(b"GIF89a")?])
            .await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        disabled.steer("unsupported").await,
        Err(Error::Unsupported(_))
    ));
    disabled.abort().await?;
    held.wait().await?;
    disabled.close().await?;
    opts.env
        .insert("OAR_TEST_OPENCODE_STEERING".into(), "legacy".into());
    let mut extra = opts.clone();
    extra
        .settings
        .additional_directories
        .push(dir.path().to_owned());
    assert!(matches!(
        rt.open(extra).await,
        Err(Error::Unsupported(reason)) if reason.contains("additionalDirectories")
    ));
    let legacy = Arc::new(store.create("opencode", Some(&rt.program), opts).await?);
    let snapshot = store.feed_snapshot(legacy.info().id)?;
    let mut stream = store.follow(legacy.info().id, Some(snapshot.cursor))?;
    let mut view = ChatReplica::from_snapshot(snapshot);
    let owner = legacy.clone();
    let running = tokio::spawn(async move { owner.send(Uuid::new_v4(), "original").await });
    loop {
        view.apply(stream.recv().await?)?;
        if view.turns().last().is_some_and(|t| {
            t.items.iter().any(|i| {
                matches!(
                    i.item,
                    Item::Message {
                        role: Role::Assistant,
                        ..
                    }
                )
            })
        }) {
            break;
        }
    }
    legacy.steer("reject late").await?;
    assert_eq!(running.await.unwrap()?.status, TurnStatus::Completed);
    let mut states = Vec::new();
    loop {
        view.apply(stream.recv().await?)?;
        let turn = &view.turns()[0];
        if let Some((_, state)) = turn.steered_inputs.first_key_value() {
            if states.last() != Some(state) {
                states.push(*state);
            }
            if *state == kar::SteerStatus::Refused {
                break;
            }
        }
    }
    assert_eq!(
        states,
        [kar::SteerStatus::Submitted, kar::SteerStatus::Refused]
    );
    assert_eq!(view.turns().len(), 1);
    assert_eq!(
        view.turns()[0]
            .items
            .iter()
            .filter(
                |i| matches!(&i.item,Item::Message{role:Role::User,text,..} if text=="reject late")
            )
            .count(),
        1
    );
    let tip = store.feed_snapshot(legacy.info().id)?;
    while view.cursor() != tip.cursor {
        view.apply(stream.recv().await?)?;
    }
    assert_eq!(view.turns(), tip.history.turns);
    Arc::try_unwrap(legacy).ok().unwrap().close().await?;
    Ok(())
}
