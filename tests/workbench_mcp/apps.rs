use super::*;
pub(super) async fn scenario() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let state = dir.join("state");
    let root = dir.join("project");
    let db = dir.join("native");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&db).unwrap();
    let observer = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = observer.local_addr().unwrap().to_string();
    let vars = [("KAR_TEST_MCP_OBSERVER", addr.as_ref())];
    let (mut server, url) = server::serve_with_env(&state, &root, &db, &vars).await;
    let c = server::client();
    let proj = server::json(
        c.post(format!("{url}/projects"))
            .json(&json!({"root":root,"name":"cards"})),
    )
    .await;
    let project = proj["id"].as_str().unwrap();
    let chat = Uuid::new_v4();
    let turn = Uuid::new_v4();
    let chats = format!("{url}/projects/{project}/chats");
    server::json(c.post(&chats).json(&json!({"id":chat,"runtime":"codex"}))).await;
    server::json(
        c.post(format!("{chats}/{chat}/messages"))
            .json(&json!({"id":turn,"text":"KAR-MCP-APP-ITEM"})),
    )
    .await;
    let api = format!("{chats}/{chat}/turns/{turn}/mcp-apps/card");
    let open = server::json(c.post(format!("{api}/open"))).await;
    let stream = &open["stream"];
    assert_eq!(open["app"]["resource_uri"], "ui://weather/main");
    assert_eq!(
        c.post(format!("{chats}/{chat}/turns/{turn}/mcp-apps/forged/open"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let request = |v| c.post(format!("{api}/requests")).json(&v);
    let info = server::json(request(
        json!({"action":"tool_info","name":"lookup","stream":stream}),
    ))
    .await;
    assert_eq!(info["tool"]["inputSchema"]["type"], "object");
    assert_eq!(info["readOnly"], true);
    let resource = server::json(request(
        json!({"action":"read_resource","uri":"ui://weather/aux","stream":stream}),
    ))
    .await;
    assert_eq!(resource["contents"][0]["uri"], "ui://weather/aux");
    let cmd = Uuid::new_v4();
    let call = json!({"action":"call_tool","name":"lookup","arguments":{"station":"ABC"},"command_id":cmd,"stream":stream});
    for _ in 0..2 {
        assert_eq!(
            server::json(request(call.clone())).await["structuredContent"]["station"],
            "ABC"
        );
    }
    for field in [
        json!({"name":"hidden"}),
        json!({"arguments":{"station":"XYZ"}}),
        json!({"stream":Uuid::new_v4()}),
    ] {
        let mut denied = call.clone();
        denied
            .as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        assert_eq!(
            request(denied).send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        fs::read_to_string(db.join("mcp-user-calls")).unwrap(),
        "call\n"
    );
    let context =
        |chat: Uuid| json!({"conversation":chat,"content":[{"type":"text","text":"WIND-ROW-17"}]});
    server::json(c.post(format!("{api}/context")).json(&context(chat))).await;
    let other = Uuid::new_v4();
    server::json(c.post(&chats).json(&json!({"id":other,"runtime":"codex"}))).await;
    assert_eq!(
        c.post(format!("{api}/context"))
            .json(&context(other))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    server.kill().await.unwrap();
    let (mut restarted, base) = server::serve_with_env(&state, &root, &db, &vars).await;
    let user = "KAR-MCP-APP-CONTEXT-NEXT";
    let reply = server::json(
        c.post(format!("{base}/projects/{project}/chats/{chat}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":user})),
    )
    .await;
    assert_eq!(reply["status"], "completed");
    let wire = fs::read_to_string(db.join("app-wire-next")).unwrap();
    assert!(wire.contains("WIND-ROW-17") && wire.contains(user));
    restarted.kill().await.unwrap();
}
