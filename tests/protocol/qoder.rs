use super::{Value, emit, json};
use kar::{CompactionFailure, CompactionStatus, Outcome, Protocol, Runtime, SessionOptions};
use std::{io::BufRead, path::PathBuf, time::Duration};

const CREDENTIAL: &str = "only-the-worker-knows-this";
const PRIVATE_DATA: &str = "private-account-identity";

fn identity(args: &[String]) -> String {
    assert_eq!(
        args.iter()
            .filter(|a| a.as_str() == "--log-level=warn")
            .count(),
        1
    );
    let (flag, id) = args
        .iter()
        .filter_map(|a| a.split_once('='))
        .find(|(key, _)| matches!(*key, "--resume" | "--session-id"))
        .unwrap();
    uuid::Uuid::parse_str(id).unwrap();
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "default"])
    );
    let path = PathBuf::from(id);
    if flag == "--resume" {
        assert!(path.is_file());
    } else {
        assert!(!path.exists());
        std::fs::write(path, b"").unwrap();
    }
    id.into()
}
pub(super) fn peer(version: &str, args: &[String]) {
    let daemon = version == "1.1.83";
    let mut id = if daemon {
        assert_eq!(args, ["--sdk"]);
        String::new()
    } else {
        identity(args)
    };
    let mut compacts = 0;
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let r = &v["request"];
        let method = r["type"].as_str().or_else(|| r["subtype"].as_str());
        if daemon {
            if method.is_some_and(|m| m.starts_with("daemon.")) {
                assert!(v.get("session_id").is_none());
            } else {
                assert_eq!(v["session_id"], id);
            }
        }
        let answer = |body: Value, session: Option<&str>| {
            let mut reply = json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":body}});
            if let Some(s) = session {
                reply["session_id"] = s.into();
            }
            emit(reply);
        };
        match method {
            Some("daemon.initialize") => {
                assert_eq!(r["auth"]["type"], "accessToken");
                answer(
                    json!({"daemon_protocol_version":2,"capabilities":["multi_session_v2","daemon_queries_v1"]}),
                    None,
                );
            }
            Some("daemon.open_session") => {
                let args: Vec<String> = serde_json::from_value(r["worker_args"].clone()).unwrap();
                id = identity(&args);
                answer(json!({}), Some(&id));
            }
            Some("initialize") => {
                emit(
                    json!({"type":"system","subtype":"init","session_id":id,"model":"Native model","diagnostic":CREDENTIAL}),
                );
                emit(json!({"type":"system","subtype":"boot","session_id":id}));
                let account =
                    json!({"account":{"secretToken":CREDENTIAL,"displayName":PRIVATE_DATA}});
                answer(account.clone(), Some(&id));
                answer(account, Some(&id));
            }
            Some("get_settings") => answer(
                json!({"applied":{"effort":"high"},"private":PRIVATE_DATA}),
                Some(&id),
            ),
            Some("get_models" | "daemon.get_models") => {
                assert_eq!(method == Some("daemon.get_models"), daemon);
                if let Ok(strategy) = std::env::var("KAR_FIXTURE_CATALOG_FAILURE")
                    && r["fetchStrategy"] == strategy
                {
                    emit(
                        json!({"type":"control_response","response":{"subtype":"error","request_id":v["request_id"],"error":PRIVATE_DATA}}),
                    );
                    continue;
                }
                let models = if r["fetchStrategy"] == "live" {
                    json!([{"modelId":"selector","value":"selector","displayName":"Native model","serverModel":{"display_name":"Native model"},
                    "efforts":[{"effort":"high"}],"private":PRIVATE_DATA}])
                } else {
                    json!([])
                };
                answer(json!({"models":models}), None);
            }
            Some("stop_task") => {
                assert_eq!(r["task_id"], "helper");
                emit(
                    json!({"type":"system","subtype":"task_notification","session_id":id,"task_id":"helper","status":"killed"}),
                );
                answer(json!({}), Some(&id));
            }
            None if v["type"] == "user" => {
                assert_eq!(v["session_id"], id);
                assert_eq!(v["priority"], "next");
                let input = v["message"]["content"][0]["text"].as_str().unwrap();
                if input == "/compact" {
                    compacts += 1;
                    let life = |state: &str| {
                        emit(
                            json!({"type":"command_lifecycle","session_id":id,"state":state,"command_uuid":v["uuid"]}),
                        )
                    };
                    life("started");
                    if compacts == 1 {
                        emit(
                            json!({"type":"system","subtype":"compact_boundary","session_id":id,"compact_metadata":{"trigger":"manual"}}),
                        );
                    }
                    emit(
                        json!({"type":"result","subtype":"success","session_id":id,"is_error":false}),
                    );
                    life(if compacts == 1 {
                        "cancelled"
                    } else {
                        "completed"
                    });
                } else if input == "foreign" {
                    emit(
                        json!({"type":"assistant","session_id":"another-session","message":{"content":[{"type":"text","text":"foreign-content"}]}}),
                    );
                } else if input == "hold" {
                    emit(
                        json!({"type":"system","subtype":"task_started","session_id":id,"task_id":"helper","task_type":"local_agent"}),
                    );
                    emit(
                        json!({"type":"system","subtype":"task_progress","session_id":id,"task_id":"helper","description":"working"}),
                    );
                } else {
                    let previous = std::fs::read_to_string(&id).unwrap();
                    std::fs::write(&id, input).unwrap();
                    let text = format!("{previous}/{input}");
                    emit(
                        json!({"type":"user","uuid":v["uuid"],"isReplay":true,"session_id":id,"message":v["message"]}),
                    );
                    emit(
                        json!({"type":"assistant","uuid":uuid::Uuid::new_v4(),"session_id":id,"message":{"content":[{"type":"text","text":text}]}}),
                    );
                    emit(
                        json!({"type":"result","session_id":id,"subtype":"success","is_error":false,"usage":{"input_tokens":8,"output_tokens":3,"cache_read_input_tokens":2}}),
                    );
                }
            }
            _ => panic!("unexpected Qoder control"),
        }
    }
}
pub(super) async fn sessions() -> kar::Result<()> {
    for version in ["1.1.78", "1.1.83"] {
        let dir = tempfile::tempdir()?;
        let runtime = Runtime::new("qoder", std::env::current_exe()?, &[], Protocol::Qoder);
        let mut opts = SessionOptions::new(dir.path());
        opts.launch_args.push("--log-level=warn".into());
        opts.env
            .insert("KAR_FIXTURE_QODER_VERSION".into(), version.into());
        opts.env
            .insert("QODER_PERSONAL_ACCESS_TOKEN".into(), "test-pat".into());
        opts.env
            .insert("QODER_SERVICE_ACCOUNT_KEY".into(), "".into());
        opts.env.insert("QODER_JOB_TOKEN".into(), "".into());
        opts.model = Some("selector".into());
        opts.settings.effort = Some("high".into());
        for (failure, expected_ids) in [
            ("cache", None),
            ("live", Some(Vec::new())),
            ("none", Some(vec!["selector".to_owned()])),
        ] {
            let mut bad = opts.clone();
            bad.model = Some("unknown-selector".into());
            bad.env
                .insert("KAR_FIXTURE_CATALOG_FAILURE".into(), failure.into());
            let Err(kar::Error::Open(e)) = runtime.open(bad).await else {
                panic!("expected Qoder model confirmation refusal");
            };
            let serialized = serde_json::to_string(&e)?;
            assert!(!serialized.contains(CREDENTIAL) && !serialized.contains(PRIVATE_DATA));
            let kar::OpenError::ModelConfirmation {
                requested,
                reported,
                catalog_selector_ids,
                cause,
                ..
            } = *e
            else {
                panic!("wrong Qoder error type")
            };
            assert_eq!(requested, "unknown-selector");
            assert_eq!(reported.as_deref(), Some("Native model"));
            assert_eq!(catalog_selector_ids, expected_ids);
            assert_eq!(
                cause
                    .as_ref()
                    .map(|c| c.native["fetchStrategy"].as_str().unwrap()),
                (failure != "none").then_some(failure)
            );
        }
        durable_chat(&runtime, &opts).await?;
        let session = runtime.open(opts.clone()).await?;
        let caps = session.capabilities();
        assert!(caps.resume && caps.manual_compaction && caps.steering && caps.stop_task);
        assert_eq!(session.info()["model"], "Native model");
        assert_eq!(session.effort().as_deref(), Some("high"));
        assert_eq!(session.models().await?[0]["efforts"], json!(["high"]));
        assert!(session.steer("idle").await.is_err());
        let first = session.prompt("hold").await?;
        while session
            .tasks()
            .first()
            .and_then(|t| t.details.description.as_deref())
            != Some("working")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        session.stop_task("helper").await?;
        session.steer("first").await?;
        assert_eq!(first.report().await?.text, "/first");
        assert_eq!(session.usage().total.unwrap().input, 10);
        assert_eq!(
            session.prompt("second").await?.wait().await?,
            Outcome::Completed
        );
        assert_eq!(session.usage().total.unwrap().input, 20);
        for completed in [true, false] {
            let r = session
                .start_compaction()
                .await?
                .wait(Some(Duration::from_secs(2)))
                .await;
            assert!(
                match r.status {
                    CompactionStatus::Completed { .. } => completed,
                    CompactionStatus::Failed {
                        code: CompactionFailure::NotCompacted,
                        ..
                    } => !completed,
                    _ => false,
                },
                "{r:?}"
            );
        }
        for record in session.records() {
            let data = serde_json::to_string(&*record)?;
            assert!(
                ![PRIVATE_DATA, CREDENTIAL, "test-pat", "kar-qoder-"]
                    .iter()
                    .any(|s| data.contains(s))
            );
        }
        opts.resume = Some(session.id().into());
        session.close().await?;
        opts.retained_bytes = Some(1);
        let session = runtime.open(opts).await?;
        assert_eq!(
            session.prompt("third").await?.report().await?.text,
            "second/third"
        );
        assert_eq!(session.usage().total.unwrap().input, 10);
        if version == "1.1.83" {
            assert!(session.prompt("foreign").await?.report().await.is_err());
            assert!(
                session
                    .records()
                    .iter()
                    .all(|r| !r.native.to_string().contains("foreign-content"))
            );
        } else {
            session.close().await?;
        }
    }
    Ok(())
}

async fn durable_chat(runtime: &Runtime, opts: &SessionOptions) -> kar::Result<()> {
    use kar::chat::{ChatReplica, ChatStore, Item, Role, TurnStatus};
    use uuid::Uuid;
    let state = tempfile::tempdir()?;
    let store = ChatStore::open(state.path())?;
    let chat = store
        .create("qoder", Some(&runtime.program), opts.clone())
        .await?;
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    let snapshot = store.feed_snapshot(id)?;
    let mut client = ChatReplica::from_snapshot(snapshot);
    let mut feed = store.follow(id, Some(client.cursor()))?;
    chat.send(Uuid::new_v4(), "same").await?;
    let tip = store.feed_snapshot(id)?.cursor;
    while client.cursor() != tip {
        client.apply(feed.recv().await?)?;
    }
    drop(feed);
    chat.close().await?;
    let key = Uuid::new_v4();
    store.enqueue(id, key, "same")?;
    let reopened = store
        .resume_with(id, |o| {
            o.env = opts.env.clone();
            o.launch_args = opts.launch_args.clone();
        })
        .await?;
    assert_eq!(reopened.info().native_id, native);
    reopened.drain_queue().await?;
    reopened.drain_queue().await?;
    reopened.close().await?;
    let mut feed = store.follow(id, Some(client.cursor()))?;
    let tip = store.feed_snapshot(id)?.cursor;
    while client.cursor() != tip {
        let event = feed.recv().await?;
        assert!(client.apply(event.clone())?);
        assert!(!client.apply(event)?);
    }
    let turns = store.history(id)?.turns;
    assert_eq!(client.turns(), turns);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].id, key);
    for (turn, answer) in turns.iter().zip(["/same", "same/same"]) {
        assert_eq!(turn.status, TurnStatus::Completed);
        let messages: Vec<_> = turn
            .items
            .iter()
            .filter_map(|i| match &i.item {
                Item::Message { role, text, .. } => Some((*role, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(messages, [(Role::User, "same"), (Role::Assistant, answer)]);
    }
    Ok(())
}
