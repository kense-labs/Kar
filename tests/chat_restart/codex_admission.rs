use super::*;

/// Native read the request, then the controller died before receiving any reply.
pub(super) fn reopen() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    for dir in ["native", "work", "elsewhere"] {
        fs::create_dir(base.join(dir)).unwrap();
    }
    let run = |args: &[&str]| success(cli(&base).args(args).output().unwrap());
    let history = success(
        cli(&base)
            .args(["new", "codex", "--cwd"])
            .arg(base.join("work"))
            .arg("--bin")
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap(),
    );
    let chat = history["chat"]["id"].as_str().unwrap();
    let input = Uuid::new_v4().to_string();
    let text = "hold without reply";
    let queue = || run(&["queue", "--chat", chat]);
    let resume = || run(&["resume", "--chat", chat]);
    run(&["enqueue", text, "--chat", chat, "--request-id", &input]);
    let mut host = cli(&base)
        .args(["resume", "--chat", chat])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let calls = base.join("native/calls");
    let limit = Instant::now() + Duration::from_secs(10);
    let request = loop {
        if let Some(request) = fs::read_to_string(&calls)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            break request;
        }
        assert!(Instant::now() < limit, "native never received turn/start");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(request["input"], input);
    host.kill().unwrap();
    host.wait().unwrap();
    assert_eq!(queue()[0]["status"], "uncertain");

    let opener = |id: &str| json!({"type":"userMessage","id":"u","clientId":id,"content":[{"type":"text","text":text}]});
    let turn =
        |id: &str, status: &str, items: Value| json!({"id":id,"status":status,"items":items});
    let saved = base.join("native/read-turns");
    let write = |turns: Value| fs::write(&saved, serde_json::to_vec(&turns).unwrap()).unwrap();
    let pending = turn("target", "inProgress", json!([opener(&input)]));

    for ambiguous in [
        json!([turn("other", "completed", json!([opener("other")]))]),
        json!([turn(
            "steered",
            "completed",
            json!([opener("other"), opener(&input)])
        )]),
        json!([
            pending,
            turn("second", "completed", json!([opener(&input)]))
        ]),
    ] {
        write(ambiguous);
        resume();
        assert_eq!(queue()[0]["status"], "uncertain");
    }

    run(&["acknowledge-uncertain", &input, "--chat", chat]);
    let next = run(&["resume", "later round", "--chat", chat]);
    assert_eq!(next["status"], "completed");
    write(json!([pending]));
    resume();
    assert_eq!(queue()[0]["status"], "sent");
    let turn_before_end = run(&["history", "--chat", chat]);
    let original = &turn_before_end["turns"][0];
    assert_eq!(original["id"], input);
    assert_eq!(original["native_turn_id"], "target");
    assert_eq!(original["status"], "interrupted");
    assert!(original["fork_anchor"].is_null());

    write(json!([turn(
        "target",
        "completed",
        json!([opener(&input), {"type":"agentMessage","id":"answer","text":"found once"}])
    )]));
    for _ in 0..2 {
        resume();
        let history = run(&["history", "--chat", chat]);
        let turns = history["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1], next);
        assert_eq!(turns[0]["id"], input);
        assert_eq!(turns[0]["fork_anchor"], "target");
        assert_eq!(turns[0]["status"], "completed");
        assert_eq!(turns[0]["items"].as_array().unwrap().len(), 2);
        assert_eq!(turns[0]["items"][1]["text"], "found once");
        assert_eq!(queue()[0]["status"], "sent");
    }
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 2);
}
