use super::*;
use kar::{Outcome, Session, TurnReport, observe::AgentStatus};
use tokio::time::{Duration, sleep, timeout};

const LIMIT: Duration = Duration::from_secs(10);
const HELD: &str = "kar-queue-held-at-model";

async fn wait_for(mut ready: impl FnMut() -> bool) {
    timeout(LIMIT, async {
        while !ready() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native Pi condition timed out");
}

async fn setup() -> (Fixture, kar::Runtime) {
    let f = fixture(true).await;
    let ext = f.temp.path().join("agent/extensions");
    fs::create_dir(&ext).unwrap();
    let gate = serde_json::to_string(&f.options.cwd.join("handler-release")).unwrap();
    let hooks = serde_json::to_string(&f.options.cwd.join("queue-hooks")).unwrap();
    let entered = serde_json::to_string(&f.options.cwd.join("handler-entered")).unwrap();
    fs::write(
        ext.join("independent.ts"),
        format!(
            r#"
        import {{ appendFileSync, existsSync, writeFileSync }} from 'node:fs';
        import {{ setTimeout as delay }} from 'node:timers/promises';
        export default pi => {{
            let ended, compete = false;
            pi.on('agent_settled', () => {{
                ended?.();
                if (compete) {{ compete = false; pi.sendUserMessage('{HELD}-EXT'); }}
            }});
            pi.on('input', async event => {{
                if (event.text === '{HELD}-A') compete = true;
                if (event.text === 'queued B') {{ appendFileSync({hooks}, 'B'); await delay(180); }}
                return {{ action: 'continue' }};
            }});
            pi.registerCommand('spawn', {{ description: 'probe', handler: async () => {{
                pi.sendUserMessage('{HELD}');
            }} }});
            pi.registerCommand('spawndelay', {{ description: 'probe', handler: async () => {{
                writeFileSync({entered}, '');
                while (!existsSync({gate})) await delay(5);
                await new Promise(resolve => {{ ended = resolve; pi.sendUserMessage('{HELD}'); }});
            }} }});
            pi.registerCommand('spawnwait', {{ description: 'probe', handler: async () => {{
                await new Promise(resolve => {{ ended = resolve; pi.sendUserMessage('{HELD}'); }});
                while (!existsSync({gate})) await delay(5);
            }} }});
        }}"#
        ),
    )
    .unwrap();
    let mut rt = kar::runtime("pi").unwrap();
    rt.program = std::env::var_os("KAR_PI_BIN").unwrap().into();
    (f, rt)
}

fn ends(s: &Session) -> usize {
    s.records()
        .iter()
        .flat_map(|r| &r.events)
        .filter(|e| matches!(e, Event::TurnEnded { .. }))
        .count()
}

fn handled(report: TurnReport) {
    assert_eq!(
        report.native_input_receipt,
        Some(kar::InputReceipt::Handled)
    );
    assert!(
        report.text.is_empty(),
        "independent model output leaked into command"
    );
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN with the public extension command API"]
async fn independent_model_outlives_its_handled_command() {
    let (f, rt) = setup().await;
    let s = rt.open(f.options.clone()).await.unwrap();
    let input = Uuid::new_v4();
    handled(
        timeout(LIMIT, async {
            s.prompt_with_id(input, "/spawn").await?.report().await
        })
        .await
        .unwrap()
        .unwrap(),
    );
    wait_for(|| f.options.cwd.join("requested").exists()).await;
    assert_eq!(ends(&s), 0);
    assert!(s.status().is_running());
    assert!(matches!(
        s.prompt("cannot interrupt EXT").await,
        Err(kar::Error::Busy)
    ));
    let (_, queued) = s.queue("continue after EXT").await.unwrap();
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    fs::write(f.options.cwd.join("release"), "").unwrap();
    assert_eq!(
        timeout(LIMIT, queued.wait()).await.unwrap().unwrap(),
        Outcome::Completed
    );
    assert_eq!(ends(&s), 2);
    assert!(matches!(
        s.await_idle().await.unwrap().status,
        AgentStatus::Idle {
            last_outcome: Some(Outcome::Completed),
            ..
        }
    ));
    assert_eq!(f.calls.lock().unwrap().len(), 2);
    s.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN with the public extension command API"]
async fn independent_end_does_not_settle_the_pending_command() {
    let (f, rt) = setup().await;
    let s = rt.open(f.options.clone()).await.unwrap();
    let client = s.clone();
    let prompt = tokio::spawn(async move { client.prompt("/spawnwait").await?.report().await });
    wait_for(|| f.options.cwd.join("requested").exists()).await;
    fs::write(f.options.cwd.join("release"), "").unwrap();
    wait_for(|| ends(&s) == 1).await;
    assert!(
        s.status().is_running(),
        "the extension command is still in preflight"
    );
    assert!(matches!(
        s.prompt("cannot overtake handler").await,
        Err(kar::Error::Busy)
    ));
    assert!(!prompt.is_finished());
    fs::write(f.options.cwd.join("handler-release"), "").unwrap();
    handled(timeout(LIMIT, prompt).await.unwrap().unwrap().unwrap());
    assert_eq!(ends(&s), 1);
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    assert!(matches!(
        s.await_idle().await.unwrap().status,
        AgentStatus::Idle {
            last_outcome: Some(Outcome::Completed),
            ..
        }
    ));
    s.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN; restarts Kar and reconnects HTTP clients across a native extension run"]
async fn clients_replay_independent_activity_without_attaching_it_to_the_command() {
    let (f, rt) = setup().await;
    let store = ChatStore::open(f.temp.path().join("chats")).unwrap();
    let chat = store
        .create("pi", Some(&rt.program), f.options.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let command = Uuid::new_v4();
    let initial = store.feed_snapshot(id).unwrap();
    let start = initial.cursor;
    let mut left = ChatReplica::from_snapshot(initial);
    let mut right = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = kar::chat::event_router(store.clone(), id);
    let host = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut a = SseClient::open(&url, &start.to_string(), None).await;
    let mut b = SseClient::open(&url, &start.to_string(), None).await;
    let result = timeout(LIMIT, chat.send(command, "/spawn"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.native_input_receipt,
        Some(kar::InputReceipt::Handled)
    );
    assert!(result.pi_input_timestamp_ms.is_none() && result.fork_anchor.is_none());
    wait_for(|| f.options.cwd.join("requested").exists()).await;
    let mut expected = store.follow(id, Some(start)).unwrap();
    let mut rows = vec![];
    let mut left_rows = vec![];
    let mut right_rows = vec![];
    let first = b.next().await;
    right_rows.push(serde_json::to_value(&first).unwrap());
    assert!(right.apply(first).unwrap());
    drop(b);
    let cut = store.feed_snapshot(id).unwrap().cursor;
    while left.cursor() != cut {
        let event = a.next().await;
        left_rows.push(serde_json::to_value(&event).unwrap());
        assert!(left.apply(event).unwrap());
    }
    assert_eq!(left.notifications().len(), 1);
    assert_eq!(left.notifications()[0].activity.status, TurnStatus::Running);
    assert_ne!(left.notifications()[0].activity.id, command);
    assert!(left.notifications()[0].joined_input.is_none());
    drop(a);
    let next = Uuid::new_v4();
    chat.enqueue(next, "queue after native activity").unwrap();
    fs::write(f.options.cwd.join("release"), "").unwrap();
    timeout(LIMIT, chat.drain_queue()).await.unwrap().unwrap();
    chat.close().await.unwrap();
    let resumed = store
        .resume_with(id, |o| o.env.clone_from(&f.options.env))
        .await
        .unwrap();
    resumed.close().await.unwrap();
    let last = store.feed_snapshot(id).unwrap();
    for (replica, output) in [(&mut left, &mut left_rows), (&mut right, &mut right_rows)] {
        let mut client = SseClient::open(
            &url,
            &start.to_string(),
            Some(&replica.cursor().to_string()),
        )
        .await;
        while replica.cursor() != last.cursor {
            let event = client.next().await;
            output.push(serde_json::to_value(&event).unwrap());
            assert!(replica.apply(event.clone()).unwrap());
            assert!(!replica.apply(event).unwrap());
        }
        assert_eq!(
            serde_json::to_value(replica.turns()).unwrap(),
            serde_json::to_value(&last.history.turns).unwrap()
        );
        assert_eq!(
            serde_json::to_value(replica.notifications()).unwrap(),
            serde_json::to_value(&last.history.notifications).unwrap()
        );
    }
    while expected.cursor() != last.cursor {
        rows.push(serde_json::to_value(expected.recv().await.unwrap()).unwrap());
    }
    assert_eq!(left_rows, rows);
    assert_eq!(right_rows, rows);
    let [opener, following] = left.turns() else {
        panic!("expected separate host inputs")
    };
    assert_eq!((opener.id, following.id), (command, next));
    assert!(!opener.items.iter().any(|i| matches!(
        i.item,
        Item::Message {
            role: Role::Assistant,
            ..
        }
    )));
    let [notice] = left.notifications() else {
        panic!("expected single independent model run")
    };
    assert_eq!(notice.activity.status, TurnStatus::Completed);
    assert!(
        notice
            .activity
            .items
            .iter()
            .any(|i| matches!(&i.item,Item::Message{role:Role::User,text,..} if text==HELD))
    );
    assert!(notice.activity.items.iter().any(
        |i| matches!(&i.item,Item::Message{role:Role::Assistant,text,..} if text=="rust-wire-ok")
    ));
    assert_eq!(following.status, TurnStatus::Completed);
    assert_eq!(f.calls.lock().unwrap().len(), 2);
    println!(
        "two HTTP clients matched {} durable events with separate command and extension activity",
        rows.len()
    );
    host.abort();
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN with an abortable native extension command"]
async fn abort_during_preflight_reaches_the_native_run_when_it_starts() {
    let (f, rt) = setup().await;
    let s = rt.open(f.options.clone()).await.unwrap();
    let client = s.clone();
    let pending = tokio::spawn(async move { client.prompt("/spawndelay").await?.report().await });
    wait_for(|| f.options.cwd.join("handler-entered").is_file()).await;
    timeout(LIMIT, s.abort()).await.unwrap().unwrap();
    fs::write(f.options.cwd.join("handler-release"), "").unwrap();
    handled(
        timeout(LIMIT, pending)
            .await
            .expect("deferred native abort timed out")
            .unwrap()
            .unwrap(),
    );
    assert!(matches!(
        s.await_idle().await.unwrap().status,
        AgentStatus::Idle {
            last_outcome: Some(Outcome::Aborted),
            ..
        }
    ));
    let rows = s.records();
    assert_eq!(ends(&s), 1);
    assert!(rows.iter().any(|r| r.events.contains(&Event::TurnEnded {
        outcome: Outcome::Aborted
    })));
    assert!(
        !rows
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e, Event::Exited { .. }))
    );
    fs::write(f.options.cwd.join("release"), "").unwrap();
    assert_eq!(
        s.prompt("new independent input")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        Outcome::Completed
    );
    s.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN; races one adapter-held input against a native extension run"]
async fn queued_refusal_during_an_independent_run_does_not_end_it() {
    let (f, rt) = setup().await;
    let s = rt.open(f.options.clone()).await.unwrap();
    let first = s.prompt(format!("{HELD}-A")).await.unwrap();
    wait_for(|| f.options.cwd.join("requested").exists()).await;
    let (input, queued) = s.queue("queued B").await.unwrap();
    fs::write(f.options.cwd.join("release"), "").unwrap();
    assert_eq!(
        timeout(LIMIT, first.wait()).await.unwrap().unwrap(),
        Outcome::Completed
    );
    wait_for(|| f.options.cwd.join("extension-requested").exists()).await;
    assert!(matches!(
        timeout(LIMIT, queued.wait()).await.unwrap(),
        Err(kar::Error::Refused(_))
    ));
    assert!(
        s.records()
            .iter()
            .any(|r| r.events.contains(&Event::NativeInput {
                input_id: input,
                receipt: kar::InputReceipt::Refused
            }) && r.events.contains(&Event::InputDropped {
                input_id: input.to_string()
            }))
    );
    assert_eq!(ends(&s), 1);
    assert!(s.status().is_running());
    assert!(matches!(
        s.prompt("cannot overtake EXT").await,
        Err(kar::Error::Busy)
    ));
    fs::write(f.options.cwd.join("extension-release"), "").unwrap();
    assert!(matches!(
        timeout(LIMIT, s.await_idle())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentStatus::Idle {
            last_outcome: Some(Outcome::Completed),
            ..
        }
    ));
    assert_eq!(ends(&s), 2);
    assert_eq!(f.calls.lock().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(f.options.cwd.join("queue-hooks")).unwrap(),
        "B"
    );
    assert_eq!(
        s.prompt("NEXT").await.unwrap().wait().await.unwrap(),
        Outcome::Completed
    );
    s.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires real KAR_PI_BIN; kills the controller while independent Pi activity is running"]
async fn killed_controller_recovers_handled_input_and_interrupts_only_the_independent_run() {
    use kar::chat::InputStatus;
    let (f, rt) = setup().await;
    let dir = f.temp.path().join("crash-chats");
    let store = ChatStore::open(&dir).unwrap();
    let chat = store
        .create("pi", Some(&rt.program), f.options.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    chat.send(Uuid::new_v4(), "seed real session")
        .await
        .unwrap();
    chat.close().await.unwrap();
    let command = Uuid::new_v4();
    let next = Uuid::new_v4();
    store.enqueue(id, command, "/spawn").unwrap();
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(f.temp.path())
        .envs(&f.options.env)
        .env("KAR_CHAT_DIR", &dir)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for(|| {
        assert!(controller.try_wait().unwrap().is_none());
        let h = store.history(id).unwrap();
        f.options.cwd.join("requested").exists()
            && h.turns
                .last()
                .is_some_and(|t| t.native_input_receipt == Some(kar::InputReceipt::Handled))
            && h.notifications
                .last()
                .is_some_and(|n| n.activity.status == TurnStatus::Running)
    })
    .await;
    let active = store.feed_snapshot(id).unwrap();
    let notice = active.history.notifications[0].activity.id;
    let mut observer = store.follow(id, Some(active.cursor)).unwrap();
    let mut replica = ChatReplica::from_snapshot(active);
    assert_eq!(
        store.enqueue(id, next, "resume after kill").unwrap().status,
        InputStatus::Pending
    );
    controller.kill().await.unwrap();
    assert!(!controller.wait().await.unwrap().success());
    timeout(LIMIT, async {
        while replica.notifications()[0].activity.status == TurnStatus::Running {
            let update = observer.recv().await.unwrap();
            assert!(replica.apply(update).unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(
        replica.notifications()[0].activity.status,
        TurnStatus::Interrupted
    );
    assert_eq!(
        replica.turns().last().unwrap().status,
        TurnStatus::Completed
    );
    fs::write(f.options.cwd.join("release"), "").unwrap();
    let reopened = store
        .resume_with(id, |o| o.env.clone_from(&f.options.env))
        .await
        .unwrap();
    assert_eq!(reopened.info().native_id, native);
    timeout(LIMIT, reopened.drain_queue())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reopened.enqueue(command, "/spawn").unwrap().status,
        InputStatus::Sent
    );
    assert_eq!(
        reopened.enqueue(next, "resume after kill").unwrap().status,
        InputStatus::Sent
    );
    reopened.close().await.unwrap();
    let h = store.history(id).unwrap();
    assert_eq!(h.notifications.len(), 1);
    assert_eq!(
        (
            h.notifications[0].activity.id,
            h.notifications[0].activity.status
        ),
        (notice, TurnStatus::Interrupted)
    );
    assert_eq!(h.turns.len(), 3);
    assert_eq!(
        (h.turns[1].id, h.turns[1].native_input_receipt),
        (command, Some(kar::InputReceipt::Handled))
    );
    assert_eq!(
        (h.turns[2].id, h.turns[2].status),
        (next, TurnStatus::Completed)
    );
    assert_eq!(f.calls.lock().unwrap().len(), 3);
}
