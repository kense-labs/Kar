use super::*;
use kar::{
    Catalog, Error,
    forms::{ListingReason, ModelListing},
};

#[tokio::test]
#[ignore = "requires Qwen Code and an isolated native profile"]
async fn catalog_distinguishes_missing_auth_empty_and_configured() {
    let f = fixture().await;
    let rt = runtime("qwen").unwrap();
    super::effort::configure(
        &f,
        "gpt-5.1",
        Some(json!({"efforts":["low","medium","high"],"defaultEffort":"medium"})),
    );
    let mut unsigned = f.opts.clone();
    let settings = f.dir.path().join("home/.qwen/settings.json");
    let saved = std::fs::read(&settings).unwrap();
    std::fs::remove_file(&settings).unwrap();
    unsigned.model = None;
    for (key, _) in std::env::vars_os() {
        if let Some(key) = key.to_str()
            && !matches!(key, "PATH" | "SystemRoot" | "TMPDIR" | "TEMP" | "TMP")
            && !key.is_empty()
            && !key.contains('=')
        {
            unsigned.unset_env.insert(key.to_owned());
        }
    }
    for key in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL"] {
        unsigned.remove_env(key).unwrap();
    }
    assert!(matches!(
        rt.form_model_listing(&unsigned, vec![]).await,
        ModelListing::Unavailable {
            reason: ListingReason::Unauthenticated
        }
    ));
    let mut empty = unsigned;
    empty.env = f.opts.env.clone();
    assert_eq!(
        rt.catalog(Catalog::Models, empty.clone()).await.unwrap(),
        json!([])
    );
    assert!(matches!(
        rt.form_model_listing(&empty, vec![]).await,
        ModelListing::Unavailable {
            reason: ListingReason::Empty
        }
    ));
    std::fs::write(settings, saved).unwrap();
    let ModelListing::Live { models } = rt.form_model_listing(&empty, vec![]).await else {
        panic!("configured catalog must be live")
    };
    assert_eq!(models.len(), 1);
    let model = &models[0];
    assert_eq!(model.id, "gpt-5.1");
    assert_eq!(model.metadata["displayName"], "Loopback model");
    assert_eq!(model.metadata["defaultEffort"], "medium");
    assert_eq!(
        model.effort_levels.as_ref().unwrap(),
        &["low", "medium", "high"]
    );
    assert!(f.requests.lock().unwrap().is_empty());
    f.http.abort();
}

#[tokio::test]
#[ignore = "requires Qwen Code and a loopback model"]
async fn session_launch_arguments_are_per_process() {
    let f = fixture().await;
    let rt = runtime("qwen").unwrap();
    for token in [
        "--outputFormat=json",
        "--resume=kar-private-argument",
        "-pkar-private-argument",
        "--",
        "--approval-mode",
    ] {
        let mut opts = f.opts.clone();
        opts.launch_args.push(token.into());
        match rt.open(opts).await {
            Err(Error::Unsupported(message)) => assert!(!message.contains("kar-private-argument")),
            Err(e) => panic!("wrong refusal for managed option: {e}"),
            Ok(s) => {
                s.close().await.unwrap();
                panic!("managed option accepted");
            }
        }
    }
    let mut opts = f.opts.clone();
    opts.launch_args.push("--auth-type=openai".into());
    let store = ChatStore::open(f.dir.path().join("chats")).unwrap();
    let chat = store.create("qwen", Some(&rt.program), opts).await.unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    assert_eq!(
        chat.send(uuid::Uuid::new_v4(), "QWEN_ARGS_FIRST")
            .await
            .unwrap()
            .status,
        kar::chat::TurnStatus::Completed
    );
    chat.close().await.unwrap();
    let resumed = store
        .resume_with(id, |opts| {
            assert!(opts.launch_args.is_empty());
            opts.env = f.opts.env.clone();
            opts.launch_args.push("--auth-type=openai".into());
        })
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    resumed.close().await.unwrap();
    let saved =
        std::fs::read_to_string(f.dir.path().join("chats").join(format!("{id}.karchat"))).unwrap();
    assert!(!saved.contains("--auth-type=openai"));
    f.http.abort();
}
