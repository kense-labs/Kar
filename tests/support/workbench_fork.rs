use super::workbench_server::{client, json as ok, serve};
use reqwest::Client;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command, time::Duration};
use uuid::Uuid;

pub(super) fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Kar",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn project(client: &Client, url: &str, path: &Path, name: &str) -> String {
    ok(client
        .post(format!("{url}/projects"))
        .json(&json!({"root":path,"name":name})))
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn say(client: &Client, url: &str, text: &str) -> Value {
    ok(client
        .post(format!("{url}/messages"))
        .json(&json!({"id":Uuid::new_v4(),"text":text})))
    .await
}

async fn next_change(response: &mut reqwest::Response) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            bytes.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            if let Some(end) = bytes.windows(2).position(|w| w == b"\n\n") {
                let frame = std::str::from_utf8(&bytes[..end]).unwrap();
                if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) {
                    break serde_json::from_str(data).unwrap();
                }
                bytes.drain(..end + 2);
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::main]
pub(crate) async fn run() {
    tokio::time::timeout(Duration::from_secs(120), scenario())
        .await
        .unwrap();
}

async fn scenario() {
    let temp = tempfile::tempdir().unwrap();
    let allowed = temp.path().join("roots");
    let main = allowed.join("repo");
    let target = allowed.join("child");
    let state = temp.path().join("state");
    let native = temp.path().join("native");
    fs::create_dir_all(&main).unwrap();
    fs::create_dir(&native).unwrap();
    git(&main, &["init", "-b", "main"]);
    fs::write(main.join("shared"), "base").unwrap();
    git(&main, &["add", "shared"]);
    git(&main, &["commit", "-m", "base"]);

    let (mut server, url) = serve(&state, &allowed, &native).await;
    let a = client();
    let b = client();
    let p = project(&a, &url, &main, "parent").await;
    let worktree_id = Uuid::new_v4();
    ok(a.post(format!("{url}/projects/{p}/git")).json(&json!({
        "op":"worktree_add","id":worktree_id,"directory":target,"branch":"forked","start":"main"
    })))
    .await;
    let target_project = project(&b, &url, &target, "forked").await;
    let chat = Uuid::new_v4();
    let child = Uuid::new_v4();
    let created = ok(a
        .post(format!("{url}/projects/{p}/chats"))
        .json(&json!({"id":chat,"runtime":"codex"})))
    .await;
    let parent_path = format!("/projects/{p}/chats/{chat}");
    let child_path = format!("/projects/{target_project}/chats/{child}");
    let parent = format!("{url}{parent_path}");
    let store = kar::chat::ChatStore::open(&state).unwrap();
    let notes = Uuid::new_v4();
    let private = Uuid::new_v4();
    for id in [notes, private] {
        store
            .persist_attachment(
                chat,
                id,
                "notes.txt",
                "text/plain",
                kar::chat::AttachmentKind::File,
                b"review notes",
            )
            .unwrap();
    }
    let first = ok(a.post(format!("{parent}/messages")).json(&json!({
        "id":Uuid::new_v4(),"text":"parent context","attachments":[notes]
    })))
    .await;
    let native_calls = fs::read_to_string(native.join("calls")).unwrap();
    let call: Value = serde_json::from_str(native_calls.lines().last().unwrap()).unwrap();
    let (_, files) = call["text"]
        .as_str()
        .unwrap()
        .split_once("\n\nAttached files (JSON):\n")
        .unwrap();
    let files: Vec<Value> = serde_json::from_str(files).unwrap();
    assert_eq!(files[0]["name"], "notes.txt");
    assert_eq!(
        fs::read(files[0]["path"].as_str().unwrap()).unwrap(),
        b"review notes"
    );
    let cut = say(&a, &parent, "shared-child").await;
    let after_cut = Uuid::new_v4();
    store
        .persist_attachment(
            chat,
            after_cut,
            "later.png",
            "image/png",
            kar::chat::AttachmentKind::Image,
            b"\x89PNG\r\n\x1a\nlater",
        )
        .unwrap();
    fs::write(native.join("hold-parent-turn"), []).unwrap();
    let (client, chat_url) = (b.clone(), parent.clone());
    let third = tokio::spawn(async move {
        ok(client.post(format!("{chat_url}/messages")).json(&json!({
            "id": Uuid::new_v4(), "text": "shared-next", "attachments": [after_cut]
        })))
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !native.join("parent-turn-running").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request = json!({"id":child,"turn":cut["id"],"destination_project":target_project});
    let receipt = ok(a.post(format!("{parent}/forks")).json(&request)).await;
    assert!(!third.is_finished());
    assert_eq!(fs::read(main.join("shared")).unwrap(), b"shared-next");
    assert_eq!(fs::read(target.join("shared")).unwrap(), b"shared-child");
    fs::remove_file(native.join("hold-parent-turn")).unwrap();
    let latest = third.await.unwrap();
    assert_eq!(receipt["project"], target_project);
    let h = &receipt["history"];
    assert_eq!(h["chat"]["id"], child.to_string());
    assert_ne!(h["chat"]["native_id"], created["info"]["native_id"]);
    assert_eq!(
        h["chat"]["forked_from"]["native_source"],
        created["info"]["native_id"]
    );
    assert_eq!(h["chat"]["worktree_id"], worktree_id.to_string());
    assert_eq!(h["turns"].as_array().unwrap().len(), 2);
    assert_eq!(h["turns"][0]["id"], first["id"]);
    assert_eq!(h["turns"][0]["attachments"][0]["id"], notes.to_string());
    let inherited_url = ok(b.post(format!("{url}{child_path}/attachments/{notes}/url"))).await;
    let read = reqwest::get(format!(
        "{url}{}",
        inherited_url["read_path"].as_str().unwrap()
    ))
    .await
    .unwrap();
    assert_eq!(
        read.error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"review notes"
    );
    assert!(
        !b.post(format!("{url}{child_path}/attachments/{private}/url"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    // The guest's attachment read belongs to this share and its copied timeline.
    for (scope, expected) in [
        (&child_path, target_project.as_str()),
        (&parent_path, p.as_str()),
    ] {
        let share = ok(a.post(format!("{url}{scope}/shares")).json(&json!({}))).await;
        assert_eq!(share["project"], expected);
        let share_id = share["id"].as_str().unwrap();
        let guest_root = format!("{url}/guest/{share_id}");
        let public = Client::new();
        let guest = ok(public
            .post(format!("{guest_root}/redeem"))
            .json(&json!({"credential":share["credential"],"name":"Reader"})))
        .await;
        let token = guest["token"].as_str().unwrap();
        let view = ok(public
            .get(format!("{guest_root}/snapshot"))
            .bearer_auth(token))
        .await;
        assert_eq!(view["turns"][0]["attachments"][0]["id"], notes.to_string());
        let read = format!("{guest_root}/attachments/{notes}");
        let bytes = public
            .get(&read)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"review notes");
        let unsent = public
            .get(format!("{guest_root}/attachments/{private}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(unsent.status(), reqwest::StatusCode::NOT_FOUND);
        let later = public
            .get(format!("{guest_root}/attachments/{after_cut}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        if *scope == parent_path {
            assert!(later.status().is_success());
            assert_eq!(later.headers()["content-type"], "image/png");
            assert_eq!(
                later.bytes().await.unwrap().as_ref(),
                b"\x89PNG\r\n\x1a\nlater"
            );
        } else {
            assert_eq!(later.status(), reqwest::StatusCode::NOT_FOUND);
        }
        ok(a.delete(format!("{url}/access/shares/{share_id}"))).await;
        assert_eq!(
            public
                .get(&read)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(h["turns"][1]["id"], cut["id"]);
    assert_eq!(fs::read(target.join("shared")).unwrap(), b"shared-child");
    assert_eq!(fs::read(main.join("shared")).unwrap(), b"shared-next");
    let fork_count = fs::read_to_string(native.join("forks")).unwrap();
    assert_eq!(fork_count.lines().count(), 1);
    let child_turn = say(&b, &format!("{url}{child_path}"), "shared-next").await;
    let replay = ok(a.post(format!("{parent}/forks")).json(&request)).await;
    assert_eq!(replay["history"]["turns"][2]["id"], child_turn["id"]);
    assert_eq!(fs::read(target.join("shared")).unwrap(), b"shared-next");
    let rename = json!({"action":"rename","command_id":Uuid::new_v4(),"title":"Fork review"});
    let renamed = ok(a.post(format!("{url}{child_path}/metadata")).json(&rename)).await;
    assert_eq!(renamed["metadata"]["title"], "Fork review");
    let start = renamed["sequence"].as_u64().unwrap();
    let listen = format!("{url}/projects/{target_project}/chat-state/events?after={start}");
    let mut a_events = a
        .get(&listen)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut b_events = b
        .get(&listen)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let tag = json!({"action":"tag","tag":"release","tagged":true,"command_id":Uuid::new_v4()});
    let tagged = ok(b.post(format!("{url}{child_path}/metadata")).json(&tag)).await;
    for feed in [&mut a_events, &mut b_events] {
        assert_eq!(next_change(feed).await, tagged);
    }
    drop((a_events, b_events));
    assert_eq!(
        ok(a.post(format!("{url}{child_path}/metadata")).json(&tag)).await,
        tagged
    );
    let pin = json!({"action":"pin","order":"ab","command_id":Uuid::new_v4()});
    assert_eq!(
        ok(a.post(format!("{url}{child_path}/metadata")).json(&pin)).await["metadata"]["pin_order"],
        "ab"
    );
    let settle = json!({"action":"settle","expected_turn":child_turn["id"],"settled_ms":900,"command_id":Uuid::new_v4()});
    let settled = ok(b.post(format!("{url}{child_path}/metadata")).json(&settle)).await;
    assert!(settled["metadata"]["pinned_ms"].is_null());
    server.kill().await.unwrap();
    let (mut next, url) = serve(&state, &allowed, &native).await;
    let query = format!("{url}/chat-state/query");
    let family = ok(b.post(&query).json(&json!({"query":"family","chat":child}))).await;
    assert_eq!(family["chats"].as_array().unwrap().len(), 2);
    let links = ok(b
        .post(&query)
        .json(&json!({"query":"links","chats":[chat,child,Uuid::new_v4()]})))
    .await;
    assert_eq!(links["values"].as_object().unwrap().len(), 2);
    assert_eq!(links["values"][child.to_string()]["title"], "Fork review");
    assert_eq!(
        links["values"][child.to_string()]["project"],
        target_project
    );
    let count = ok(b
        .post(&query)
        .json(&json!({"query":"older_settled","before_ms":901,"limit":0})))
    .await;
    assert_eq!(count["counts"][&target_project], 1);
    let page = ok(a
        .post(&query)
        .json(&json!({"query":"older_settled","before_ms":901,"limit":1})))
    .await;
    assert_eq!(page["chats"][0]["info"]["id"], child.to_string());
    let page = ok(a
        .post(&query)
        .json(&json!({"query":"older_settled","before_ms":900,"after_id":child,"limit":1})))
    .await;
    assert!(page["chats"].as_array().unwrap().is_empty());
    let poll = format!("{url}/projects/{target_project}/chat-state/events");
    let mut replay_events = b
        .get(&poll)
        .header("last-event-id", start)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(next_change(&mut replay_events).await, tagged);
    drop(replay_events);
    assert_eq!(
        ok(a.post(format!("{url}{child_path}/metadata")).json(&tag)).await,
        tagged
    );
    let child_find = format!("{url}{child_path}/find");
    let find = ok(b.post(&child_find).json(&json!({"query":"ha","index":1}))).await;
    // Two distinct native replies have identical text. Both remain searchable after a fork.
    assert!(find["total_matches"].as_u64().unwrap() >= 4);
    let occurrence = &find["match"];
    let detail = ok(b.get(format!(
        "{url}{child_path}/items/{}",
        occurrence["entry_id"].as_str().unwrap()
    )))
    .await;
    assert_eq!(detail["item"]["turn"], occurrence["turn"]);
    let next_match = ok(b
        .post(&child_find)
        .json(&json!({"query":"HA","start":occurrence,"offset":1})))
    .await;
    assert_eq!(next_match["active_index"], 2);
    let raw = b
        .post(format!("{child_find}/events"))
        .json(&json!({"query":"ha"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let frames: Vec<Value> = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|text| serde_json::from_str(text).unwrap())
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["complete"], false);
    assert_eq!(frames[0]["cursor"], frames[1]["cursor"]);
    assert_eq!(frames[1]["total_matches"], find["total_matches"]);
    let root_diff = format!("{url}{parent_path}/checkpoints/diff");
    let full = ok(b.get(format!("{root_diff}?to=3"))).await;
    assert!(full["diff"].as_str().unwrap().contains("+shared-next"));
    assert!(full["diff"].as_str().unwrap().contains("-base"));
    let single = ok(b.get(format!("{root_diff}?from=1&to=2"))).await;
    assert!(single["diff"].as_str().unwrap().contains("+shared-child"));
    let inherited = ok(b.get(format!("{url}{child_path}/checkpoints/diff?from=1&to=2"))).await;
    assert_eq!(inherited["diff"], single["diff"]);
    assert_eq!(
        ok(b.get(format!("{root_diff}?from=2&to=2"))).await["diff"],
        ""
    );
    assert!(
        !b.get(format!("{root_diff}?from=4&to=2"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        !b.get(format!("{root_diff}?to=5"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let forks = format!("{url}{parent_path}/forks");
    let replay = ok(b.post(&forks).json(&request)).await;
    assert_eq!(
        replay["history"]["chat"]["native_id"],
        h["chat"]["native_id"]
    );
    let mut wrong = request.clone();
    wrong["turn"] = latest["id"].clone();
    assert!(
        !a.post(&forks)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        !a.post(&forks)
            .json(&json!({"id":child,"turn":cut["id"]}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let parent_snapshot = ok(b.get(format!("{url}{parent_path}/snapshot"))).await;
    assert_eq!(parent_snapshot["history"]["turns"][2]["id"], latest["id"]);
    say(
        &b,
        &format!("{url}{child_path}"),
        "continue on original fork",
    )
    .await;
    assert_eq!(
        fs::read_to_string(native.join("forks")).unwrap(),
        fork_count
    );
    let command_turn = Uuid::new_v4();
    let run = tokio::spawn(ok(a
        .post(format!("{url}{child_path}/messages"))
        .json(&json!({"id":command_turn,"text":"watch command output"}))));
    let entries = format!("{url}{child_path}/turns/{command_turn}/items");
    let entry = loop {
        let response = b.get(&entries).send().await.unwrap();
        if response.status().is_success() {
            let rows = response.json::<Vec<Value>>().await.unwrap();
            if let Some(row) = rows.iter().find(|r| r["item"]["kind"] == "tool") {
                break row["entry_id"].as_str().unwrap().to_owned();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let output = format!("{url}{child_path}/items/{entry}/output");
    let mut left = a
        .get(&output)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut right = b
        .get(&output)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for feed in [&mut left, &mut right] {
        assert_eq!(
            next_change(feed).await,
            json!({"kind":"replace","text":"","truncated":false,"running":true})
        );
    }
    fs::write(native.join("output-append"), []).unwrap();
    for feed in [&mut left, &mut right] {
        assert_eq!(
            next_change(feed).await,
            json!({"kind":"append","text":"20%","truncated":false,"running":true})
        );
    }
    fs::write(native.join("output-replace"), []).unwrap();
    for feed in [&mut left, &mut right] {
        let frame = next_change(feed).await;
        assert_eq!(frame["kind"], "replace");
        assert_eq!(frame["text"].as_str().unwrap().len(), 18003);
    }
    fs::write(native.join("output-finish"), []).unwrap();
    let expected = json!({"kind":"replace","text":"PASS\n","running":false,"truncated":false});
    for feed in [&mut left, &mut right] {
        assert_eq!(next_change(feed).await, expected);
    }
    assert_eq!(run.await.unwrap()["status"], "completed");
    drop((left, right));
    let mut late = b
        .get(&output)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(next_change(&mut late).await, expected);
    let parent_url = format!("{url}{parent_path}");
    let rewind = json!({"id":Uuid::new_v4(), "turn":first["id"], "expected_head":latest["id"], "policy":"native"});
    let before_rewind = fs::read(main.join("shared")).unwrap();
    let result = ok(a.post(format!("{parent_url}/rewinds")).json(&rewind)).await;
    assert_eq!(result["mode"], "native");
    assert_eq!(result["chat"], chat.to_string());
    assert_eq!(result["removed_turns"], json!([cut["id"], latest["id"]]));
    assert_eq!(fs::read(main.join("shared")).unwrap(), before_rewind);
    assert_eq!(
        ok(b.post(format!("{parent_url}/rewinds")).json(&rewind)).await,
        result
    );
    let parent_snapshot = ok(b.get(format!("{parent_url}/snapshot"))).await;
    assert_eq!(
        parent_snapshot["history"]["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        parent_snapshot["history"]["chat"]["native_id"],
        result["native_id"]
    );
    assert_eq!(
        ok(b.get(format!("{url}{child_path}/snapshot"))).await["history"]["turns"][1]["id"],
        cut["id"]
    );
    verify_rich_thread_find(&b, &url, &p, &state, &store, chat).await;
    next.kill().await.unwrap();
}

async fn verify_rich_thread_find(
    client: &Client,
    url: &str,
    project: &str,
    state: &Path,
    store: &kar::chat::ChatStore,
    template_chat: Uuid,
) {
    let store_dir = state.canonicalize().unwrap();
    let first_line = fs::read_to_string(store_dir.join(format!("{template_chat}.karchat")))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let mut header: Value = serde_json::from_str(&first_line).unwrap();
    let rich_chat = Uuid::new_v4();
    header["chat"]["id"] = json!(rich_chat);
    header["chat"]["native_id"] = json!("rich-find-native");
    header["chat"]["worktree_id"] = Value::Null;
    header["chat"]["forked_from"] = Value::Null;
    header["chat"]["imported"] = json!({
        "project": Uuid::parse_str(project).unwrap(),
        "runtime": "codex",
        "native_id": "rich-find-native",
        "transcript": store_dir.join("rich-transcript.jsonl"),
        "stamp": "1",
        "title": "Rich thread find",
        "omitted_messages": 0,
    });

    let source = json!({"session_id": "rich-find-native", "path": []});
    let turn_1 = Uuid::new_v4();
    let turn_2 = Uuid::new_v4();
    let turn_3 = Uuid::new_v4();
    let turn_4 = Uuid::new_v4();
    let turn_5 = Uuid::new_v4();

    let assistant_rich = concat!(
        "Compare [src/notes.ts](src/notes.ts#L42-L64) with [test/notes.ts](test/notes.ts#L10), ",
        "read [important description](/tmp/actual.ts) and [public docs](https://hidden.example/secret), ",
        "inspect `/tmp/inline_chip.ts:25:4` and /Users/alice/work/Makefile:9.\n\n",
        "<details><summary>折叠标题</summary><p>折叠正文</p></details>\n\n",
        "`★ Insight ─────────────────────────────────────`\n",
        "Insight_visible_body\n",
        "`─────────────────────────────────────────────────`\n\n",
        "<picture>",
        "<source media=\"(prefers-color-scheme: dark)\" srcset=\"/tmp/theme_dark_only.ts\">",
        "<source media=\"(prefers-color-scheme: light)\" srcset=\"/tmp/theme_light_only.ts\">",
        "<img src=\"/tmp/theme_fallback.ts\">",
        "</picture>",
    );

    let turns: Vec<kar::chat::ChatTurn> = serde_json::from_value(json!([
        {
            "id": turn_1,
            "started_ms": 1000,
            "finished_ms": 1001,
            "status": "completed",
            "fork_anchor": null,
            "items": [
                {
                    "source": source,
                    "kind": "message",
                    "role": "user",
                    "text": "Review [Assistant quote](kar-citation://v1/env/chat/msg?text=quoted_hidden&comment=visible_annotation) and before![img](kar-context://v1/image/1)after plus Install as ~/a/x, then read (~/a/guide.md) and anchor_token",
                    "provider_id": "u1",
                    "complete": true
                },
                {
                    "source": source,
                    "kind": "message",
                    "role": "assistant",
                    "text": assistant_rich,
                    "provider_id": "a1",
                    "complete": true
                }
            ]
        },
        {
            "id": turn_2,
            "started_ms": 2000,
            "finished_ms": 2001,
            "status": "completed",
            "fork_anchor": null,
            "items": [
                {
                    "source": source,
                    "kind": "message",
                    "role": "user",
                    "text": "between message without anchor",
                    "provider_id": "u2",
                    "complete": true
                },
                {
                    "source": source,
                    "kind": "tool",
                    "id": "mcp-app-1",
                    "activity": {
                        "name": "launch",
                        "mcp_app_uri": "ui://weather/main",
                        "finished": true,
                        "result": {
                            "status": "succeeded",
                            "exit_code": null,
                            "content": []
                        }
                    }
                },
                {
                    "source": source,
                    "kind": "message",
                    "role": "assistant",
                    "text": "",
                    "provider_id": "a2",
                    "complete": true
                }
            ]
        },
        {
            "id": turn_3,
            "started_ms": 3000,
            "finished_ms": 3001,
            "status": "completed",
            "fork_anchor": null,
            "items": [
                {
                    "source": source,
                    "kind": "message",
                    "role": "user",
                    "text": "follow up",
                    "provider_id": "u3",
                    "complete": true
                },
                {
                    "source": source,
                    "kind": "message",
                    "role": "assistant",
                    "text": "second anchor_token at end with [pkg/notes.ts](pkg/notes.ts#L99)",
                    "provider_id": "a3",
                    "complete": true
                },
                {
                    "source": source,
                    "kind": "request_error",
                    "error": {
                        "cursor": {"stream_id": Uuid::new_v4(), "after_seq": 1},
                        "request_id": "req-err-1",
                        "method": "item/tool/requestUserInput",
                        "request_kind": "question",
                        "status": "failed",
                        "reason": "delivery_failure_visible_reason",
                        "recorded_ms": 3002
                    }
                }
            ]
        },
        {
            "id": turn_4,
            "started_ms": 4000,
            "finished_ms": 4001,
            "status": "completed",
            "fork_anchor": null,
            "items": [
                {
                    "source": source,
                    "kind": "message",
                    "role": "user",
                    "text": "render html card",
                    "provider_id": "u4",
                    "complete": true
                },
                {
                    "source": source,
                    "kind": "tool",
                    "id": "html-render-1",
                    "activity": {
                        "name": "kar_html_render",
                        "finished": true,
                        "result": {
                            "status": "succeeded",
                            "exit_code": null,
                            "content": [
                                {
                                    "type": "text",
                                    "text": "{\"htmlRender\":{\"title\":\"Preview\"}}"
                                }
                            ]
                        }
                    }
                },
                {
                    "source": source,
                    "kind": "message",
                    "role": "assistant",
                    "text": "",
                    "provider_id": "a4",
                    "complete": true
                }
            ]
        },
        {
            "id": turn_5,
            "started_ms": 5000,
            "finished_ms": 5001,
            "status": "completed",
            "fork_anchor": null,
            "items": [
                {
                    "source": source,
                    "kind": "message",
                    "role": "user",
                    "text": "plain empty reply",
                    "provider_id": "u5",
                    "complete": true
                },
                {
                    "source": source,
                    "kind": "message",
                    "role": "assistant",
                    "text": "",
                    "provider_id": "a5",
                    "complete": true
                }
            ]
        }
    ]))
    .unwrap();

    let imported_row = kar::chat::ChatUpdate::Imported { turns };
    fs::write(store_dir.join(format!("{rich_chat}.lock")), []).unwrap();
    fs::write(
        store_dir.join(format!("{rich_chat}.karchat")),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&imported_row).unwrap()
        ),
    )
    .unwrap();
    assert_eq!(store.history(rich_chat).unwrap().turns.len(), 5);

    let rich_base = format!("{url}/projects/{project}/chats/{rich_chat}");
    let find_url = format!("{rich_base}/find");
    let query_find = |body: Value| ok(client.post(&find_url).json(&body));

    // 1. Duplicate basename file chips (within turn_1 and across turn_3), visible line ranges, custom prose labels, hidden URLs/prefixes.
    let notes = query_find(json!({"query": "notes.ts", "index": 0})).await;
    assert_eq!(notes["total_matches"], 3);
    assert_eq!(notes["match"]["turn"], turn_1.to_string());
    assert_eq!(notes["match"]["occurrence"], 0);
    let notes_second = query_find(json!({
        "query": "notes.ts",
        "start": notes["match"],
        "offset": 1
    }))
    .await;
    assert_eq!(notes_second["active_index"], 1);
    assert_eq!(notes_second["match"]["turn"], turn_1.to_string());
    assert_eq!(notes_second["match"]["occurrence"], 1);
    let notes_third = query_find(json!({
        "query": "notes.ts",
        "start": notes["match"],
        "offset": 2
    }))
    .await;
    assert_eq!(notes_third["active_index"], 2);
    assert_eq!(notes_third["match"]["turn"], turn_3.to_string());
    assert_eq!(notes_third["match"]["occurrence"], 0);

    for present in [
        "repo/src",
        "repo/test",
        "L42–64",
        "L10",
        "important description",
        "actual.ts",
        "public docs",
        "inline_chip.ts · L25:C4",
        "Makefile · L9",
    ] {
        let hit = query_find(json!({"query": present})).await;
        assert_eq!(hit["total_matches"], 1, "expected 1 match for {present:?}");
        assert_eq!(hit["match"]["turn"], turn_1.to_string());
    }
    let cross_turn_line = query_find(json!({"query": "notes.ts · L99"})).await;
    assert_eq!(cross_turn_line["total_matches"], 1);
    assert_eq!(cross_turn_line["match"]["turn"], turn_3.to_string());
    for hidden in [
        "L42-64",
        "src/notes.ts",
        "test/notes.ts",
        "pkg/notes.ts",
        "repo/pkg",
        "https://hidden.example/secret",
        "hidden.example",
        "secret",
        "/tmp/actual.ts",
        "/tmp/inline_chip.ts",
        "/Users/alice",
    ] {
        let miss = query_find(json!({"query": hidden})).await;
        assert_eq!(
            miss["total_matches"], 0,
            "expected 0 matches for {hidden:?}"
        );
    }

    // 2. Quote / citation annotation & kar-context:// image boundary & user single-tilde preservation.
    assert_eq!(
        query_find(json!({"query": "visible_annotation"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "quoted_hidden"})).await["total_matches"],
        0
    );
    assert_eq!(
        query_find(json!({"query": "Assistant quote"})).await["total_matches"],
        0
    );
    assert_eq!(
        query_find(json!({"query": "beforeafter"})).await["total_matches"],
        0
    );
    assert_eq!(
        query_find(json!({"query": "~/a/x"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "~/a/guide.md"})).await["total_matches"],
        1
    );

    // 3. Folded <details>/<summary>, Insight dividers, and <picture> theme filtering.
    assert_eq!(
        query_find(json!({"query": "折叠标题"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "折叠正文"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "折叠标题折叠正文"})).await["total_matches"],
        0
    );
    assert_eq!(
        query_find(json!({"query": "Insight_visible_body"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "★ Insight"})).await["total_matches"],
        0
    );
    assert_eq!(
        query_find(json!({"query": "theme_dark_only.ts", "theme": "dark"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "theme_dark_only.ts", "theme": "light"})).await["total_matches"],
        0
    );
    assert_eq!(
        query_find(json!({"query": "theme_light_only.ts", "theme": "light"})).await["total_matches"],
        1
    );
    assert_eq!(
        query_find(json!({"query": "theme_light_only.ts", "theme": "dark"})).await["total_matches"],
        0
    );

    // 4. RequestError indexing & suppressed "(empty response)" for MCP App and kar_html_render.
    let err_hit = query_find(json!({"query": "delivery_failure_visible_reason"})).await;
    assert_eq!(err_hit["total_matches"], 1);
    assert_eq!(err_hit["match"]["turn"], turn_3.to_string());

    let empty_hit = query_find(json!({"query": "empty response"})).await;
    assert_eq!(empty_hit["total_matches"], 1);
    assert_eq!(empty_hit["match"]["turn"], turn_5.to_string());

    // 5. Reading-position jump from a 0-match middle message advances to next match (and agrees with SSE).
    let turn_2_items: Vec<Value> = ok(client.get(format!("{rich_base}/turns/{turn_2}/items")))
        .await
        .as_array()
        .unwrap()
        .clone();
    let between_entry = turn_2_items[0]["entry_id"].as_str().unwrap();
    let from_middle = query_find(json!({
        "query": "anchor_token",
        "start": {"entry_id": between_entry, "occurrence": 0}
    }))
    .await;
    assert_eq!(from_middle["total_matches"], 2);
    assert_eq!(from_middle["active_index"], 1);
    assert_eq!(from_middle["match"]["turn"], turn_3.to_string());
    assert_eq!(from_middle["match"]["occurrence"], 0);

    let prev_from_middle = query_find(json!({
        "query": "anchor_token",
        "start": {"entry_id": between_entry, "occurrence": 0},
        "offset": -1
    }))
    .await;
    assert_eq!(prev_from_middle["active_index"], 0);
    assert_eq!(prev_from_middle["match"]["turn"], turn_1.to_string());

    let sse_raw = client
        .post(format!("{find_url}/events"))
        .json(&json!({
            "query": "anchor_token",
            "start": {"entry_id": between_entry, "occurrence": 0}
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let sse_frames: Vec<Value> = sse_raw
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|text| serde_json::from_str(text).unwrap())
        .collect();
    assert_eq!(sse_frames.len(), 2);
    assert_eq!(sse_frames[0]["complete"], false);
    assert_eq!(sse_frames[1]["complete"], true);
    assert_eq!(sse_frames[0]["match"], from_middle["match"]);
    assert_eq!(sse_frames[1]["match"], from_middle["match"]);
    assert_eq!(sse_frames[1]["active_index"], 1);
}
