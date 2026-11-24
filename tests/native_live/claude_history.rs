use super::{interrupted_copy, respond_with_tool, resume_prompts::settings, sse::SseClient};
use kar::{
    Event,
    chat::{self, ChatEdit, ChatReplica, ChatStore, ChatUpdate, TurnStatus},
};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "real Claude Code, isolated native transcript, local model and two HTTP clients"]
async fn replay_native_blocks_after_controller_crash() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut opts = settings(
        "claude",
        &root,
        &format!("http://{}", model.local_addr().unwrap()),
    );
    let file = opts.cwd.join("note.txt");
    opts.env
        .insert("KAR_TEST_SECRET".into(), "should-stay-private".into());
    fs::write(&file, "value from the native read: should-stay-private").unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let server = tokio::spawn(async move {
        while let Ok((socket, _)) = model.accept().await {
            let s = socket.into_std().unwrap();
            s.set_nonblocking(false).unwrap();
            let (file, calls) = (file.clone(), log.clone());
            tokio::task::spawn_blocking(move || respond_with_tool(s, Some(&file), Some(&calls)));
        }
    });
    let source = root.join("store");
    let store = ChatStore::open(&source).unwrap();
    let chat = store.create("claude", None, opts.clone()).await.unwrap();
    let id = chat.info().id;
    let opening = store.feed_snapshot(id).unwrap().cursor;
    let key = Uuid::new_v4();
    let prompt = "oar-claude-recover orange-original";
    let original = chat.send(key, prompt).await.unwrap();
    assert_eq!(original.status, TurnStatus::Completed);
    assert_eq!(
        original.items.len(),
        5,
        "user, tool, reasoning and both model text blocks"
    );
    assert!(original.items.iter().any(|item| matches!(&item.item,chat::Item::Tool{id,activity} if id=="toolu_recover" && activity.result.as_ref().and_then(|r|r.text()).is_some_and(|s|s.contains("value from the native read")))));
    assert!(
        serde_json::to_string(&original.items)
            .unwrap()
            .contains("[redacted]")
    );
    let mut feed = store.follow(id, Some(opening)).unwrap();
    let tip = store.feed_snapshot(id).unwrap().cursor;
    let mut cuts = BTreeMap::new();
    while feed.cursor() != tip {
        let e = feed.recv().await.unwrap();
        let phase = match &e.update {
            ChatUpdate::Begin { .. } => Some("begin"),
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|edit| {
                    matches!(
                        edit,
                        ChatEdit::Activity {
                            event: Event::ToolInputDelta { .. }
                        }
                    )
                }) =>
            {
                Some("args")
            }
            ChatUpdate::Frame { frame, .. }
                if frame.edits.iter().any(|edit| {
                    matches!(
                        edit,
                        ChatEdit::Text {
                            final_text: false,
                            ..
                        }
                    )
                }) =>
            {
                Some("text")
            }
            _ => None,
        };
        if let Some(label) = phase {
            cuts.entry(label).or_insert(e.cursor);
        }
    }
    assert_eq!(cuts.len(), 3);
    // A later native input has the same text (and the fixture reuses the model
    // message id). Only the ancestry of the first SDK input UUID is relevant.
    chat.send(Uuid::new_v4(), prompt).await.unwrap();
    chat.close().await.unwrap();
    let before = calls.lock().unwrap().len();
    let begin = cuts["begin"];
    for (label, cut) in cuts {
        let store = interrupted_copy(&source, &root.join(label), id, cut);
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", http.local_addr().unwrap());
        let router = chat::event_router(store.clone(), id);
        let web = tokio::spawn(async move { axum::serve(http, router).await.unwrap() });
        let mut a = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        let mut b = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
        let mut connected = SseClient::open(&url, &a.cursor().to_string(), None).await;
        for attempt in 0..2 {
            let reopened = store
                .resume_with(id, |o| o.env = opts.env.clone())
                .await
                .unwrap();
            let limit = reopened.snapshot().cursor.unwrap().after_seq;
            let mut records = reopened.subscribe(None).unwrap();
            let mut repairs = 0;
            loop {
                let record =
                    tokio::time::timeout(std::time::Duration::from_secs(5), records.next())
                        .await
                        .unwrap()
                        .unwrap();
                repairs += usize::from(record.native["type"] == "claude/transcript");
                if record.seq >= limit {
                    break;
                }
            }
            assert_eq!(repairs, usize::from(attempt == 0));
            let actual = reopened.send(key, prompt).await.unwrap();
            assert_eq!(
                actual.status,
                TurnStatus::Interrupted,
                "missing SDK result receipt"
            );
            assert!(actual.native_result_id.is_none());
            assert_eq!(actual.items, original.items, "{label}");
            reopened.close().await.unwrap();
        }
        let tip = store.feed_snapshot(id).unwrap().cursor;
        let mut reconnected =
            SseClient::open(&url, &opening.to_string(), Some(&b.cursor().to_string())).await;
        for (replica, client) in [(&mut a, &mut connected), (&mut b, &mut reconnected)] {
            let mut count = 0;
            while replica.cursor() != tip {
                let event = client.next().await;
                count += usize::from(matches!(event.update, ChatUpdate::Reconciled { .. }));
                assert!(replica.apply(event.clone()).unwrap());
                assert!(!replica.apply(event).unwrap());
            }
            assert_eq!(count, 1, "{label}");
            assert_eq!(replica.turns()[0].items, original.items);
        }
        let durable = fs::read_to_string(root.join(label).join(format!("{id}.oarchat"))).unwrap();
        assert!(!durable.contains("should-stay-private"));
        web.abort();
    }
    // The vendor can persist end_turn on each text envelope. Expose a stable
    // prefix of the real file, then expose the remaining block on a later open.
    let native = store.history(id).unwrap().chat.native_id;
    let projects = root.join("home/projects");
    let transcript = fs::read_dir(projects)
        .unwrap()
        .flatten()
        .map(|d| d.path().join(format!("{native}.jsonl")))
        .find(|p| p.is_file())
        .unwrap();
    let full = fs::read_to_string(&transcript).unwrap();
    let mut prefix = String::new();
    for line in full.lines() {
        prefix.push_str(line);
        prefix.push('\n');
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        if row["type"] == "assistant"
            && row["message"]["stop_reason"] == "end_turn"
            && row
                .pointer("/message/content/0/text")
                .and_then(|t| t.as_str())
                == Some("rust-wire-ok")
        {
            break;
        }
    }
    assert!(prefix.len() < full.len());
    fs::write(&transcript, &prefix).unwrap();
    let store = interrupted_copy(&source, &root.join("growing"), id, begin);
    for length in [original.items.len() - 1, original.items.len()] {
        let session = store
            .resume_with(id, |o| o.env = opts.env.clone())
            .await
            .unwrap();
        let turn = session.send(key, prompt).await.unwrap();
        assert_eq!(turn.items, original.items[..length]);
        assert_eq!(turn.status, TurnStatus::Interrupted);
        session.close().await.unwrap();
        fs::write(&transcript, &full).unwrap();
    }
    assert_eq!(calls.lock().unwrap().len(), before);
    server.abort();
}
