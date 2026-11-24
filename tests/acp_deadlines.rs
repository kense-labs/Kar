//! Advance Tokio time at a real child process's request boundary.
use kar::{AcpTimeouts, Error, McpServer, Protocol, Runtime, SessionOptions};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn peer() {
    let env = |key: &str| std::env::var(key).unwrap();
    let held = env("KAR_TEST_HELD");
    let provider = env("KAR_TEST_PROFILE");
    let gate = std::net::TcpStream::connect(env("KAR_TEST_GATE")).unwrap();
    let mut gate = std::io::BufReader::new(gate);
    let wait = |gate: &mut std::io::BufReader<std::net::TcpStream>, label: &str| {
        writeln!(gate.get_mut(), "{label} {}", std::process::id()).unwrap();
        let mut go = String::new();
        assert_ne!(gate.read_line(&mut go).unwrap(), 0);
    };
    let write = |v: Value| {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{v}").unwrap();
        out.flush().unwrap();
    };
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let packet: Value = serde_json::from_str(&line).unwrap();
        let Some(method) = packet["method"].as_str() else {
            continue;
        };
        if method == held {
            wait(&mut gate, method);
        }
        let selected = env("KAR_TEST_OPEN_METHOD");
        let result = match method {
            "initialize" => {
                json!({"protocolVersion":1,"authMethods":[{"id":if provider=="kimi" {"login"} else {"cached_token"}}],
                "agentCapabilities":{"loadSession":true,"mcpCapabilities":{"http":true},"sessionCapabilities":{
                "list":{},"resume": if selected=="session/load" {Value::Null} else {json!({})}, "fork":{}}}})
            }
            "authenticate" => {
                assert_eq!(
                    packet["params"]["methodId"],
                    if provider == "kimi" {
                        "login"
                    } else {
                        "cached_token"
                    }
                );
                json!({})
            }
            "session/list" => {
                json!({"sessions":[{"sessionId":"saved","cwd":std::env::current_dir().unwrap()}]})
            }
            "session/new" | "session/resume" | "session/load" | "session/fork" => {
                assert_eq!(method, selected);
                json!({"sessionId":if method=="session/fork" {"forked"} else {"saved"},"configOptions":[{"category":"thought_level","id":"thinking","currentValue":"low"},{"id":"mode","currentValue":"default","options":[{"value":"plan"}]}]})
            }
            "session/prompt" => {
                assert_eq!(packet["params"]["sessionId"], "saved");
                assert_eq!(
                    packet["params"]["prompt"][0]["text"],
                    "continue after timeout"
                );
                write(
                    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"saved",
                    "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"continued"}}}}),
                );
                json!({"stopReason":"end_turn"})
            }
            "session/set_mode" => {
                assert_eq!(packet["params"]["modeId"], "plan");
                json!({})
            }
            "session/set_model" => json!({}),
            "session/set_config_option" => {
                json!({"configOptions":[{"category":"thought_level","id":"thinking","currentValue":"high"}]})
            }
            _ => panic!("unexpected method: {method}"),
        };
        write(json!({"jsonrpc":"2.0","id":packet["id"],"result":result}));
        if method == selected && std::env::var_os("KAR_TEST_READY").is_some() {
            wait(&mut gate, "readiness");
            for (name, params) in [
                (
                    "_x.ai/mcp_initialized",
                    json!({"sessionId":"saved","mcpToolCount":1}),
                ),
                (
                    "_x.ai/mcp/server_status",
                    json!({"sessionId":"saved","name":"host","status":"ready"}),
                ),
            ] {
                write(json!({"jsonrpc":"2.0","method":name,"params":params}));
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Stalled,
    After(u64),
    Readiness {
        reply_after: u64,
        ready_after: Option<u64>,
    },
}

async fn check(
    provider: &str,
    opened: &str,
    method: &str,
    seconds: u64,
    reply: Reply,
    overrides: AcpTimeouts,
) {
    eprintln!("Checking {provider} {opened} {method} ({seconds}s)");
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut opts = SessionOptions::new(dir.path());
    opts.acp_timeouts = overrides;
    opts.env
        .insert("HOME".into(), dir.path().as_os_str().into());
    opts.env
        .insert("USERPROFILE".into(), dir.path().as_os_str().into());
    for (key, value) in [
        ("KAR_TEST_PROFILE", provider),
        ("KAR_TEST_HELD", method),
        ("KAR_TEST_OPEN_METHOD", opened),
    ] {
        opts.env.insert(key.into(), value.into());
    }
    opts.env.insert(
        "KAR_TEST_GATE".into(),
        listener.local_addr().unwrap().to_string().into(),
    );
    if matches!(opened, "session/resume" | "session/load") {
        opts.resume = Some("saved".into());
    }
    if method == "session/set_mode" {
        opts.settings.acp_mode = Some("plan".into());
    }
    if method == "session/set_model" {
        opts.model = Some("asked".into());
    }
    if method == "session/set_config_option" {
        opts.settings.effort = Some("high".into());
    }
    if matches!(reply, Reply::Readiness { .. }) {
        opts.env.insert("KAR_TEST_READY".into(), "1".into());
        opts.mcp_servers.insert(
            "host".into(),
            McpServer::HostedHttp {
                url: "http://127.0.0.1:1/mcp".into(),
                headers: Default::default(),
            },
        );
    }
    let rt = Runtime::new(
        provider,
        std::env::current_exe().unwrap(),
        &["acp-peer"],
        Protocol::Acp,
    );
    let retry = (matches!(reply, Reply::Stalled) && method == "session/load").then(|| {
        let mut retry = opts.clone();
        retry.env.insert("KAR_TEST_HELD".into(), "".into());
        (rt.clone(), retry)
    });
    let fork = opened == "session/fork";
    let mut opening = tokio::spawn(async move {
        if fork {
            rt.fork("saved", None, opts).await
        } else {
            rt.open(opts).await
        }
    });
    let (conn, _) = tokio::select! {
        v=listener.accept()=>v.unwrap(),
        v=&mut opening=>panic!("open ended before handshake: {:?}",v.unwrap().err())
    };
    let mut conn = tokio::io::BufReader::new(conn);
    let mut marker = String::new();
    assert_ne!(
        conn.read_line(&mut marker).await.unwrap(),
        0,
        "{provider}/{method}"
    );
    let (sent, pid) = marker.trim().split_once(' ').unwrap();
    assert_eq!(sent, method);
    let pid = pid.parse::<i32>().unwrap();
    let limit = Duration::from_secs(seconds);
    let mut expected_timeout = None;
    match reply {
        Reply::Stalled => {
            tokio::time::advance(limit - Duration::from_millis(1)).await;
            assert!(!opening.is_finished());
            // Tokio rounds timers up to its millisecond wheel tick.
            tokio::time::advance(Duration::from_millis(2)).await;
            expected_timeout = Some(method);
        }
        Reply::After(sec) => {
            tokio::time::advance(Duration::from_secs(sec)).await;
            assert!(!opening.is_finished());
            conn.get_mut().write_all(b"go\n").await.unwrap();
        }
        Reply::Readiness {
            reply_after,
            ready_after,
        } => {
            tokio::time::advance(Duration::from_secs(reply_after)).await;
            conn.get_mut().write_all(b"go\n").await.unwrap();
            let mut waiting = String::new();
            conn.read_line(&mut waiting).await.unwrap();
            assert!(waiting.starts_with("readiness "));
            if let Some(after) = ready_after {
                tokio::time::advance(Duration::from_secs(after)).await;
                conn.get_mut().write_all(b"ready\n").await.unwrap();
            } else {
                tokio::time::advance(
                    limit - Duration::from_secs(reply_after) - Duration::from_millis(1),
                )
                .await;
                assert!(!opening.is_finished());
                // Tokio rounds timers up to its millisecond wheel tick.
                tokio::time::advance(Duration::from_millis(2)).await;
                expected_timeout = Some("_x.ai/mcp_initialized");
            }
        }
    }
    let result = opening.await.unwrap();
    match expected_timeout {
        Some(label) => {
            let Err(Error::Timeout(reason)) = result else {
                panic!("expected {provider}/{label} timeout")
            };
            assert_eq!(reason, format!("{label} ({} ms)", limit.as_millis()));
            #[cfg(unix)]
            assert_eq!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
                Err(nix::errno::Errno::ESRCH),
                "{provider}/{label} returned before the child was reaped"
            );
        }
        None => {
            let session = result.unwrap();
            assert_eq!(
                session.id(),
                if opened == "session/fork" {
                    "forked"
                } else {
                    "saved"
                }
            );
            session.close().await.unwrap();
        }
    }
    let mut end = String::new();
    assert_eq!(conn.read_line(&mut end).await.unwrap(), 0);
    if let Some((runtime, opts)) = retry {
        let session = runtime.open(opts).await.unwrap();
        assert_eq!(session.id(), "saved");
        let turn = session.prompt("continue after timeout").await.unwrap();
        assert_eq!(turn.wait().await.unwrap(), kar::Outcome::Completed);
        assert!(
            session
                .records()
                .iter()
                .flat_map(|r| &r.events)
                .any(|e| matches!(e, kar::Event::TextDelta { text } if text == "continued"))
        );
        session.close().await.unwrap();
    }
}

async fn trials() {
    let auto = AcpTimeouts::default();
    for method in ["session/new", "session/resume", "session/fork"] {
        check("kimi", method, method, 630, Reply::After(60), auto).await;
        check("kimi", method, method, 630, Reply::Stalled, auto).await;
    }
    for (provider, slow) in [("kimi", 45), ("grok", 40)] {
        check(
            provider,
            "session/load",
            "session/load",
            60,
            Reply::After(slow),
            auto,
        )
        .await;
        check(
            provider,
            "session/load",
            "session/load",
            60,
            Reply::Stalled,
            auto,
        )
        .await;
        for method in [
            "initialize",
            "authenticate",
            "session/set_model",
            "session/set_config_option",
        ] {
            check(provider, "session/new", method, 30, Reply::Stalled, auto).await;
        }
    }
    check(
        "kimi",
        "session/new",
        "session/set_mode",
        30,
        Reply::Stalled,
        auto,
    )
    .await;
    check(
        "kimi",
        "session/resume",
        "session/set_mode",
        30,
        Reply::After(1),
        auto,
    )
    .await;
    check(
        "kimi",
        "session/load",
        "session/list",
        30,
        Reply::Stalled,
        auto,
    )
    .await;
    check(
        "grok",
        "session/new",
        "session/new",
        30,
        Reply::Stalled,
        auto,
    )
    .await;
    for ready_after in [Some(5), None] {
        check(
            "grok",
            "session/load",
            "session/load",
            60,
            Reply::Readiness {
                reply_after: 25,
                ready_after,
            },
            auto,
        )
        .await;
    }
    check(
        "fixture",
        "session/new",
        "session/new",
        15,
        Reply::Stalled,
        auto,
    )
    .await;
    check(
        "fixture",
        "session/load",
        "session/load",
        60,
        Reply::Stalled,
        auto,
    )
    .await;
    let custom = AcpTimeouts {
        session_load: Some(Duration::from_secs(45)),
        ..auto
    };
    check(
        "fixture",
        "session/load",
        "session/load",
        45,
        Reply::Stalled,
        custom,
    )
    .await;
    check(
        "fixture",
        "session/new",
        "session/new",
        15,
        Reply::Stalled,
        custom,
    )
    .await;
    let short_start = AcpTimeouts {
        session_start: Some(Duration::from_secs(7)),
        ..auto
    };
    check(
        "kimi",
        "session/resume",
        "session/resume",
        7,
        Reply::Stalled,
        short_start,
    )
    .await;
    check(
        "kimi",
        "session/load",
        "session/load",
        60,
        Reply::Stalled,
        short_start,
    )
    .await;
    let long = AcpTimeouts {
        request: Some(Duration::from_secs(90)),
        ..auto
    };
    check(
        "fixture",
        "session/load",
        "session/load",
        90,
        Reply::Stalled,
        long,
    )
    .await;
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("1.0.92");
        return;
    }
    if std::env::args().any(|a| a == "acp-peer") {
        peer();
        return;
    }
    tokio::time::pause();
    // Keep the executor awake while the external peer is scheduled. Only explicit
    // advances above may expire a protocol timer. A wall clock bounds the suite.
    tokio::select! {
        ()=trials()=>{},
        ()=async {let start=std::time::Instant::now();loop {
            assert!(start.elapsed()<Duration::from_secs(25),"ACP deadline trials stalled");
            tokio::task::yield_now().await;
        }}=>unreachable!(),
    }
    println!("ACP request, session start, saved history, and Grok readiness deadlines verified");
}
