use super::*;

pub(super) async fn exercise(
    url: &str,
    host: &Client,
    project: &str,
    caller: Uuid,
    observer: &tokio::net::UdpSocket,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let target = Uuid::new_v4();
    server::json(
        host.post(format!("{url}/projects/{project}/chats"))
            .json(&json!({"id":target,"runtime":"codex"})),
    )
    .await;
    // Keep native handshakes in order for the subsequent workspace handoff scenario.
    observer.recv(&mut [0u8; 192]).await.unwrap();
    let policy = format!("{url}/projects/{project}/chats/{caller}/model-tools/permissions");
    let invoke = |name: &str, args: Value| rpc("tools/call", json!({"name":name,"arguments":args}));
    let read = || invoke("kar_thread_read", json!({"thread_id":target}));
    assert_eq!(server::json(read()).await["result"]["isError"], true);
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","thread_tools":{"write":true,"targets":[target]}})),
    )
    .await;
    let command = Uuid::new_v4();
    let rename = || {
        invoke(
            "kar_thread_update",
            json!({"thread_id":target,"command_id":command,"action":"rename","title":"Scoped follow-up"}),
        )
    };
    let (first, second) = tokio::join!(server::json(rename()), server::json(rename()));
    assert_ne!(first["result"]["isError"], true);
    assert_eq!(first["result"], second["result"]);
    assert_eq!(
        server::json(invoke(
            "kar_thread_update",
            json!({"thread_id":target,"command_id":command,"action":"rename","title":"different"})
        ))
        .await["result"]["isError"],
        true
    );
    let found = server::json(invoke(
        "kar_thread_search",
        json!({"query":"Scoped follow-up"}),
    ))
    .await;
    assert_eq!(
        found["result"]["structuredContent"]["threads"][0]["thread_id"],
        json!(target)
    );
    let id = Uuid::new_v4();
    let send = || {
        invoke(
            "kar_thread_send",
            json!({"thread_id":target,"command_id":id,"text":"one scoped input","mode":"queue"}),
        )
    };
    assert_eq!(
        server::json(send().header("kar-mode-limit", "auto/plan")).await["result"]["isError"],
        true
    );
    let (first, second) = tokio::join!(server::json(send()), server::json(send()));
    let id = first["result"]["structuredContent"]["input_id"]
        .as_str()
        .unwrap();
    assert_eq!(second["result"]["structuredContent"]["input_id"], id);
    let snap = server::json(invoke(
        "kar_thread_wait",
        json!({"thread_id":target,"timeout_ms":0}),
    ))
    .await;
    assert_eq!(
        snap["result"]["structuredContent"]["cursor"]["chat"],
        json!(target)
    );
    let current = server::json(read()).await;
    assert_eq!(
        current["result"]["structuredContent"]["thread_id"],
        json!(target)
    );
    assert_eq!(
        server::json(invoke(
            "kar_thread_read",
            json!({"thread_id":Uuid::new_v4()})
        ))
        .await["result"]["isError"],
        true
    );
    // Fork a completed turn while retaining the current working files. Its contribution
    // is held for the parent's next actual message, not dispatched as a new turn.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snap =
                server::json(host.get(format!("{url}/projects/{project}/chats/{target}/snapshot")))
                    .await;
            if snap["history"]["turns"].as_array().is_some_and(|ts| {
                ts.iter()
                    .any(|r| r["id"] == id && r["status"] == "completed")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let base_turn = Uuid::new_v4();
    server::json(
        host.post(format!("{url}/projects/{project}/chats/{target}/messages"))
            .json(&json!({"id":base_turn,"text":"fork anchor"})),
    )
    .await;
    let child = Uuid::new_v4();
    let fork_args = json!({"action":"fork","command_id":child,"thread_id":target,"source_point":{"type":"run","turn_id":base_turn}});
    let first = server::json(invoke("kar_thread_transfer", fork_args.clone())).await;
    assert_eq!(
        first["result"]["structuredContent"]["target_thread_id"],
        json!(child)
    );
    assert_eq!(
        server::json(invoke("kar_thread_transfer", fork_args)).await["result"],
        first["result"]
    );
    let child_turn = Uuid::new_v4();
    server::json(
        host.post(format!("{url}/projects/{project}/chats/{child}/messages"))
            .json(&json!({"id":child_turn,"text":"branch contribution"})),
    )
    .await;
    observer.recv(&mut [0u8; 192]).await.unwrap();
    let merge = Uuid::new_v4();
    let args = json!({"action":"merge_back","command_id":merge,"thread_id":child,"target_thread_id":target});
    assert_eq!(
        server::json(invoke("kar_thread_transfer", args.clone())).await["result"]["structuredContent"]
            ["status"],
        "pending"
    );
    assert_eq!(
        server::json(invoke("kar_thread_transfer", args)).await["result"]["structuredContent"]["status"],
        "pending"
    );
    let next = Uuid::new_v4();
    let next = server::json(
        host.post(format!("{url}/projects/{project}/chats/{target}/messages"))
            .json(&json!({"id":next,"text":"consume the merged context"})),
    )
    .await;
    assert_eq!(next["merged_context"]["id"], json!(merge));
    assert!(
        next["merged_context"]["text"]
            .as_str()
            .unwrap()
            .contains("branch contribution")
    );
    let later = server::json(
        host.post(format!("{url}/projects/{project}/chats/{target}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":"stay in the same native chat"})),
    )
    .await;
    assert!(later.get("merged_context").is_none());
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"plan","thread_tools":{"targets":[target]}})),
    )
    .await;
    assert_eq!(server::json(rename()).await["result"]["isError"], true);
    server::json(host.put(&policy).json(&json!({"mode":"execute"}))).await;
    assert_eq!(server::json(read()).await["result"]["isError"], true);
}
