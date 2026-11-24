use super::emit;
use kar::{Delivery, Error, Event, Outcome, SessionOptions, SteerStatus};
use serde_json::{Value, json};
use std::io::BufRead;

fn reply(id: &Value, reason: &str) {
    emit(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":reason}}));
}
fn reject(id: &Value, code: i64) {
    emit(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":"provider test failure"}}));
}

pub(super) fn peer(version: &str) {
    let mut prompts = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = &v["id"];
        match v["method"].as_str().unwrap() {
            "initialize" => emit(
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,
                "agentInfo":{"name":"Antigravity ACP Server","version":version},
                "agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{}},"promptCapabilities":{"image":true}}}}),
            ),
            "session/new" => {
                emit(json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"agy-root"}}))
            }
            "session/list" => emit(
                json!({"jsonrpc":"2.0","id":id,"result":{"sessions":[{"sessionId":"agy-root","cwd":std::env::current_dir().unwrap()}]}}),
            ),
            "session/load" => emit(json!({"jsonrpc":"2.0","id":id,"result":{}})),
            "session/prompt" => {
                assert_eq!(v["params"]["sessionId"], "agy-root");
                let body = v["params"]["prompt"].as_array().unwrap();
                match body.last().unwrap()["text"].as_str().unwrap() {
                    "hold" => {
                        assert!(prompts.is_empty());
                        prompts.push(id.clone());
                    }
                    "one" => {
                        assert_eq!(prompts.len(), 1);
                        prompts.push(id.clone());
                    }
                    "replace" => {
                        assert_eq!(prompts.len(), 2);
                        assert_eq!(
                            body[0],
                            json!({"type":"image","mimeType":"image/gif","data":"R0lGODlh"})
                        );
                        // The last request settles first. The obsolete request fails last.
                        reply(id, "end_turn");
                        reply(&prompts[0], "cancelled");
                        reject(&prompts[1], -32603);
                        prompts.clear();
                    }
                    "invalid-first" => reject(id, -32602),
                    "invalid-last" => {
                        reply(&prompts.remove(0), "end_turn");
                        reject(id, -32602);
                    }
                    "worker" => {
                        reply(&prompts.remove(0), "end_turn");
                        reject(id, -32603);
                    }
                    "not-admitted" => reject(id, -32603),
                    "abort-me" => {
                        reply(&prompts.remove(0), "end_turn");
                        prompts.push(id.clone());
                    }
                    "next" => {
                        assert!(prompts.is_empty());
                        reply(id, "end_turn");
                    }
                    "exit" => return,
                    bounded if bounded.len() == 185_000 => reply(id, "end_turn"),
                    other => panic!("unexpected {other}"),
                }
            }
            "session/cancel" => reject(&prompts.remove(0), -32603),
            other => panic!("unexpected {other}"),
        }
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut opts = SessionOptions::new(dir.path());
    let mut rt = kar::runtime("antigravity")?;
    rt.program = std::env::current_exe()?;
    opts.env.insert("OAR_TEST_AGY_STEER".into(), "1.4.4".into());
    let old = rt.open(opts.clone()).await?;
    assert!(!old.capabilities().steering);
    old.close().await?;
    opts.env
        .insert("OAR_TEST_AGY_STEER".into(), "1.5.15".into());
    let s = rt.open(opts.clone()).await?;
    assert_eq!(
        s.prompt("é".repeat(92_500)).await?.wait().await?,
        Outcome::Completed
    );
    let over = "é".repeat(92_501);
    assert!(matches!(
        s.prompt(&over).await,
        Err(Error::InputTooLarge {
            bytes: 185_002,
            limit: 185_000
        })
    ));
    assert!(matches!(
        s.queue(&over).await,
        Err(Error::InputTooLarge { .. })
    ));
    assert!(s.capabilities().steering);
    assert!(matches!(s.steer("idle").await, Err(Error::Refused(_))));
    let events = || {
        s.records()
            .iter()
            .flat_map(|r| r.events.clone())
            .collect::<Vec<_>>()
    };
    let first = s.prompt("hold").await?;
    assert!(matches!(
        s.steer(&over).await,
        Err(Error::InputTooLarge { .. })
    ));
    assert!(matches!(s.deliver("one").await?, Delivery::Steered));
    let (_, next) = s.queue("next").await?;
    s.steer_images("replace", vec![kar::InputImage::from_bytes(b"GIF89a")?])
        .await?;
    assert_eq!(first.wait().await?, Outcome::Completed);
    assert_eq!(next.wait().await?, Outcome::Completed);
    assert_eq!(
        events()
            .iter()
            .filter(|e| matches!(e, Event::TurnEnded { .. }))
            .count(),
        3
    );
    assert!(
        !events()
            .iter()
            .any(|e| matches!(e, Event::TurnFailure { .. }))
    );
    for steer in ["invalid-first", "not-admitted"] {
        let turn = s.prompt("hold").await?;
        s.steer(steer).await?;
        assert!(matches!(s.prompt("busy").await, Err(Error::Busy)));
        s.steer("invalid-last").await?;
        assert_eq!(turn.wait().await?, Outcome::Completed);
    }
    let worker = s.prompt("hold").await?;
    s.steer("worker").await?;
    let report = worker.report().await?;
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(report.failure, s.snapshot().last_failure);
    assert_eq!(
        s.snapshot().last_failure.unwrap().class,
        kar::FailureClass::Unknown
    );
    let cancel = s.prompt("hold").await?;
    s.steer("abort-me").await?;
    s.abort().await?;
    assert_eq!(cancel.wait().await?, Outcome::Aborted);
    let died = s.prompt("hold").await?;
    s.steer("exit").await?;
    assert!(died.wait().await.is_err());
    let records = events();
    for (text, status) in [
        ("exit", SteerStatus::Unconfirmed),
        ("one", SteerStatus::Accepted),
        ("replace", SteerStatus::Accepted),
        ("worker", SteerStatus::Accepted),
        ("abort-me", SteerStatus::Accepted),
        ("invalid-first", SteerStatus::Refused),
        ("invalid-last", SteerStatus::Refused),
        ("not-admitted", SteerStatus::Refused),
    ] {
        assert!(records.iter().any(|e| matches!(e,Event::SteeringInput{text:t,status:s,..} if t == text && *s == status)), "{text}");
    }
    assert_eq!(
        records
            .iter()
            .filter(|e| matches!(e, Event::InputDropped { .. }))
            .count(),
        4
    );
    use kar::chat::{ChatStore, InputStatus};
    let root = dir.path().join("chats");
    let store = ChatStore::open(&root)?;
    let chat = store
        .create("antigravity", Some(&rt.program), opts.clone())
        .await?;
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    let pending = uuid::Uuid::new_v4();
    assert!(matches!(
        store.enqueue(id, pending, &over),
        Err(Error::InputTooLarge { .. })
    ));
    assert!(matches!(
        chat.send(pending, &over).await,
        Err(Error::InputTooLarge { .. })
    ));
    chat.close().await?;
    let next = uuid::Uuid::new_v4();
    let old = json!({"chat":id,"native_id":native,"entries":[
        {"id":pending,"text":over,"withdrawn":false},{"id":next,"text":"next","withdrawn":false}]});
    std::fs::write(root.join(format!("{id}.inputs")), serde_json::to_vec(&old)?)?;
    let chat = store.resume_with(id, |o| o.env = opts.env.clone()).await?;
    chat.drain_queue().await?;
    let inputs = store.queued_inputs(id)?;
    assert_eq!(inputs[0].status, InputStatus::TooLarge);
    assert_eq!(inputs[1].status, InputStatus::Sent);
    assert_eq!(chat.history()?.turns.len(), 1);
    chat.close().await?;
    Ok(())
}
