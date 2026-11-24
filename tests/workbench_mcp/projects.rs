use super::*;

pub(super) async fn exercise(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    root: &Path,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let directory = root.parent().unwrap().join("model-projects");
    fs::create_dir(&directory).unwrap();
    let configure = |mode: &str| {
        host.put(&policy).json(&json!({"mode":mode,"project_access":{"manage":true,"launch":true,"directories":[directory]}}))
    };
    let call = |name: &str, args: Value| rpc("tools/call", json!({"name":name,"arguments":args}));
    let create = Uuid::new_v4();
    let request = json!({"command_id":create,"title":"New model project"});
    server::json(configure("plan")).await;
    assert!(server::json(call("kar_project_create", request.clone())).await["error"].is_object());
    server::json(configure("execute")).await;
    let first = server::json(call("kar_project_create", request.clone())).await;
    let saved = &first["result"]["structuredContent"];
    assert_eq!(saved["id"], json!(create), "{first}");
    assert!(
        Path::new(saved["root"].as_str().unwrap())
            .join("README.md")
            .is_file()
    );
    assert_eq!(
        server::json(call("kar_project_create", request)).await["result"]["structuredContent"],
        *saved
    );
    let change =
        json!({"command_id":Uuid::new_v4(),"project_id":create,"settings":{"title":"Renamed"}});
    assert_eq!(
        server::json(call("kar_project_update", change)).await["result"]["structuredContent"]["name"],
        "Renamed"
    );
    let get = json!({"project_id":create});
    assert_eq!(
        server::json(call("kar_project_read", get)).await["result"]["structuredContent"]["name"],
        "Renamed"
    );
    let id = Uuid::new_v4();
    let launch = json!({"command_id":id,"project_id":create,"title":"New conversation","workspace":"root","runtime":"codex"});
    let launched = server::json(call("kar_thread_launch", launch.clone())).await;
    assert_eq!(
        launched["result"]["structuredContent"]["thread_id"],
        json!(id),
        "{launched}"
    );
    assert_eq!(
        server::json(call("kar_thread_launch", launch)).await["result"]["structuredContent"],
        launched["result"]["structuredContent"]
    );
    let del = json!({"command_id":Uuid::new_v4(),"project_id":create,"force":true});
    assert!(server::json(call("kar_project_delete",del)).await["result"]["structuredContent"]["deleted"].as_bool().unwrap_or(false));
    assert!(
        Path::new(saved["root"].as_str().unwrap())
            .join("README.md")
            .is_file()
    );
}
