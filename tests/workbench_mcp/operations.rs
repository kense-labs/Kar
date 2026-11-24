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
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"plan","thread_tools":{"targets":[target]}})),
    )
    .await;
    assert_eq!(server::json(rename()).await["result"]["isError"], true);
    server::json(host.put(&policy).json(&json!({"mode":"execute"}))).await;
    assert_eq!(server::json(read()).await["result"]["isError"], true);
}
