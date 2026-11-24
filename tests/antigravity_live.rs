//! Antigravity CLI against a Rust loopback Gemini service, with an isolated user profile.
use axum::{
    Json, Router,
    extract::State,
    http::{Uri, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use kar::{
    Event, SessionOptions, SteerStatus,
    chat::{ChatReplica, ChatStore, HttpChatFeed, Item, Role, TurnStatus},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Notify};
use uuid::Uuid;

#[derive(Default)]
struct Gemini {
    entered: Notify,
    release: Notify,
    conversations: Mutex<Vec<Value>>,
}
async fn generate(State(model): State<Arc<Gemini>>, uri: Uri, Json(body): Json<Value>) -> Response {
    let contents = &body["contents"];
    let part = if !contents.to_string().contains("AGY_ORIGINAL")
        || body["systemInstruction"]
            .to_string()
            .contains("conversation title generator")
    {
        json!({"text":"Local test title"})
    } else {
        let first = {
            let mut calls = model.conversations.lock().unwrap();
            calls.push(contents.clone());
            calls.len() == 1
        };
        if first {
            model.entered.notify_one();
            model.release.notified().await;
            let tools = body["tools"].as_array().unwrap();
            let name = tools
                .iter()
                .filter_map(|t| t["functionDeclarations"].as_array())
                .flatten()
                .filter_map(|t| t["name"].as_str())
                .find(|s| s.ends_with("_local_list"))
                .expect("Antigravity did not offer the injected MCP list tool");
            json!({"functionCall":{"name":name,"args":{"toolSummary":"List local test agents"}}})
        } else {
            json!({"text":"AGY_COMPLETE"})
        }
    };
    let reply = json!({"candidates":[{"index":0,"content":{"role":"model","parts":[part]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":4,"totalTokenCount":20}});
    if uri.path().contains("streamGenerateContent") {
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            format!("data: {reply}\n\n"),
        )
            .into_response()
    } else {
        Json(reply).into_response()
    }
}

#[tokio::test]
#[ignore = "requires OAR_ANTIGRAVITY_BIN; uses an isolated CLI and a local Gemini service"]
async fn steer_during_tool_loop_and_resume_both_clients() {
    tokio::time::timeout(Duration::from_secs(50), actual())
        .await
        .expect("Antigravity test exceeded its deadline");
}
async fn actual() {
    let bin = PathBuf::from(std::env::var_os("OAR_ANTIGRAVITY_BIN").unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let cwd = base.join("workspace");
    let profile = home.join(".gemini");
    for path in [
        &cwd,
        &profile.join("config"),
        &profile.join("antigravity-cli"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(
        profile.join("antigravity-cli/settings.json"),
        json!({"modelProvider":"gemini","setupComplete":true,"onboardingComplete":true})
            .to_string(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let model = Arc::new(Gemini::default());
    let app = Router::new()
        .fallback(post(generate))
        .with_state(model.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut opts = SessionOptions::new(cwd);
    opts.approvals = kar::ApprovalMode::Host;
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        opts.env.insert(key.into(), home.clone().into());
    }
    opts.env.insert("GEMINI_HOME".into(), profile.into());
    for (key, val) in [
        ("GOOGLE_GEMINI_BASE_URL", endpoint.as_str()),
        ("GEMINI_API_KEY", "loopback-fixture"),
        ("AGY_CLI_DISABLE_AUTO_UPDATE", "true"),
    ] {
        opts.env.insert(key.into(), val.into());
    }
    opts.mcp_servers.insert(
        "local".into(),
        kar::McpServer::Stdio {
            command: env!("CARGO_BIN_EXE_kar").into(),
            args: vec!["mcp".into(), "--runtimes".into(), "codex".into()],
            env: Default::default(),
        },
    );
    let store = ChatStore::open(base.join("chats")).unwrap();
    let chat = Arc::new(store.create("antigravity", Some(&bin), opts).await.unwrap());
    let id = chat.info().id;
    let mut raw = chat.subscribe(None).unwrap();
    let web = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", web.local_addr().unwrap());
    let router = kar::chat::event_router(store.clone(), id);
    let feed_service = tokio::spawn(async move { axum::serve(web, router).await.unwrap() });
    let mut live = HttpChatFeed::new(&url).unwrap();
    let mut dropping = HttpChatFeed::new(&url).unwrap();
    let mut a = ChatReplica::from_snapshot(live.snapshot().await.unwrap());
    let mut b = ChatReplica::from_snapshot(dropping.snapshot().await.unwrap());
    let owner = chat.clone();
    let turn_id = Uuid::new_v4();
    let mut sending = tokio::spawn(async move { owner.send(turn_id, "AGY_ORIGINAL").await });
    tokio::select! {
        _ = model.entered.notified() => {},
        result = &mut sending => panic!("Original prompt ended before reaching local Gemini: {result:?}"),
    }
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEElEQVR4nGMQdm2HIwbiOABjYw3x5BI1EwAAAABJRU5ErkJggg==";
    let image = kar::InputImage::from_bytes(&STANDARD.decode(png).unwrap()).unwrap();
    chat.steer_images("AGY_ADDED", vec![image]).await.unwrap();
    loop {
        let update = dropping.recv(b.cursor()).await.unwrap();
        b.apply(update).unwrap();
        if b.turns().first().is_some_and(|t| {
            t.steered_inputs
                .values()
                .any(|s| *s == SteerStatus::Submitted)
        }) {
            break;
        }
    }
    drop(dropping);
    while a.cursor() != b.cursor() {
        let event = live.recv(a.cursor()).await.unwrap();
        assert!(a.apply(event).unwrap());
    }
    assert_eq!(a.turns(), b.turns());
    // A separate store ends at exactly this durable prefix, as after a controller crash.
    let stale_dir = base.join("crash-prefix");
    let stale = ChatStore::open(&stale_dir).unwrap();
    for suffix in ["oarchat", "lock"] {
        let file = format!("{id}.{suffix}");
        std::fs::copy(base.join("chats").join(&file), stale_dir.join(&file)).unwrap();
    }
    let recovered = stale.feed_snapshot(id).unwrap();
    let lost = &recovered.history.turns[0];
    assert_eq!(lost.status, TurnStatus::Interrupted);
    assert_eq!(lost.steered_inputs.len(), 1);
    assert!(
        lost.steered_inputs
            .values()
            .all(|s| *s == SteerStatus::Unconfirmed)
    );
    assert_eq!(recovered.cursor, stale.feed_snapshot(id).unwrap().cursor);
    loop {
        let r = raw.next().await.unwrap();
        assert!(
            !r.events
                .iter()
                .any(|e| matches!(e, Event::TurnEnded { .. }))
        );
        if r.native["result"]["stopReason"] == "end_turn" {
            break;
        }
    }
    assert!(!sending.is_finished());
    assert_eq!(
        store.history(id).unwrap().turns[0].status,
        TurnStatus::Running
    );
    model.release.notify_one();
    loop {
        let frame = raw.next().await.unwrap();
        if let Some(request) = chat.snapshot().pending_requests.into_iter().next() {
            assert_eq!(request.method, "session/request_permission");
            assert!(
                request.body.to_string().to_lowercase().contains("list"),
                "Unexpected tool approval"
            );
            chat.decide(&request, kar::PermissionDecision::AllowOnce)
                .await
                .unwrap();
            break;
        }
        assert!(
            !frame
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnEnded { .. })),
            "Native loop ended before tool approval"
        );
    }
    let finished = sending.await.unwrap().unwrap();
    let messages: Vec<_> = finished
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
        .collect();
    assert_eq!(messages, ["AGY_ORIGINAL", "AGY_ADDED"]);
    assert!(finished.checkpoint.as_ref().unwrap().after_tree.is_some());
    assert_eq!(finished.status, TurnStatus::Completed);
    assert_eq!(finished.steered_inputs.len(), 1);
    assert!(
        finished
            .steered_inputs
            .values()
            .all(|s| *s == SteerStatus::Accepted)
    );
    let calls = model.conversations.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].to_string().contains("AGY_ADDED"));
    let last = calls[1].to_string();
    for marker in [
        "AGY_ORIGINAL",
        "AGY_ADDED",
        "functionResponse",
        "unread",
        png,
    ] {
        assert!(last.contains(marker), "Missing {marker} from model history");
    }
    assert!(
        finished
            .items
            .iter()
            .any(|i| matches!(i.item, Item::Tool { .. }))
    );
    assert!(finished.items.iter().any(|i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text.contains("AGY_COMPLETE"))));
    Arc::try_unwrap(chat).ok().unwrap().close().await.unwrap();
    let tip = store.feed_snapshot(id).unwrap();
    let mut reconnected = HttpChatFeed::new(&url).unwrap();
    for (feed, replica) in [(&mut live, &mut a), (&mut reconnected, &mut b)] {
        while replica.cursor() != tip.cursor {
            let event = feed.recv(replica.cursor()).await.unwrap();
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert_eq!(replica.turns(), tip.history.turns);
        assert_eq!(replica.usage(), tip.history.usage);
        assert_eq!(replica.turns().len(), 1);
    }
    feed_service.abort();
    server.abort();
}
