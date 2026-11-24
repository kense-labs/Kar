use super::*;

const BASE: &str = "Rust 根聊天 {{literal}}\n第二行";
const TAIL: &str = "附加 {{not_a_variable}}";

pub(super) fn system(request: &Value) -> String {
    // The DeepSeek route can carry a full replacement as a later system-role
    // message. Pure tool deltas in the same channel do not replace the text.
    if let Some(update) = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .filter(|message| message["role"] == "system")
        .find_map(|message| {
            let text: Vec<_> = message["content"]
                .as_array()?
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            (!text.is_empty()).then(|| text.join("\n\n"))
        })
    {
        return update;
    }
    let value = &request["system"];
    if let Some(text) = value.as_str() {
        return text.into();
    }
    value
        .as_array()
        .expect("Anthropic system blocks")
        .iter()
        .map(|b| b["text"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[tokio::test]
#[ignore = "requires real DSH Web"]
async fn native_system_prompt() {
    scoped_prompts(Protocol::Dsh).await;
}
#[tokio::test]
#[ignore = "requires real DSH ACP"]
async fn legacy_system_prompt() {
    scoped_prompts(Protocol::Acp).await;
}

async fn scoped_prompts(protocol: Protocol) {
    let tb = testbed(protocol).await;
    let rules = tb.runtime.resumed_session_options().unwrap();
    for rule in [
        rules.system_prompt.unwrap(),
        rules.append_system_prompt.unwrap(),
    ] {
        assert_eq!(rule.provided, kar::ProvidedOnResume::Immediate);
        assert_eq!(rule.omitted, kar::OmittedOnResume::Default);
    }
    let mut opts = tb.opts.clone();
    opts.settings.system_prompt = Some(BASE.into());
    opts.settings.append_system_prompt = Some(TAIL.into());
    opts.settings.disallowed_tools.push("bash".into());
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), opts)
        .await
        .unwrap();
    let (id, native) = (chat.info().id, chat.info().native_id.clone());
    let first = Uuid::new_v4();
    let expected = format!("{BASE}\n\n{TAIL}");
    assert_eq!(
        chat.send(first, "DSH_PROMPT_INITIAL").await.unwrap().status,
        TurnStatus::Completed
    );
    {
        let data = tb.data.requests.lock().unwrap();
        let request = data.last().unwrap();
        assert!(
            !request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "bash")
        );
        assert_eq!(system(request), expected);
    }
    let root = chat.send(Uuid::new_v4(), "DSH_PROMPT_PARENT delegate");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(25), root)
            .await
            .unwrap()
            .unwrap()
            .status,
        TurnStatus::Completed
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            {
                let calls = tb.data.prompt_sessions.lock().unwrap();
                if let Some((_, child)) = calls
                    .iter()
                    .find(|(session, _)| !session.is_empty() && *session != native)
                {
                    assert!(!child.is_empty() && !child.contains(BASE) && !child.contains(TAIL));
                    assert!(
                        calls
                            .iter()
                            .filter(|(session, _)| *session == native)
                            .all(|(_, prompt)| *prompt == expected)
                    );
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("separate native child request");
    let sibling = tb
        .store
        .create("dsh", Some(&tb.runtime.program), tb.opts.clone())
        .await
        .unwrap();
    sibling
        .send(Uuid::new_v4(), "DSH_PROMPT_SIBLING")
        .await
        .unwrap();
    let default = system(tb.data.requests.lock().unwrap().last().unwrap());
    assert!(!default.is_empty() && !default.contains(BASE) && !default.contains(TAIL));
    sibling.close().await.unwrap();
    chat.close().await.unwrap();
    let store = ChatStore::open(tb.dir.path().join("store")).unwrap();
    let resumed = store
        .resume_with(id, |o| o.env = tb.opts.env.clone())
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    resumed
        .send(Uuid::new_v4(), "DSH_PROMPT_RESUME")
        .await
        .unwrap();
    assert_eq!(
        system(tb.data.requests.lock().unwrap().last().unwrap()),
        expected
    );
    resumed.close().await.unwrap();
    let mut changed = tb.opts.clone();
    changed.resume = Some(native.clone());
    changed.settings.append_system_prompt = Some(TAIL.into());
    let appended = tb.runtime.open(changed).await.unwrap();
    let outcome = appended
        .prompt("DSH_PROMPT_APPEND")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(outcome, kar::Outcome::Completed);
    let text = system(tb.data.requests.lock().unwrap().last().unwrap());
    assert!(text.ends_with(TAIL) && text.len() > TAIL.len() && !text.contains(BASE));
    appended.close().await.unwrap();
    if protocol == Protocol::Dsh {
        let branch = store
            .fork_at_with(id, first, |o| {
                o.env = tb.opts.env.clone();
                o.settings.append_system_prompt = None;
                o.settings.system_prompt = Some("fork {{literal}}".into());
            })
            .await
            .unwrap();
        assert_ne!(branch.info().native_id, native);
        branch
            .send(Uuid::new_v4(), "DSH_PROMPT_FORK")
            .await
            .unwrap();
        assert_eq!(
            system(tb.data.requests.lock().unwrap().last().unwrap()),
            "fork {{literal}}"
        );
        branch.close().await.unwrap();
    }
    let mut omitted = tb.opts.clone();
    omitted.resume = Some(native);
    let native = tb.runtime.open(omitted).await.unwrap();
    native
        .prompt("DSH_PROMPT_DEFAULT")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let text = system(tb.data.requests.lock().unwrap().last().unwrap());
    assert!(!text.is_empty() && !text.contains(BASE) && !text.contains(TAIL));
    native.close().await.unwrap();
}

fn hook(tb: &Testbed, mode: &str) {
    let file = tb.dir.path().join("prompt-hook.node");
    if !file.exists() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh_prompt_hook.rs");
        let mut cmd = std::process::Command::new("rustc");
        cmd.args(["--edition=2024", "--crate-type=cdylib", "-Awarnings"]);
        if cfg!(target_os = "macos") {
            cmd.args(["-Clink-arg=-undefined", "-Clink-arg=dynamic_lookup"]);
        }
        assert!(
            cmd.arg(source)
                .arg("-o")
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
    }
    let home = tb.dir.path().join("home/.dsh");
    std::fs::create_dir_all(&home).unwrap();
    let row = json!([{"insert":[{"id":"fixture-prompt-hook","name":url::Url::from_file_path(file).unwrap().as_str(),"config":{"mode":mode}}]}]);
    std::fs::write(home.join("cordis.patch.yml"), row.to_string()).unwrap();
}

#[tokio::test]
#[ignore = "requires real DSH Web"]
async fn native_system_prompt_creation_hook() {
    creation_hook(Protocol::Dsh).await;
}
#[tokio::test]
#[ignore = "requires real DSH ACP"]
async fn legacy_system_prompt_creation_hook() {
    creation_hook(Protocol::Acp).await;
}

async fn creation_hook(protocol: Protocol) {
    let tb = testbed(protocol).await;
    hook(&tb, "auto");
    let mut opts = tb.opts.clone();
    opts.settings.system_prompt = Some(BASE.into());
    opts.settings.disallowed_tools = vec!["bash".into()];
    let session = tb.runtime.open(opts.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(12), async {
        while tb.data.requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("native creation hook delivered its queued input");
    {
        let reqs = tb.data.requests.lock().unwrap();
        assert_eq!(system(&reqs[0]), BASE);
        assert!(
            reqs[0]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["name"] != "bash"
                    && !tool["name"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("kar_startup_")),
            "startup input saw an excluded tool"
        );
    }
    session.close().await.unwrap();
    if protocol == Protocol::Dsh {
        let count = tb.data.requests.lock().unwrap().len();
        let mut invalid = opts.clone();
        invalid.model = Some("missing-provider/no-such-model".into());
        assert!(tb.runtime.open(invalid).await.is_err());
        assert_eq!(
            tb.data.requests.lock().unwrap().len(),
            count,
            "native input ran before model validation"
        );
    }
    hook(&tb, "conflict");
    for append in [false, true] {
        opts.settings.system_prompt = (!append).then(|| BASE.into());
        opts.settings.append_system_prompt = append.then(|| TAIL.into());
        let count = tb.data.requests.lock().unwrap().len();
        let err = tb
            .runtime
            .open(opts.clone())
            .await
            .err()
            .expect("native complete section conflicts");
        let setting = if append {
            "append_system_prompt"
        } else {
            "system_prompt"
        };
        assert!(
            matches!(&err, kar::Error::Unsupported(reason) if reason.contains(setting)),
            "{err}"
        );
        assert_eq!(
            tb.data.requests.lock().unwrap().len(),
            count,
            "creation hook reached the model before verification"
        );
    }
}
