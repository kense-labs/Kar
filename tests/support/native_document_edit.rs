//! Real Codex app-server + native filesystem tool. Only the model endpoint is a loopback fixture.
use super::*;
use axum::{Json, Router, extract::State, http::header::CONTENT_TYPE, routing::post};
use std::{
    ffi::OsStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Model {
    entered: Notify,
    release: Notify,
    tools: AtomicUsize,
}

fn editor(tools: &[Value], namespace: Option<&str>) -> Option<Value> {
    let command = "printf '%s\\n' '周一' '中间' '小林' > document.md";
    for tool in tools {
        let Some(name) = tool["name"].as_str() else {
            continue;
        };
        if let Some(members) = tool["tools"].as_array()
            && let Some(item) = editor(members, Some(name))
        {
            return Some(item);
        }
        let args = match name {
            "exec_command" => json!({"cmd":command}),
            "shell_command" => json!({"command":command}),
            "shell" => json!({"command":["/bin/sh","-c",command]}),
            _ => continue,
        };
        let mut item = json!({"type":"function_call","name":name,"id":"fc_edit","call_id":"document-edit","arguments":args.to_string(),"status":"completed"});
        if let Some(namespace) = namespace {
            item["namespace"] = json!(namespace);
        }
        return Some(item);
    }
    None
}
async fn responses(
    State(model): State<Arc<Model>>,
    Json(request): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let tool_done = request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|i| i["type"] == "function_call_output" && i["call_id"] == "document-edit");
    let item = if tool_done {
        json!({"type":"message","id":"msg_done","role":"assistant","status":"completed","content":[{"type":"output_text","text":"saved","annotations":[]}]})
    } else {
        let item = editor(request["tools"].as_array().unwrap(), None)
            .expect("installed Codex advertised a shell editing tool");
        model.tools.fetch_add(1, Ordering::SeqCst);
        model.entered.notify_one();
        tokio::time::timeout(Duration::from_secs(30), model.release.notified())
            .await
            .unwrap();
        item
    };
    let response = json!({"id":"resp_edit","object":"response","status":"completed","model":"gpt-5.1","output":[item],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}});
    let body = [
        json!({"type":"response.created","response":{"id":"resp_edit","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":response}),
    ].into_iter().map(|event| format!("event: {}\ndata: {event}\n\n",event["type"].as_str().unwrap())).collect();
    ([(CONTENT_TYPE, "text/event-stream")], body)
}

#[tokio::test]
#[ignore = "installed Codex CLI on macOS; local model fixture and real native file edit"]
async fn actual_codex_merges_a_native_file_edit_with_a_concurrent_owner_save() -> Result<()> {
    let model = Arc::new(Model::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(model.clone());
    let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    let file = root.join("notes.md");
    std::fs::write(&file, "周一\n中间\n小李\n")?;
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"model = "gpt-5.1"
model_provider = "fixture"
[model_providers.fixture]
name = "Kar local fixture"
base_url = "http://{addr}/v1"
wire_api = "responses"
env_key = "KAR_FIXTURE_KEY"
"#
        ),
    )?;
    let vars = [
        ("HOME", home.as_os_str()),
        ("CODEX_HOME", home.as_os_str()),
        ("KAR_FIXTURE_KEY", OsStr::new("local-only")),
    ];
    let state = tmp.path().join("state");
    let (mut server, url) = launch_with_env(&state, &root, &vars).await?;
    let owner = client();
    let p = ok(owner
        .post(format!("{url}/projects"))
        .json(&json!({"name":"native","root":root})))
    .await?;
    let scope = format!("{url}/projects/{}", p["id"].as_str().unwrap());
    let endpoint = format!("{scope}/documents/native?path=notes.md");
    let input = json!({"runtime":"codex","instructions":"Change the last line to 小林. Keep all other lines.","base_revision":0,"operation_id":"codex-edit"});
    let request = client().post(&endpoint).json(&input);
    let mut pending = tokio::spawn(ok(request));
    tokio::select! {
        _ = tokio::time::timeout(Duration::from_secs(45), model.entered.notified()) => {
            anyhow::ensure!(model.tools.load(Ordering::SeqCst)>0,"Codex never reached the model tool request");
        },
        result = &mut pending => anyhow::bail!("native edit ended before the owner save: {result:?}"),
    }
    ok(owner
        .put(format!("{scope}/file?path=notes.md"))
        .json(&json!({"text":"周三\n中间\n小李\n","base_revision":0,"operation_id":"owner-edit"})))
    .await?;
    model.release.notify_one();
    let receipt = tokio::time::timeout(Duration::from_secs(60), pending).await???;
    assert_eq!(receipt["status"], "applied");
    assert_eq!(receipt["revision"], 2);
    assert_eq!(std::fs::read_to_string(&file)?, "周三\n中间\n小林\n");
    server.kill().await?;
    let (mut server, url) = launch_with_env(&state, &root, &vars).await?;
    let retry = format!(
        "{url}/projects/{}/documents/native?path=notes.md",
        p["id"].as_str().unwrap()
    );
    assert_eq!(ok(owner.post(retry).json(&input)).await?, receipt);
    assert_eq!(model.tools.load(Ordering::SeqCst), 1);
    server.kill().await?;
    mock.abort();
    Ok(())
}
