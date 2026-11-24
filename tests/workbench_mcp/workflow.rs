use super::*;

pub(super) async fn exercise(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |input: Value| {
        rpc(
            "tools/call",
            json!({"name":"kar_workflow","arguments":input}),
        )
    };
    let request = |action: &str, mut body: Value| {
        body["action"] = action.into();
        call(body)
    };
    let id = Uuid::new_v4();
    let command = Uuid::new_v4();
    server::json(
        host.put(format!("{url}/projects/{project}/workflows/{id}"))
            .json(&json!({
                "name":"MCP run","enabled":true,"schedule":{"type":"manual"},
                "steps":[{"type":"ask","question":"Proceed?"},{"type":"wait","duration_ms":300000}]
            })),
    )
    .await;
    let start = json!({"action":"start","workflow_id":id,"command_id":command,"input":"test"});
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"plan","workflow_reads":true})),
    )
    .await;
    assert_eq!(
        server::json(call(start.clone())).await["result"]["isError"],
        true
    );
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","workflow_writes":true})),
    )
    .await;
    let first = server::json(call(start.clone())).await;
    let run = first["result"]["structuredContent"]["run"]["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        server::json(call(start)).await["result"]["structuredContent"]["run"]["id"],
        run
    );
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let q = server::json(request("questions", json!({}))).await;
            if let Some(v) = q["result"]["structuredContent"]["questions"]
                .as_array()
                .and_then(|qs| qs.iter().find(|q| q["run"] == run))
            {
                break v.clone();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(question["question"], "Proceed?");
    let answer = json!({"run_id":run,"attempt_id":question["attempt"],"command_id":Uuid::new_v4(),"value":"yes"});
    for _ in 0..2 {
        let response = server::json(request("signal", answer.clone())).await;
        assert_eq!(
            response["result"]["structuredContent"]["run"]["receipts"][0]["result"],
            "yes"
        );
    }
    let stop = json!({"run_id":run,"command_id":Uuid::new_v4()});
    assert_eq!(
        server::json(request("cancel", stop.clone()).header("kar-mode-limit", "auto/plan")).await["result"]
            ["isError"],
        true
    );
    for _ in 0..2 {
        assert_eq!(
            server::json(request("cancel", stop.clone())).await["result"]["structuredContent"]["run"]
                ["status"],
            "cancelled"
        );
    }
    let list = server::json(request("list", json!({"status":"cancelled"}))).await;
    assert_eq!(list["result"]["structuredContent"]["runs"][0]["id"], run);
    let read = server::json(request("read", json!({"run_id":run}))).await;
    assert_eq!(
        read["result"]["structuredContent"]["run"]["steps"][0]["type"],
        "ask"
    );
    assert_eq!(
        server::json(request(
            "read",
            json!({"run_id":run,"project_id":Uuid::new_v4()})
        ))
        .await["result"]["isError"],
        true
    );

    // A failed script stops on the exact stored attempt until the caller explicitly retries it.
    let fail = Uuid::new_v4();
    server::json(host.put(format!("{url}/projects/{project}/workflows/{fail}")).json(&json!({
        "name":"MCP retry","enabled":true,"schedule":{"type":"manual"},"steps":[{"type":"script","command":"exit 19"}]
    }))).await;
    let started = server::json(request(
        "start",
        json!({"workflow_id":fail,"command_id":Uuid::new_v4()}),
    ))
    .await;
    let failed = started["result"]["structuredContent"]["run"]["id"]
        .as_str()
        .unwrap();
    let attempt = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let r = server::json(request("read", json!({"run_id":failed}))).await;
            let row = &r["result"]["structuredContent"]["run"];
            if row["status"] == "failed" {
                break row["receipts"][0]["attempt"]["id"].clone();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let resume = json!({"run_id":failed,"attempt_id":attempt,"command_id":Uuid::new_v4()});
    for _ in 0..2 {
        assert_eq!(
            server::json(request("resume", resume.clone())).await["result"]["structuredContent"]["run"]
                ["id"],
            failed
        );
    }

    let key = "ci/status";
    let set = json!({"key":key,"value":{"passed":true},"command_id":Uuid::new_v4()});
    assert_eq!(
        server::json(request("store_set", set.clone()).header("kar-mode-limit", "auto/plan")).await
            ["result"]["isError"],
        true
    );
    for _ in 0..2 {
        assert_eq!(
            server::json(request("store_set", set.clone())).await["result"]["structuredContent"]["entries"]
                [0]["value"]["passed"],
            true
        );
    }
    assert_eq!(
        server::json(request("store", json!({"key":"ci/"}))).await["result"]["structuredContent"]["entries"]
            [0]["key"],
        key
    );
    let delete = json!({"key":key,"command_id":Uuid::new_v4()});
    for _ in 0..2 {
        assert_eq!(
            server::json(request("store_delete", delete.clone())).await["result"]["structuredContent"]
                ["entries"],
            json!([])
        );
    }
    let wf = Uuid::new_v4();
    server::json(host.put(format!("{url}/projects/{project}/workflows/{wf}")).json(&json!({
        "name":"Signal delivery","enabled":true,"schedule":{"type":"manual"},
        "steps":[{"type":"wait_signal","channel":"ci"},{"type":"wait_signal","channel":"ci"}]
    }))).await;
    let early =
        json!({"workflow_id":wf,"channel":"ci","command_id":Uuid::new_v4(),"value":"early"});
    for _ in 0..2 {
        assert_eq!(
            server::json(request("signal", early.clone())).await["result"]["structuredContent"]["delivery"],
            "buffered"
        );
    }
    let run = server::json(request(
        "start",
        json!({"workflow_id":wf,"command_id":Uuid::new_v4()}),
    ))
    .await["result"]["structuredContent"]["run"]["id"]
        .clone();
    tokio::time::timeout(Duration::from_secs(10),async{
        loop{
            let row=server::json(request("read",json!({"run_id":run}))).await["result"]["structuredContent"]["run"].clone();
            if row["cursor"]==1 && row["status"]=="waiting"{assert_eq!(row["receipts"][0]["result"],"early");break;}
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }).await.unwrap();
    let later = json!({"workflow_id":wf,"channel":"ci","command_id":Uuid::new_v4(),"value":"late"});
    for _ in 0..2 {
        assert_eq!(
            server::json(request("signal", later.clone())).await["result"]["structuredContent"]["delivery"],
            "delivered"
        );
    }
    let final_run = server::json(request("read", json!({"run_id":run}))).await;
    assert_eq!(
        final_run["result"]["structuredContent"]["run"]["status"],
        "completed"
    );
    assert_eq!(
        final_run["result"]["structuredContent"]["run"]["receipts"][1]["result"],
        "late"
    );
}
