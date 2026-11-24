use super::*;
use kar::{OmittedOnResume, ProvidedOnResume};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

const BASE: [&str; 2] = ["RUST_BASE_ORIGINAL_4721", "RUST_BASE_CHANGED_6374"];
const APPEND: [&str; 2] = ["RUST_APPEND_ORIGINAL_9376", "RUST_APPEND_CHANGED_5918"];
type Calls = Arc<Mutex<Vec<Value>>>;

pub(super) fn settings(id: &str, root: &Path, endpoint: &str) -> SessionOptions {
    let work = root.join("project");
    let home = root.join("home");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&home).unwrap();
    let mut opts = SessionOptions::new(work);
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env.insert(key.into(), home.clone().into());
    }
    match id {
        "codex" => {
            fs::write(
                home.join("config.toml"),
                format!(
                    r#"model = "gpt-5.1"
model_provider = "fixture"
[model_providers.fixture]
name = "Loopback fixture"
base_url = "{endpoint}/v1"
env_key = "OAR_FIXTURE_KEY"
wire_api = "responses"
"#
                ),
            )
            .unwrap();
            opts.env.insert("CODEX_HOME".into(), home.into());
            opts.env
                .insert("OAR_FIXTURE_KEY".into(), "only-a-fixture".into());
        }
        "claude" => {
            opts.model = Some("sonnet".into());
            opts.env.insert("CLAUDE_CONFIG_DIR".into(), home.into());
            for (k, v) in [
                ("CLAUDE_CODE_MAX_RETRIES", "1"),
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
                ("ANTHROPIC_BASE_URL", endpoint),
                ("ANTHROPIC_API_KEY", "fixture-key-only"),
                ("ANTHROPIC_AUTH_TOKEN", ""),
            ] {
                opts.env.insert(k.into(), v.into());
            }
        }
        "pi" => {
            fs::write(home.join("models.json"), json!({"providers":{"fixture":{"baseUrl":endpoint,"api":"anthropic-messages","apiKey":"local-fixture","models":[{"id":"fixture","name":"Fixture","reasoning":false,"input":["text"],"contextWindow":200000,"maxTokens":8192}]}}}).to_string()).unwrap();
            opts.model = Some("fixture/fixture".into());
            opts.env.insert("OAR_PI_AGENT_DIR".into(), home.into());
            opts.env.insert("PI_OFFLINE".into(), "1".into());
        }
        _ => unreachable!(),
    }
    opts
}

async fn check_turn(
    session: &kar::Session,
    calls: &Calls,
    input: &str,
    expected: &[&str],
    has_history: bool,
) {
    let start = calls.lock().unwrap().len();
    let turn = session.prompt(input).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(40), turn.wait())
            .await
            .unwrap()
            .unwrap(),
        Outcome::Completed
    );
    let calls = calls.lock().unwrap();
    let mut matched = 0;
    for body in &calls[start..] {
        let Some(messages) = body
            .get("messages")
            .or_else(|| body.get("input"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let Some(user) = messages.iter().rev().find(|m| m["role"] == "user") else {
            continue;
        };
        let content = &user["content"];
        if content != input
            && !content
                .as_array()
                .is_some_and(|a| a.iter().any(|c| c["text"] == input))
        {
            continue;
        }
        matched += 1;
        let instructions = json!([
            body["system"],
            body["instructions"],
            messages
                .iter()
                .filter(|m| m["role"] == "system" || m["role"] == "developer")
                .collect::<Vec<_>>()
        ])
        .to_string();
        let observed: Vec<_> = BASE
            .into_iter()
            .chain(APPEND)
            .filter(|m| instructions.contains(m))
            .collect();
        assert_eq!(
            observed, expected,
            "effective system/developer instructions for {input}"
        );
        if has_history {
            assert!(
                messages.iter().any(|m| m["role"] == "user"
                    && m["content"].to_string().contains("seed instruction probe")),
                "missing earlier user turn"
            );
        }
    }
    assert!(matched > 0, "no native model request for {input}");
}

async fn run(id: &str) {
    let calls = Calls::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let observed = calls.clone();
    let model = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let socket = socket.into_std().unwrap();
            socket.set_nonblocking(false).unwrap();
            let observed = observed.clone();
            tokio::task::spawn_blocking(move || {
                respond_observed(socket, None, None, None, false, |path, body| {
                    if path.ends_with("/responses")
                        || path.split('?').next() == Some("/v1/messages")
                    {
                        observed.lock().unwrap().push(body.clone());
                    }
                })
            });
        }
    });
    let rt = runtime(id).unwrap();
    let decl = rt.resumed_session_options().unwrap();
    let options = [
        decl.system_prompt.unwrap(),
        decl.append_system_prompt.unwrap(),
    ];
    for change_base in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let mut opts = settings(id, root.path(), &endpoint);
        opts.settings.system_prompt = Some(BASE[0].into());
        opts.settings.append_system_prompt = Some(APPEND[0].into());
        let seed = rt.open(opts.clone()).await.unwrap();
        check_turn(
            &seed,
            &calls,
            "seed instruction probe",
            &[BASE[0], APPEND[0]],
            false,
        )
        .await;
        opts.resume = Some(seed.id().into());
        seed.close().await.unwrap();
        opts.settings.system_prompt = change_base.then(|| BASE[1].into());
        opts.settings.append_system_prompt = (!change_base).then(|| APPEND[1].into());
        let resumed = rt.open(opts).await.unwrap();
        assert_eq!(resumed.id(), seed.id());
        for compacted in [false, true] {
            if compacted {
                if !options.iter().any(|o| {
                    o.provided == ProvidedOnResume::NextCompaction
                        || o.omitted == OmittedOnResume::SavedUntilCompaction
                }) {
                    break;
                }
                let report = resumed
                    .start_compaction()
                    .await
                    .unwrap()
                    .wait(Some(Duration::from_secs(40)))
                    .await;
                assert!(
                    matches!(report.status, kar::CompactionStatus::Completed { .. }),
                    "{report:?}"
                );
            }
            let mut expected = Vec::new();
            for (markers, semantics, provided) in [
                (BASE, options[0], change_base),
                (APPEND, options[1], !change_base),
            ] {
                if provided {
                    expected.push(
                        markers[usize::from(
                            compacted || semantics.provided == ProvidedOnResume::Immediate,
                        )],
                    );
                } else if semantics.omitted == OmittedOnResume::Saved
                    || (!compacted && semantics.omitted == OmittedOnResume::SavedUntilCompaction)
                {
                    expected.push(markers[0]);
                }
            }
            let input = if compacted {
                "after compact instruction probe"
            } else {
                "resumed instruction probe"
            };
            check_turn(&resumed, &calls, input, &expected, !compacted).await;
        }
        resumed.close().await.unwrap();
    }
    model.abort();
}

#[tokio::test]
#[ignore = "requires Codex CLI with a loopback model"]
async fn codex_prompt_persistence() {
    run("codex").await;
}
#[tokio::test]
#[ignore = "requires Claude Code with a loopback model"]
async fn claude_prompt_persistence() {
    run("claude").await;
}
#[tokio::test]
#[ignore = "requires Pi RPC with a loopback model"]
async fn pi_prompt_persistence() {
    run("pi").await;
}
