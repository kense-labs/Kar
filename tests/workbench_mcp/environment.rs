use super::*;
pub(super) async fn controls(
    url: &str,
    host: &Client,
    p: &str,
    chat: Uuid,
    rpc: &impl Fn(&str, Value) -> RequestBuilder,
) {
    let invoke =
        |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let perm = format!("{url}/projects/{p}/chats/{chat}/model-tools/permissions");
    let cards = format!("{url}/projects/{p}/chats/{chat}/collaboration/requests");
    let pair = server::json(host.post(format!("{url}/access/pairings"))).await;
    let dev = server::json(
        Client::new()
            .post(format!("{url}/access/pair"))
            .json(&json!({"credential":pair["credential"],"label":"model-env-test"})),
    )
    .await;
    let device = Client::new();
    let token = dev["token"].as_str().unwrap();
    let policy = json!({"mode":"execute","environment_writes":true});
    assert!(
        !device
            .put(&perm)
            .bearer_auth(token)
            .json(&policy)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    server::json(host.put(&perm).json(&policy)).await;
    assert_eq!(
        device
            .get(&cards)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let restore = server::json(host.get(format!("{url}/environment"))).await;
    let env = server::json(invoke("kar_environment_read", json!({}))).await;
    assert_eq!(
        env["result"]["structuredContent"]["environment_id"],
        restore["environment_id"]
    );
    let change = json!({"preferences":{"prompt_suggestions":true,"source_control_writing_style":{"mode":"custom","custom_instructions":"Use active voice."}}});
    assert!(
        server::json(
            invoke("kar_environment_preferences_update", change.clone())
                .header("kar-mode-limit", "full-access/plan")
        )
        .await["error"]
            .is_object()
    );
    let prefs = server::json(invoke("kar_environment_preferences_update", change)).await;
    assert_eq!(
        prefs["result"]["structuredContent"]["prompt_suggestions"],
        true
    );
    assert_eq!(
        server::json(host.get(format!("{url}/settings/prompt-suggestions"))).await["enabled"],
        true
    );
    server::json(
        host.put(format!("{url}/environment/preferences"))
            .json(&restore["preferences"]),
    )
    .await;
    let publish=server::json(invoke("kar_theme_set",json!({"theme_id":"sample","theme":{"name":"Sample","appearance":"dark","accent":"#aabbcc","colors":{"canvas":"#111"},"css":"body { color: #ccc; }"},"appearance":"dark"}))).await;
    assert_eq!(publish["result"]["structuredContent"]["published"], true);
    let theme = server::json(host.get(format!("{url}/environment/theme?theme_id=sample"))).await;
    assert_eq!(theme["current"]["theme_id"], "sample");
    assert_eq!(theme["theme"]["css"], "body { color: #ccc; }");
    let sec = Uuid::new_v4();
    let future = invoke(
        "kar_request_secret",
        json!({"request_id":sec,"label":"Fixture","reason":"Test the private file handoff","delivery":"file","fields":[{"name":"accessCode","label":"Access code"}]}),
    );
    let future = tokio::spawn(async move { server::json(future).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let rows = server::json(host.get(&cards)).await;
            if rows.to_string().contains(&sec.to_string()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    let _ = server::json(
        host.post(format!("{cards}/{sec}"))
            .json(&json!({"accept":true,"input":{"accessCode":"private-fixture"}})),
    )
    .await;
    let reply = future.await.unwrap();
    let file = reply["result"]["structuredContent"]["files"]["accessCode"]
        .as_str()
        .unwrap();
    assert_eq!(fs::read_to_string(file).unwrap(), "private-fixture");
    assert!(!reply.to_string().contains("private-fixture"));
    assert!(
        !server::json(host.get(&cards))
            .await
            .to_string()
            .contains("private-fixture")
    );
    let report = Uuid::new_v4();
    let requested=server::json(invoke("kar_feedback",json!({"request_id":report,"user_requested":true,"subject":"general","kind":"bug","expected":"first display","actual":"second display","turns":1}))).await;
    assert_eq!(
        requested["result"]["structuredContent"]["status"],
        "pending"
    );
    let saved = server::json(
        host.post(format!("{cards}/{report}"))
            .json(&json!({"accept":true,"input":{"include_conversation":false}})),
    )
    .await;
    assert_eq!(saved["status"], "stored");
    assert_eq!(saved["sent"], false);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let peer_observer=tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_address=peer_observer.local_addr().unwrap().to_string();
    let (mut remote,remote_url)=server::serve_with_env(&temp.path().join("state"),&root,temp.path(),&[("KAR_TEST_MCP_OBSERVER",std::ffi::OsStr::new(&peer_address))]).await;
    let destination = server::json(host.get(format!("{remote_url}/environment"))).await;
    let destination_id = &destination["environment_id"];
    let req = Uuid::new_v4();
    assert_ne!(destination_id, &restore["environment_id"]);
    let ask=server::json(invoke("kar_environment_link",json!({"request_id":req,"environment_id":destination_id,"hint":"second machine","url":remote_url}))).await;
    assert_eq!(ask["result"]["structuredContent"]["status"], "pending");
    assert!(
        server::json(host.get(format!("{remote_url}/access/devices")))
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    let grant = server::json(
        host.post(format!("{remote_url}/api/auth/pairing-token"))
            .json(&json!({"purpose":"peer-link","access":"read-only"})),
    )
    .await;
    let got =
        server::json(host.post(format!("{cards}/{req}")).json(
            &json!({"accept":true,"input":{"url":remote_url,"credential":grant["credential"]}}),
        ))
        .await;
    assert_eq!(got["environment_id"], *destination_id);
    let linked = server::json(invoke("kar_environment_links", json!({}))).await;
    assert_eq!(
        linked["result"]["structuredContent"]["links"][0]["online"],
        true
    );
    assert!(!linked.to_string().contains("token"));
    let remote_desc = server::json(invoke(
        "kar_environment_read",
        json!({"environment_id":destination_id}),
    ))
    .await;
    assert_eq!(
        remote_desc["result"]["structuredContent"]["environment_id"],
        *destination_id
    );
    let project = server::json(
        host.post(format!("{remote_url}/projects"))
            .json(&json!({"root":root,"name":"remote"})),
    )
    .await;
    let project = project["id"].as_str().unwrap();
    let target = Uuid::new_v4();
    server::json(
        host.post(format!("{remote_url}/projects/{project}/chats"))
            .json(&json!({"runtime":"codex","id":target})),
    )
    .await;
    let proxied = format!(
        "{url}/environment/links/{}/projects/{project}/chats/{target}",
        destination_id.as_str().unwrap()
    );
    let page = server::json(host.get(format!("{proxied}/history?limit=1"))).await;
    assert!(page["turns"].is_array());
    assert!(server::json(host.get(format!("{proxied}/item?item=absent"))).await["item"].is_null());
    let mut stream = host
        .get(format!("{proxied}/events"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut prefix = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), async {
        while !String::from_utf8_lossy(&prefix).contains("event: snapshot") {
            prefix.extend(
                stream
                    .chunk()
                    .await
                    .unwrap()
                    .expect("missing peer snapshot"),
            );
        }
    })
    .await
    .unwrap();
    let removed = server::json(invoke(
        "kar_environment_unlink",
        json!({"environment_id":destination_id}),
    ))
    .await;
    assert_eq!(removed["result"]["structuredContent"]["linked"], false);
    assert!(
        server::json(host.get(format!("{remote_url}/access/devices")))
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    tokio::time::timeout(Duration::from_secs(8), async {
        while matches!(stream.chunk().await, Ok(Some(_))) {}
    })
    .await
    .expect("unlink must close a live viewer");
    assert!(
        !host
            .get(format!("{proxied}/history"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    // Start a fresh explicit operate pairing. The previous read-only session has been revoked.
    let readonly=server::json(host.post(format!("{remote_url}/api/auth/pairing-token")).json(&json!({"purpose":"peer-link","access":"read-only"}))).await;
    let anonymous=Client::new();
    let readonly=server::json(anonymous.post(format!("{remote_url}/access/pair")).json(&json!({"credential":readonly["credential"],"label":"read-only peer","purpose":"peer-link"}))).await;
    let forbidden=anonymous.put(format!("{remote_url}/peer/delegations/{}/{project}/{}",destination_id.as_str().unwrap(),Uuid::new_v4())).bearer_auth(readonly["token"].as_str().unwrap()).json(&json!({"task":"do not run"})).send().await.unwrap();
    assert_eq!(forbidden.status(),StatusCode::FORBIDDEN);
    let second=Uuid::new_v4();
    let grant=server::json(host.post(format!("{remote_url}/api/auth/pairing-token")).json(&json!({"purpose":"peer-link","access":"approval-required","peer_tasks":{(project):target}}))).await;
    let requested=server::json(invoke("kar_environment_link",json!({"request_id":second,"environment_id":destination_id,"url":remote_url}))).await;
    assert_eq!(requested["result"]["structuredContent"]["status"],"pending");
    server::json(host.post(format!("{cards}/{second}")).json(&json!({"accept":true,"input":{"url":remote_url,"credential":grant["credential"]}}))).await;
    let scoped=json!({"mode":"execute","environment_writes":true,"thread_tools":{"delegate":true,"remote_projects":{(destination_id.as_str().unwrap()):[project]}}});
    server::json(host.put(&perm).json(&scoped)).await;
    let work=Uuid::new_v4();
    let input=json!({"task":"write a one-line reply","command_id":work,"remote":{"environment_id":destination_id,"project_id":project}});
    let mut outside=input.clone();outside["remote"]["project_id"]=json!(Uuid::new_v4());
    let denied=server::json(invoke("kar_delegate_task",outside)).await;
    assert!(denied.get("error").is_some() || denied["result"]["isError"]==true);
    for _ in 0..2{assert_eq!(server::json(invoke("kar_delegate_task",input.clone())).await["result"]["structuredContent"]["task_id"],json!(work));}
    let done=tokio::time::timeout(Duration::from_secs(35),async{
        loop{
            let data=server::json(invoke("kar_task_status",json!({"task_id":work}))).await;
            let task=&data["result"]["structuredContent"];
            if task["work_state"]=="result_available"{break task.clone();}
            assert_ne!(task["work_state"],"failed","{task}");
            assert_ne!(task["work_state"],"unconfirmed","{task}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.unwrap();
    let remote_chat=done["thread_id"].as_str().unwrap();assert_ne!(remote_chat,work.to_string());
    let history=server::json(host.get(format!("{remote_url}/projects/{project}/chats/{remote_chat}/snapshot"))).await;
    assert!(history.to_string().contains("write a one-line reply"));
    server::json(invoke("kar_environment_unlink",json!({"environment_id":destination_id}))).await;
    remote.kill().await.unwrap();
    remote.wait().await.unwrap();
}
