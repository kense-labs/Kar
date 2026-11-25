use super::*;
pub(super) async fn exercise(
    url: &str,
    host: &Client,
    project: &str,
    caller: Uuid,
    observer: &tokio::net::UdpSocket,
    db: &std::path::Path,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let permissions = format!("{url}/projects/{project}/chats/{caller}/model-tools/permissions");
    let set = |mode: &str| {
        host.put(&permissions).json(
            &json!({"mode":mode,"thread_tools":{"delegate":true,"schedule":true,"write":true}}),
        )
    };
    let invoke = |name: &str, args: Value| rpc("tools/call", json!({"name":name,"arguments":args}));
    let task = Uuid::new_v4();
    let payload =
        json!({"command_id":task,"task":"Independent delegate task","title":"Delegate test"});
    server::json(set("plan")).await;
    let plan = server::json(invoke("kar_delegate_task", payload.clone())).await;
    assert!(plan["result"]["isError"] == true || plan["error"].is_object());
    server::json(set("execute")).await;
    let parent_turn = Uuid::new_v4();
    let base = format!("{url}/projects/{project}/chats/{caller}");
    let details = |id| format!("{base}/delegations/{id}");
    fs::write(db.join("hold-delegation-parent"), []).unwrap();
    let parent = tokio::spawn(server::json(
        host.post(format!("{base}/messages"))
            .json(&json!({"id":parent_turn,"text":"delegation wait in parent"})),
    ));
    tokio::time::timeout(Duration::from_secs(15), async {
        while !db.join("delegation-parent-ready").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            server::json(invoke("kar_delegate_task", payload.clone())).await["result"]["structuredContent"]
                ["task_id"],
            json!(task)
        );
    }
    observer.recv(&mut [0u8; 192]).await.unwrap();
    let ready = |id: Uuid| async move {
        let mut last = Value::Null;
        match tokio::time::timeout(Duration::from_secs(35), async {
            loop {
                let s = server::json(host.get(format!(
                    "{url}/projects/{project}/chats/{caller}/delegations/{id}"
                )))
                .await;
                if s["work_state"] == "result_available" {
                    break s;
                }
                assert!(
                    !matches!(
                        s["work_state"].as_str(),
                        Some("failed" | "cancelled" | "unconfirmed")
                    ),
                    "delegation entered unexpected terminal state: {s}"
                );
                last = s;
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        })
        .await
        {
            Ok(s) => s,
            Err(_) => panic!("delegation {id} timed out, last state: {last}"),
        }
    };
    let r = ready(task).await;
    assert_eq!(r["parent_run_id"], json!(parent_turn));
    assert_eq!(r["completion_delivery"]["state"], "pending");
    assert_eq!(r["delivered"], false);
    let policy =
        json!({"command_id":Uuid::new_v4(),"action":"wake_policy","completion_wake":"always"});
    let claimed = server::json(host.post(details(task)).json(&policy)).await;
    let wake = claimed["completion_delivery"]["message_id"]
        .as_str()
        .unwrap();
    assert_eq!(claimed["delivered"], false);
    let sibling = Uuid::new_v4();
    server::json(invoke(
        "kar_delegate_task",
        json!({"command_id":sibling,"task":"Second delegate","completion_wake":"always"}),
    ))
    .await;
    observer.recv(&mut [0u8; 192]).await.unwrap();
    assert_eq!(
        ready(sibling).await["completion_delivery"]["message_id"],
        wake
    );
    let ack = server::json(invoke("kar_task_status", json!({"task_id":task}))).await;
    assert_eq!(
        ack["result"]["structuredContent"]["completion_delivery"]["observed_by_run_id"],
        json!(parent_turn)
    );
    assert_eq!(ack["result"]["structuredContent"]["delivered"], false);
    let inbox = server::json(host.get(format!("{base}/queue"))).await;
    let entry = inbox["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == wake)
        .unwrap();
    let text = entry["text"].as_str().unwrap();
    assert!(!text.contains(&task.to_string()) && text.contains(&sibling.to_string()));
    let dispose = json!({"command_id":Uuid::new_v4(),"task_id":sibling,"action":"dispose"});
    for _ in 0..2 {
        let result = server::json(invoke("kar_task_delivery", dispose.clone())).await;
        assert_eq!(
            result["result"]["structuredContent"]["completion_delivery"]["state"],
            "disposed"
        );
    }
    let final_child = Uuid::new_v4();
    server::json(invoke(
        "kar_delegate_task",
        json!({"command_id":final_child,"task":"Notify after parent settles"}),
    ))
    .await;
    observer.recv(&mut [0u8; 192]).await.unwrap();
    assert_eq!(
        ready(final_child).await["completion_delivery"]["state"],
        "pending"
    );
    fs::remove_file(db.join("hold-delegation-parent")).unwrap();
    assert_eq!(parent.await.unwrap()["id"], json!(parent_turn));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let settled = server::json(host.get(details(final_child))).await;
            if settled["completion_delivery"]["state"] == "delivered" {
                assert_eq!(settled["delivered"], true);
                break;
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        server::json(host.get(details(task))).await["delivered"],
        false
    );
    let bad = server::json(invoke("kar_task_status", json!({"task_id":Uuid::new_v4()}))).await;
    assert_eq!(bad["result"]["isError"], true);
    let schedule = Uuid::new_v4();
    let setup = json!({"command_id":schedule,"title":"Scheduled agent","prompt":"scheduled conversation","schedule":{"type":"manual"},"bind_to_current_thread":true});
    for _ in 0..2 {
        assert_eq!(
            server::json(invoke("kar_schedule_task", setup.clone())).await["result"]["structuredContent"]
                ["scheduled_task_id"],
            json!(schedule)
        );
    }
    let trigger = json!({"command_id":Uuid::new_v4(),"scheduled_task_id":schedule});
    let first = server::json(invoke("kar_run_scheduled_task_now", trigger.clone())).await;
    let run = first["result"]["structuredContent"]["run_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        server::json(invoke("kar_run_scheduled_task_now", trigger)).await["result"]["structuredContent"]
            ["run_id"],
        run
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row =
                server::json(host.get(format!("{url}/projects/{project}/workflow-runs/{run}")))
                    .await;
            if row["status"] == "completed" {
                assert_eq!(row["receipts"][0]["attempt"]["chat"], json!(caller));
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let removed = json!({"command_id":Uuid::new_v4(),"scheduled_task_id":schedule});
    assert_eq!(
        server::json(invoke("kar_delete_scheduled_task", removed)).await["result"]["structuredContent"]
            ["deleted"],
        true
    );
    assert_eq!(
        server::json(invoke("kar_list_scheduled_tasks", json!({}))).await["result"]["structuredContent"]
            ["tasks"],
        json!([])
    );

    let stopped_child = Uuid::new_v4();
    server::json(invoke(
        "kar_delegate_task",
        json!({"command_id":stopped_child,"task":"await interrupt","title":"Stop child"}),
    ))
    .await;
    observer.recv(&mut [0u8; 192]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !db.join("interrupt-ready").exists() {
            let _ = server::json(host.get(details(stopped_child))).await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let _ = fs::remove_file(db.join("interrupt-ready"));
    server::json(
        host.post(format!("{url}/projects/{project}/chats/{stopped_child}/force-stop"))
            .json(&json!({})),
    )
    .await;
    let cancelled = server::json(host.get(details(stopped_child))).await;
    assert_eq!(cancelled["work_state"], "cancelled", "{cancelled}");

    server::json(host.put(&permissions).json(&json!({"mode":"execute"}))).await;
}
