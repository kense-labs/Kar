#[path = "support/declaration.rs"]
mod declaration;

use axum::{Json, Router, body::Body, extract::State, http::Response, routing::post};
use kar::{NativeMemory, NativeMemoryDefault, Outcome, SessionOptions, chat::ChatStore, runtime};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

type Requests = Arc<Mutex<Vec<Value>>>;

async fn chat(State(requests): State<Requests>, Json(body): Json<Value>) -> Response<Body> {
    let model = body["model"].clone();
    let messages = body["messages"].clone();
    let slow = messages
        .as_array()
        .and_then(|a| a.last())
        .is_some_and(|m| m.to_string().contains("slow marker"));
    requests.lock().unwrap().push(body.clone());
    if slow {
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    let mut sse = String::new();
    for (delta, stop) in [
        (
            json!({"role":"assistant","content":"qwen-rust-reply"}),
            Value::Null,
        ),
        (json!({}), json!("stop")),
    ] {
        let event = json!({"id":"chatcmpl-local","object":"chat.completion.chunk","created":1,"model":model,
            "choices":[{"index":0,"delta":delta,"finish_reason":stop}]});
        sse.push_str(&format!("data: {event}\n\n"));
    }
    let usage = json!({"id":"chatcmpl-local","object":"chat.completion.chunk","created":1,"model":model,"choices":[],"usage":{"prompt_tokens":17,"completion_tokens":4,"total_tokens":21}});
    sse.push_str(&format!("data: {usage}\n\n"));
    sse.push_str("data: [DONE]\n\n");
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    opts: SessionOptions,
    requests: Requests,
    http: tokio::task::JoinHandle<()>,
}

async fn fixture() -> Fixture {
    let requests = Requests::default();
    let router = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir_all(home.join(".qwen")).unwrap();
    let settings = json!({"modelProviders":{"openai":[{"id":"gpt-5.1","name":"Loopback model","baseUrl":base,"envKey":"OPENAI_API_KEY"}]},
        "security":{"auth":{"selectedType":"openai"}},"model":{"name":"gpt-5.1"}});
    std::fs::write(home.join(".qwen/settings.json"), settings.to_string()).unwrap();
    let mut opts = SessionOptions::new(&cwd);
    for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME"] {
        opts.env.insert(key.into(), home.as_os_str().into());
    }
    for (k, v) in [
        ("OPENAI_API_KEY", "qwen-local-fixture"),
        ("OPENAI_BASE_URL", &base),
        ("OPENAI_MODEL", "gpt-5.1"),
        ("NO_PROXY", "*"),
    ] {
        opts.env.insert(k.into(), v.into());
    }
    opts.env
        .insert("QWEN_HOME".into(), home.join(".qwen").into());
    opts.env.insert(
        "QWEN_CODE_SYSTEM_SETTINGS_PATH".into(),
        dir.path().join("system-settings.json").into(),
    );
    opts.model = Some("gpt-5.1".into());
    Fixture {
        dir,
        opts,
        requests,
        http,
    }
}

#[tokio::test]
#[ignore = "requires Qwen Code; uses an isolated home and a loopback model"]
async fn actual_qwen_turn_catalog_history_and_interrupt() {
    let Fixture {
        dir: _dir,
        mut opts,
        requests,
        http,
    } = fixture().await;
    opts.settings.system_prompt = Some("QWEN-BASE-RS".into());
    opts.settings.append_system_prompt = Some("QWEN-APPEND-RS".into());
    opts.settings.disallowed_tools = vec!["read_file".into()];
    let qwen = runtime("qwen").unwrap();
    let declared = declaration::for_installation(&qwen, &opts).await;
    let session = qwen.open(opts.clone()).await.unwrap();
    declaration::check(&declared, session.capabilities());
    assert!(!session.capabilities().image_input);
    let catalog = session.models().await.unwrap();
    assert!(
        catalog
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "gpt-5.1"),
        "{catalog}"
    );
    let turn = session.prompt("first history marker").await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(35), turn.report())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, Outcome::Completed);
    assert_eq!(report.text, "qwen-rust-reply");
    let total = session.usage().total.expect("actual Qwen token report");
    assert_eq!((total.input, total.output), (17, 4));
    assert_eq!(session.model().as_deref(), Some("gpt-5.1"));
    if session
        .info()
        .pointer("/capabilities/can_get_context_usage")
        == Some(&Value::Bool(true))
    {
        let current = session
            .current_context()
            .expect("Qwen context before the turn handle resolves");
        let usage = session.context_usage().await.unwrap();
        assert_eq!(current.context_window, usage["contextWindowSize"].as_u64());
        assert_eq!(usage["subtype"], "get_context_usage");
        assert!(usage["contextWindowSize"].as_u64().is_some_and(|n| n > 0));
    }
    opts.resume = Some(session.id().into());
    session.close().await.unwrap();
    opts.settings.system_prompt = Some("QWEN-CHANGED-BASE".into());
    opts.settings.append_system_prompt = Some("QWEN-CHANGED-APPEND".into());
    let resumed = qwen.open(opts.clone()).await.unwrap();
    assert_eq!(resumed.id(), session.id());
    let report = tokio::time::timeout(
        Duration::from_secs(35),
        resumed
            .prompt("second history marker")
            .await
            .unwrap()
            .report(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.text, "qwen-rust-reply");
    assert!(resumed.usage().total.is_none());
    assert!(requests.lock().unwrap().iter().any(|m| {
        let s = m.to_string();
        s.contains("first history marker")
            && s.contains("second history marker")
            && s.contains("qwen-rust-reply")
    }));
    for (probe, expected, stale) in [
        (
            "first history marker",
            ["QWEN-BASE-RS", "QWEN-APPEND-RS"],
            ["QWEN-CHANGED-BASE", "QWEN-CHANGED-APPEND"],
        ),
        (
            "second history marker",
            ["QWEN-CHANGED-BASE", "QWEN-CHANGED-APPEND"],
            ["QWEN-BASE-RS", "QWEN-APPEND-RS"],
        ),
    ] {
        let requests = requests.lock().unwrap();
        let request = requests
            .iter()
            .find(|r| r["messages"].to_string().contains(probe))
            .unwrap();
        let system = &request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "system")
            .unwrap()["content"];
        let system = system.to_string();
        assert!(
            expected.iter().all(|s| system.contains(s))
                && stale.iter().all(|s| !system.contains(s)),
            "Qwen native instructions for {probe}"
        );
        let tools = request["tools"].as_array().unwrap();
        assert!(
            tools.iter().any(|t| t["function"]["name"] == "write_file")
                && !tools.iter().any(|t| t["function"]["name"] == "read_file"),
            "Qwen native tool exclusions"
        );
    }
    let slow = resumed.prompt("slow marker").await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !requests
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.to_string().contains("slow marker"))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    resumed.abort().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), slow.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Aborted
    );
    resumed.close().await.unwrap();
    opts.settings.system_prompt = None;
    opts.settings.append_system_prompt = None;
    let defaults = qwen.open(opts).await.unwrap();
    assert_eq!(defaults.id(), session.id());
    let turn = defaults.prompt("third history marker").await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(35), turn.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Completed
    );
    {
        let calls = requests.lock().unwrap();
        let messages = calls
            .iter()
            .rev()
            .find(|r| r["messages"].to_string().contains("third history marker"))
            .unwrap()["messages"]
            .as_array()
            .unwrap();
        let system =
            messages.iter().find(|m| m["role"] == "system").unwrap()["content"].to_string();
        assert!(
            system.len() > 50
                && !system.contains("QWEN-CHANGED-")
                && !system.contains("QWEN-BASE-RS")
                && !system.contains("QWEN-APPEND-RS")
        );
        assert!(
            messages.iter().any(|m| m["role"] == "user"
                && m["content"].to_string().contains("first history marker"))
        );
    }
    defaults.close().await.unwrap();
    http.abort();
}

#[tokio::test]
#[ignore = "requires Qwen Code 0.26+; uses isolated settings and a loopback model"]
async fn native_memory_default_survives_chat_restart_and_user_settings_take_precedence() {
    use uuid::Uuid;
    let Fixture {
        dir,
        mut opts,
        requests,
        http,
    } = fixture().await;
    let store_path = dir.path().join("chats");
    let defaults = dir.path().join("system-defaults.json");
    let original = "{ /* existing defaults */ \"memory\": { \"enableManagedAutoMemory\": true } }";
    std::fs::write(&defaults, original).unwrap();
    opts.env.insert(
        "QWEN_CODE_SYSTEM_DEFAULTS_PATH".into(),
        defaults.clone().into(),
    );
    opts.settings.native_memory_default = Some(NativeMemoryDefault::Off);
    let store = ChatStore::open(&store_path).unwrap();
    let chat = store.create("qwen", None, opts.clone()).await.unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    assert_eq!(chat.capabilities().native_memory, Some(NativeMemory::Off));
    for prompt in [
        "remember that my favorite color is chartreuse",
        "what is 2 + 2",
    ] {
        assert_eq!(
            chat.send(Uuid::new_v4(), prompt).await.unwrap().status,
            kar::chat::TurnStatus::Completed
        );
    }
    chat.close().await.unwrap();
    drop(store);
    const CATALOG: &str = "# Current auto-memory catalog";
    {
        let seen = requests.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            !seen
                .iter()
                .any(|r| r["messages"].to_string().contains(CATALOG))
        );
    }
    let user = dir.path().join("home/.qwen/settings.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&user).unwrap()).unwrap();
    settings["memory"] = json!({"enableManagedAutoMemory":true});
    std::fs::write(user, settings.to_string()).unwrap();
    let store = ChatStore::open(store_path).unwrap();
    let resumed = store
        .resume_with(id, |launch| launch.env = opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    assert_eq!(
        resumed.info().settings.native_memory_default,
        Some(NativeMemoryDefault::Off)
    );
    assert_eq!(resumed.capabilities().native_memory, Some(NativeMemory::On));
    assert_eq!(
        resumed
            .send(Uuid::new_v4(), "user enabled memory marker")
            .await
            .unwrap()
            .status,
        kar::chat::TurnStatus::Completed
    );
    resumed.close().await.unwrap();
    assert!(
        requests.lock().unwrap().iter().skip(2).any(|r| {
            let messages = r["messages"].to_string();
            messages.contains("user enabled memory marker") && messages.contains(CATALOG)
        }),
        "expected the native memory catalog in the resumed model request"
    );
    assert_eq!(std::fs::read_to_string(defaults).unwrap(), original);
    http.abort();
}

#[path = "support/public_readbacks.rs"]
mod public_readbacks;

#[tokio::test]
#[ignore = "real Qwen Code with a loopback model"]
async fn public_prompt_readbacks() {
    let f = fixture().await;
    let session = runtime("qwen").unwrap().open(f.opts.clone()).await.unwrap();
    public_readbacks::check(&session).await.unwrap();
    public_readbacks::stream::check(&session).await.unwrap();
    session.close().await.unwrap();
    f.http.abort();
}
