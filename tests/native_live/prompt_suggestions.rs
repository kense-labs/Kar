//! Installed CLI, isolated vendor home and a scripted local model. No protocol peer.
use super::{Outcome, SessionOptions, respond_observed, runtime};
use kar::PromptSuggestions;
use serde_json::Value;
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

async fn check(provider: &str) {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&home).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let server = tokio::spawn({
        let calls = calls.clone();
        async move {
            while let Ok((socket, _)) = listener.accept().await {
                let calls = calls.clone();
                tokio::task::spawn_blocking(move || {
                    let socket = socket.into_std().unwrap();
                    socket.set_nonblocking(false).unwrap();
                    let _ = respond_observed(socket, None, None, None, false, |_, body| {
                        calls.lock().unwrap().push(body.clone());
                    });
                });
            }
        }
    });
    let mut opts = SessionOptions::new(work);
    opts.env.insert("HOME".into(), home.clone().into());
    if provider == "codex" {
        fs::write(home.join("config.toml"),format!("model = \"gpt-5.1\"\nmodel_provider = \"fixture\"\n[model_providers.fixture]\nname = \"loopback\"\nbase_url = \"{address}/v1\"\nenv_key = \"KAR_FIXTURE_KEY\"\nwire_api = \"responses\"\n")).unwrap();
        opts.env.insert("CODEX_HOME".into(), home.into());
        opts.env
            .insert("KAR_FIXTURE_KEY".into(), "local-fixture".into());
    } else {
        opts.env.insert("CLAUDE_CONFIG_DIR".into(), home.into());
        for (key, value) in [
            ("ANTHROPIC_BASE_URL", address.as_str()),
            ("ANTHROPIC_API_KEY", "local-fixture"),
            ("ANTHROPIC_AUTH_TOKEN", ""),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "true"),
        ] {
            opts.env.insert(key.into(), value.into());
        }
        opts.model = Some("sonnet".into());
    }
    let hub = PromptSuggestions::new(true);
    let chat = Uuid::new_v4();
    let mut changed = hub.changes();
    opts.prompt_suggestions = Some(hub.source(chat));
    let rt = runtime(provider).unwrap();
    let session = rt.open(opts).await.unwrap();
    let input = Uuid::new_v4();
    let turn = session
        .prompt_with_id(
            input,
            "KAR-NEXT-PROMPT-PROBE: describe one missing calculator function in a sentence.",
        )
        .await
        .unwrap();
    assert_eq!(turn.wait().await.unwrap(), Outcome::Completed);
    let predicted = tokio::time::timeout(Duration::from_secs(55), async {
        loop {
            if let Some(current) = hub.current(chat) {
                return current;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("installed provider did not publish a next prompt");
    assert_eq!(predicted.turn_id, input);
    assert!(!predicted.text.trim().is_empty());
    assert!(
        session
            .records()
            .iter()
            .all(|r| r.native["type"] != "prompt_suggestion"
                && r.native["method"] != "thread/sample"
                && r.native.pointer("/result/sourceTurnId").is_none())
    );
    if provider == "codex" {
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|v| v.to_string().contains("Predict the next message"))
        );
    }
    hub.set_enabled(false);
    assert!(hub.current(chat).is_none());
    session.close().await.unwrap();
    server.abort();
}
#[tokio::test]
#[ignore = "requires installed Codex thread/sample and a loopback Responses API"]
async fn codex_samples_the_completed_native_turn() {
    check("codex").await;
}

#[tokio::test]
#[ignore = "requires installed Claude Code SDK prompt_suggestion events and a loopback model"]
async fn claude_emits_a_next_prompt_for_the_completed_root_turn() {
    check("claude").await;
}
