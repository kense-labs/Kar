//! Real npm Pi, an isolated config directory, and a local HTTP model service.
use axum::{Json, Router, http::StatusCode, routing::post};
use kar::{
    Error, ModelRequests, Runtime, SessionOptions,
    chat::{ChatReplica, ChatStore, event_router},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

#[path = "support/sse.rs"]
mod sse;
const MARKER: &str = "private-pi-payload-marker";
const SECRET: &str = "only-in-private-pi-request";
const TRANSFORM: &str = r#"export default function(pi) {
    pi.on("before_provider_request", async ({ payload }) => {
        await Promise.resolve();
        return { ...payload, oar_test_field: "private-pi-payload-marker", oar_secret: process.env.OAR_TEST_SECRET };
    });
}"#;
struct Fixture {
    temp: tempfile::TempDir,
    agent: PathBuf,
    runtime: Runtime,
    options: SessionOptions,
    endpoint: String,
    wire: mpsc::Receiver<(Value, oneshot::Sender<()>)>,
    model: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.model.abort();
    }
}
impl Fixture {
    async fn new(extension: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let agent = base.join("agent");
        let work = base.join("work");
        fs::create_dir_all(agent.join("extensions")).unwrap();
        fs::create_dir(&work).unwrap();
        fs::write(agent.join("extensions/test.ts"), extension).unwrap();
        fs::write(
            agent.join("settings.json"),
            r#"{"retry":{"enabled":false},"compaction":{"enabled":false}}"#,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (requests, wire) = mpsc::channel(8);
        let router = Router::new().route("/{*path}",post(move |Json(body):Json<Value>| {
            let requests = requests.clone();
            async move {
                let (release,wait) = oneshot::channel();
                requests.send((body,release)).await.unwrap();
                let _ = wait.await;
                (StatusCode::UNAUTHORIZED,Json(json!({"error":{"type":"authentication_error","message":"fixture declined"}})))
            }
        }));
        let model = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut runtime = kar::runtime("pi").unwrap();
        runtime.program = std::env::var_os("OAR_PI_BIN").unwrap().into();
        let mut options = SessionOptions::new(&work);
        for key in [
            "HOME",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
        ] {
            options.env.insert(key.into(), base.join(key).into());
        }
        options.env.insert("PI_OFFLINE".into(), "1".into());
        options
            .env
            .insert("OAR_PI_AGENT_DIR".into(), agent.clone().into());
        options.env.insert("OAR_TEST_SECRET".into(), SECRET.into());
        options.settings.model_requests = Some(ModelRequests::Latest);
        options.model = Some("fixture/fixture".into());
        Self {
            temp,
            agent,
            runtime,
            options,
            endpoint,
            wire,
            model,
        }
    }
    fn models(&self, api: &str) {
        let provider = |api: &str| json!({"baseUrl":self.endpoint,"api":api,"apiKey":"loopback-test","models":[{"id":"fixture","name":"Fixture","reasoning":false,"input":["text"],"contextWindow":200000,"maxTokens":8192}]});
        fs::write(
            self.agent.join("models.json"),
            json!({"providers":{"fixture":provider(api),"other":provider("google-generative-ai")}})
                .to_string(),
        )
        .unwrap();
    }
    async fn request(&mut self) -> (Value, oneshot::Sender<()>) {
        tokio::time::timeout(Duration::from_secs(10), self.wire.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn public<T: serde::Serialize>(data: &T) {
    let text = serde_json::to_string(data).unwrap();
    assert!(!text.contains(MARKER) && !text.contains(SECRET));
}

#[tokio::test]
#[ignore = "requires npm Pi and Node with native addon support; checks the wire before any provider response"]
async fn final_root_body_for_three_apis() {
    let mut f = Fixture::new(TRANSFORM).await;
    for (api, format) in [
        ("anthropic-messages", "anthropic.messages"),
        ("openai-completions", "openai.chat"),
        ("openai-responses", "openai.responses"),
    ] {
        f.models(api);
        let session = f.runtime.open(f.options.clone()).await.unwrap();
        assert!(session.capabilities().model_requests);
        assert!(session.last_model_request().await.unwrap().is_none());
        let turn = session.prompt("read-root-capture").await.unwrap();
        let (mut actual, release) = f.request().await;
        let snapshot = session.last_model_request().await.unwrap().unwrap();
        assert_eq!(snapshot.format, format);
        assert_eq!(snapshot.model.as_deref(), Some("fixture"));
        assert_eq!(snapshot.provider.as_deref(), Some("fixture"));
        assert!(snapshot.at.parse::<jiff::Timestamp>().is_ok());
        assert_eq!(actual["oar_test_field"], MARKER);
        assert_eq!(actual["oar_secret"], SECRET);
        actual["oar_secret"] = json!("[redacted]");
        assert_eq!(snapshot.body.as_ref(), Some(&actual));
        release.send(()).unwrap();
        let _ = turn.wait().await;
        for record in session.records() {
            public(record.as_ref());
        }
        let mut reopen = f.options.clone();
        reopen.resume = Some(session.id().to_owned());
        session.close().await.unwrap();
        assert!(session.last_model_request().await.unwrap().is_none());
        let resumed = f.runtime.open(reopen).await.unwrap();
        assert!(resumed.last_model_request().await.unwrap().is_none());
        resumed.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires npm Pi; exercises SDK startup hooks and a second SDK agent in the same process"]
async fn startup_model_switch_and_nested_root_isolation() {
    let extension = r#"import { createRequire } from "node:module";
    import { writeFileSync } from "node:fs";
    export default function(pi) {
        pi.on("session_start",async (_,ctx) => {
            if (process.env.OAR_TEST_SWITCH) {
                writeFileSync("startup-hook-entered","true");
                await pi.setModel(ctx.modelRegistry.find("other","fixture"));
                return;
            }
            const { createAgentSession, SessionManager, DefaultResourceLoader } = createRequire(process.argv[1])("./index.js");
            const cwd = ctx.cwd, agentDir = process.env.PI_CODING_AGENT_DIR;
            const loader = new DefaultResourceLoader({cwd,agentDir,noExtensions:true});
            await loader.reload();
            const {session} = await createAgentSession({cwd,agentDir,model:ctx.model,sessionManager:SessionManager.inMemory(cwd),resourceLoader:loader});
            try {
                await session.bindExtensions({mode:"rpc"});
                await session.prompt("only-the-nested-agent");
            } finally { session.dispose(); }
        });
    }"#;
    let mut f = Fixture::new(extension).await;
    f.models("anthropic-messages");
    let mut switched = f.options.clone();
    switched.model = None;
    switched.env.insert("OAR_TEST_SWITCH".into(), "1".into());
    // Explicitly select fixture first so the extension actually changes the API.
    let mut settings: Value =
        serde_json::from_slice(&fs::read(f.agent.join("settings.json")).unwrap()).unwrap();
    settings["defaultProvider"] = json!("fixture");
    settings["defaultModel"] = json!("fixture");
    fs::write(f.agent.join("settings.json"), settings.to_string()).unwrap();
    assert!(matches!(
        f.runtime.open(switched.clone()).await,
        Err(Error::Unsupported(_))
    ));
    let marker = f.options.cwd.join("startup-hook-entered");
    fs::remove_file(&marker).unwrap();
    switched.model = Some("other/fixture".into());
    assert!(matches!(
        f.runtime.open(switched).await,
        Err(Error::Unsupported(_))
    ));
    assert!(!marker.exists());
    let rt = f.runtime.clone();
    let options = f.options.clone();
    let opening = tokio::spawn(async move { rt.open(options).await });
    let (nested, release) = f.request().await;
    assert!(
        nested["messages"]
            .to_string()
            .contains("only-the-nested-agent")
    );
    release.send(()).unwrap();
    let root = opening.await.unwrap().unwrap();
    assert!(root.last_model_request().await.unwrap().is_none());
    let turn = root.prompt("only-the-root").await.unwrap();
    let (actual, release) = f.request().await;
    let request = root
        .last_model_request()
        .await
        .unwrap()
        .unwrap()
        .body
        .unwrap();
    assert_eq!(request, actual);
    assert!(request["messages"].to_string().contains("only-the-root"));
    assert!(
        !request["messages"]
            .to_string()
            .contains("only-the-nested-agent")
    );
    release.send(()).unwrap();
    let _ = turn.wait().await;
    root.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires npm Pi; proves two SSE clients resume without publishing model request bodies"]
async fn persistent_chat_two_subscribers() {
    let mut f = Fixture::new(TRANSFORM).await;
    f.models("anthropic-messages");
    let store = ChatStore::open(f.temp.path().join("chats")).unwrap();
    let chat = store
        .create("pi", Some(Path::new(&f.runtime.program)), f.options.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let base = store.feed_snapshot(id).unwrap();
    let start = base.cursor.to_string();
    let mut first = ChatReplica::from_snapshot(store.feed_snapshot(id).unwrap());
    let mut second = ChatReplica::from_snapshot(base);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = event_router(store.clone(), id);
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut a = sse::SseClient::open(&url, &start, None).await;
    let mut b = sse::SseClient::open(&url, &start, None).await;
    let send = chat.send(uuid::Uuid::new_v4(), "public-message");
    let inspect = async {
        let (_, release) = f.request().await;
        let req = chat.last_model_request().await.unwrap().unwrap();
        assert_eq!(req.body.unwrap()["oar_test_field"], MARKER);
        let event = b.next().await;
        public(&event);
        second.apply(event).unwrap();
        drop(b);
        release.send(()).unwrap();
    };
    let (outcome, ()) = tokio::join!(send, inspect);
    outcome.unwrap();
    chat.close().await.unwrap();
    let resumed = store
        .resume_with(id, |o| o.env = f.options.env.clone())
        .await
        .unwrap();
    assert!(resumed.last_model_request().await.unwrap().is_none());
    resumed.close().await.unwrap();
    let mut b = sse::SseClient::open(&url, &start, Some(&second.cursor().to_string())).await;
    let tip = store.feed_snapshot(id).unwrap();
    public(&tip);
    for (view, client) in [(&mut first, &mut a), (&mut second, &mut b)] {
        while view.cursor() != tip.cursor {
            let event = client.next().await;
            public(&event);
            assert!(view.apply(event.clone()).unwrap());
            assert!(!view.apply(event).unwrap());
        }
    }
    assert_eq!(first.turns(), second.turns());
    http.abort();
}

#[tokio::test]
#[ignore = "requires npm Pi; exercises per-request model selection and JSON encoding failures"]
async fn later_unobservable_request_supersedes_previous_snapshot() {
    let mut f = Fixture::new(
        r#"export default function(pi) {
        pi.on("input",async ({text},ctx) => {
            const provider = text === "change-api" ? "other" : "fixture";
            await pi.setModel(ctx.modelRegistry.find(provider,"fixture"));
            return { action:"continue" };
        });
        pi.on("before_provider_request", ({payload}) => {
            if (JSON.stringify(payload).includes("make-circular")) {
                payload.oar_cycle = payload;
                return payload;
            }
        });
    }"#,
    )
    .await;
    f.models("anthropic-messages");
    let session = f.runtime.open(f.options.clone()).await.unwrap();
    for (text, valid) in [
        ("first-visible", true),
        ("change-api", false),
        ("visible-again", true),
    ] {
        let turn = session.prompt(text).await.unwrap();
        let (actual, release) = f.request().await;
        let snapshot = session.last_model_request().await;
        if valid {
            assert_eq!(snapshot.unwrap().unwrap().body, Some(actual));
        } else {
            assert!(matches!(snapshot, Err(Error::Unsupported(_))));
        }
        release.send(()).unwrap();
        let _ = turn.wait().await;
    }
    let turn = session.prompt("make-circular").await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), turn.wait())
        .await
        .unwrap();
    assert!(matches!(
        session.last_model_request().await,
        Err(Error::Protocol(_))
    ));
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires npm Pi; a startup handler starts the first root prompt while binding"]
async fn first_request_started_by_a_user_extension() {
    let mut f = Fixture::new(
        r#"export default function(pi) {
        let built;
        const seen = new Promise(resolve => { built = resolve; });
        pi.on("session_start", async () => {
            pi.sendUserMessage("startup-root-prompt");
            await seen;
        });
        pi.on("before_provider_request",({payload}) => {
            built();
            return { ...payload, startup_field:"from-user-extension" };
        });
    }"#,
    )
    .await;
    f.models("anthropic-messages");
    let rt = f.runtime.clone();
    let opts = f.options.clone();
    let opening = tokio::spawn(async move { rt.open(opts).await });
    let (actual, release) = f.request().await;
    let root = tokio::time::timeout(Duration::from_secs(5), opening)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(actual["startup_field"], "from-user-extension");
    assert!(
        actual["messages"]
            .to_string()
            .contains("startup-root-prompt")
    );
    assert_eq!(
        root.last_model_request().await.unwrap().unwrap().body,
        Some(actual)
    );
    release.send(()).unwrap();
    root.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires npm Pi; checks opt-in capture availability and ordinary RPC fallback"]
async fn node_with_addons_disabled() {
    let mut f = Fixture::new(TRANSFORM).await;
    f.models("openai-completions");
    f.options
        .env
        .insert("NODE_OPTIONS".into(), "--no-addons".into());
    assert!(matches!(
        f.runtime.open(f.options.clone()).await,
        Err(Error::Unsupported(_))
    ));
    f.options.settings.model_requests = None;
    let normal = f.runtime.open(f.options.clone()).await.unwrap();
    assert!(!normal.capabilities().model_requests);
    assert!(matches!(
        normal.last_model_request().await,
        Err(Error::Unsupported(_))
    ));
    normal.close().await.unwrap();
}

#[tokio::test]
async fn a_protocol_launcher_without_the_vendor_sdk() {
    let work = tempfile::tempdir().unwrap();
    let mut options = SessionOptions::new(work.path());
    options.settings.model_requests = Some(ModelRequests::Latest);
    let rt = Runtime::new(
        "pi",
        std::env::current_exe().unwrap(),
        &["--mode", "rpc"],
        kar::Protocol::Pi,
    );
    assert!(matches!(rt.open(options).await, Err(Error::Unsupported(_))));
}
