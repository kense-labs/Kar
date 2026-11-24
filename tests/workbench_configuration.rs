#[path = "support/workbench_server.rs"]
#[allow(dead_code)]
mod server;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
#[tokio::test]
async fn shortcuts_and_preferences_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let native = dir.path().join("native");
    let state = dir.path().join("state");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&native).unwrap();
    let (mut proc, base) = server::serve(&state, &work, &native).await;
    let host = server::client();
    let endpoint = format!("{base}/server/config/events");
    let stream = || async {
        host.get(&endpoint)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes_stream()
            .eventsource()
    };
    let (mut first, mut second) = tokio::join!(stream(), stream());
    let a = first.next().await.unwrap().unwrap();
    let b = second.next().await.unwrap().unwrap();
    assert_eq!(a.id, b.id);
    let snap: Value = serde_json::from_str(&a.data).unwrap();
    assert!(snap["keybindings"].as_array().unwrap().len() > 70);
    let key = json!({"key":"cmd+shift+8","command":"workspace.focusNext","when":"!terminalFocus && (composerFocus || isDesktop)","replace":{"key":"f6","command":"workspace.focusNext"}});
    let keys = server::json(host.put(format!("{base}/server/keybindings")).json(&key)).await;
    let set = keys["keybindings"].as_array().unwrap();
    assert!(set.iter().any(|r| r["command"] == "workspace.focusNext"
        && r["shortcut"]["key"] == "8"
        && r["whenAst"]["type"] == "and"));
    assert!(set.iter().any(|r| r["command"] == "workspace.focusNext"
        && r["shortcut"]["key"] == "f6"
        && r["shortcut"]["modKey"] == true));
    for event in [
        first.next().await.unwrap().unwrap(),
        second.next().await.unwrap().unwrap(),
    ] {
        assert!(event.id.parse::<u64>().unwrap() > a.id.parse::<u64>().unwrap())
    }
    assert!(
        host.put(format!("{base}/server/keybindings"))
            .json(&json!({"key":"x","command":"script.build.run","when":"("}))
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    let prefs=server::json(host.patch(format!("{base}/server/settings")).json(&json!({"favorites":[{"provider":"codex","model":"chosen"}],"newWorktreesStartFromOrigin":false}))).await;
    assert_eq!(prefs["favorites"][0]["model"], "chosen");
    assert_eq!(
        server::json(host.get(format!("{base}/environment"))).await["preferences"]["new_worktrees_start_from_origin"],
        false
    );
    tokio::time::timeout(Duration::from_secs(4), first.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    proc.kill().await.unwrap();
    let (mut proc, base) = server::serve(&state, &work, &native).await;
    let persisted = server::json(host.get(format!("{base}/server/config"))).await;
    assert_eq!(persisted["settings"], prefs);
    assert_eq!(persisted["keybindings"], keys["keybindings"]);
    let removed=server::json(host.delete(format!("{base}/server/keybindings")).json(&json!({"key":"cmd+shift+8","command":"workspace.focusNext","when":"!terminalFocus && (composerFocus || isDesktop)"}))).await;
    assert!(
        !removed["keybindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["command"] == "workspace.focusNext" && r["shortcut"]["key"] == "8")
    );
    proc.kill().await.unwrap();
}
