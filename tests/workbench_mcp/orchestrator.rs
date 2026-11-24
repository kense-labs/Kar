use super::*;
pub(super) async fn exercise(
    url: &str,
    host: &Client,
    project: &str,
    caller: Uuid,
    observer: &tokio::net::UdpSocket,
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
    for _ in 0..2 {
        assert_eq!(
            server::json(invoke("kar_delegate_task", payload.clone())).await["result"]["structuredContent"]
                ["task_id"],
            json!(task)
        );
    }
    observer.recv(&mut [0u8; 192]).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let s = server::json(invoke("kar_task_status", json!({"task_id":task}))).await;
            if s["result"]["structuredContent"]["work_state"] == "result_available" {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(90)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["result"]["structuredContent"]["delivered"], true);
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
    server::json(host.put(&permissions).json(&json!({"mode":"execute"}))).await;
}
