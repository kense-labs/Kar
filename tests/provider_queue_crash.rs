//! SIGKILL a real Kar owner while the native CLI has no way to publish its host receipt.
#![cfg(unix)]
#[path = "provider_queue_crash/model.rs"]
mod model;
#[path = "support/native_group.rs"]
mod native_group;
#[path = "provider_queue_crash/observe.rs"]
mod observe;

use kar::{
    Protocol, SessionOptions,
    chat::{ChatStore, InputStatus, TurnStatus},
};
use native_group::{NativeGroup, wait};
use nix::sys::signal::Signal;
use serde_json::Value;
use std::{fs, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::timeout,
};
use uuid::Uuid;

const SEED: &str = "KAR_CUT_SEED";
const HELD: &str = "KAR_CUT_HELD";
const NEXT: &str = "KAR_CUT_NEXT";
const LAST: &str = "KAR_CUT_LAST";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires real Kimi KAP, private process groups and loopback"]
async fn kimi_kap_sigkill_after_native_acceptance() {
    crash("kimi", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires real Codex app-server, private process groups and loopback"]
async fn codex_sigkill_after_native_acceptance() {
    crash("codex", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires real Kimi KAP, private process groups and loopback"]
async fn kimi_kap_sigkill_before_native_acceptance() {
    crash("kimi", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires real Codex app-server, private process groups and loopback"]
async fn codex_sigkill_before_native_acceptance() {
    crash("codex", false).await;
}

async fn crash(vendor: &str, accepted: bool) {
    timeout(Duration::from_secs(160), exercise(vendor, accepted))
        .await
        .expect("provider crash case timed out");
}

async fn exercise(vendor: &str, accepted: bool) {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().canonicalize().unwrap();
    let work = private.join("work");
    let state = private.join("state");
    let chats = private.join("chats");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&state).unwrap();
    let fixture = model::Fixture::start().await;
    let mut opts = SessionOptions::new(&work);
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        opts.env.insert(key.into(), private.as_os_str().into());
    }
    let (variable, protocol) = if vendor == "kimi" {
        opts.env
            .insert("KIMI_CODE_HOME".into(), state.as_os_str().into());
        opts.env.insert("KIMI_DISABLE_TELEMETRY".into(), "1".into());
        opts.env.insert(
            "KIMI_CODE_MODEL_CATALOG_REFRESH_ON_START".into(),
            "0".into(),
        );
        opts.model = Some("fixture".into());
        fs::write(
            state.join("config.toml"),
            format!(
                r#"default_model = "fixture"
auto_session_title = false
[loop_control]
max_attempts_per_step = 1
[providers.local]
type = "openai"
base_url = "{}/v1"
api_key = "local-test"
[models.fixture]
provider = "local"
model = "fixture"
max_context_size = 65536
"#,
                fixture.origin
            ),
        )
        .unwrap();
        ("KAR_KIMI_BIN", Protocol::KimiKap)
    } else {
        opts.env
            .insert("CODEX_HOME".into(), state.as_os_str().into());
        opts.env
            .insert("KAR_FIXTURE_KEY".into(), "local-test".into());
        fs::write(
            state.join("config.toml"),
            format!(
                r#"model = "gpt-5.1"
model_provider = "fixture"
[model_providers.fixture]
name = "loopback"
base_url = "{}/v1"
env_key = "KAR_FIXTURE_KEY"
wire_api = "responses"
"#,
                fixture.origin
            ),
        )
        .unwrap();
        ("KAR_CODEX_BIN", Protocol::Codex)
    };
    let bin = PathBuf::from(
        std::env::var_os(variable).unwrap_or_else(|| panic!("{variable} is required")),
    )
    .canonicalize()
    .unwrap();
    let version = Command::new(&bin)
        .current_dir(&work)
        .envs(&opts.env)
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    let store = ChatStore::open(&chats).unwrap();
    let chat = store
        .create(vendor, Some(&bin), opts.clone())
        .await
        .unwrap();
    let id = chat.info().id;
    let root = chat.info().native_id.clone();
    assert_eq!(chat.info().protocol, protocol);
    let seed = Uuid::new_v4();
    assert_eq!(
        chat.send(seed, SEED).await.unwrap().status,
        TurnStatus::Completed
    );
    chat.close().await.unwrap();

    let mut cli = Command::new(env!("CARGO_BIN_EXE_kar"))
        .current_dir(&work)
        .envs(&opts.env)
        .env("KAR_CHAT_DIR", &chats)
        .args(["chat", "--json", "resume", "--chat", &id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(cli.stdout.take().unwrap()).lines();
    let ready: Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ready["chat"]["native_id"], root);
    let group = NativeGroup::of(cli.id().unwrap());
    group.signal(Signal::SIGSTOP);
    let keys = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (key, prompt) in keys.into_iter().zip([HELD, NEXT, LAST]) {
        assert_eq!(
            store.enqueue(id, key, prompt).unwrap().status,
            InputStatus::Pending
        );
    }
    wait(
        || {
            store
                .history(id)
                .unwrap()
                .turns
                .last()
                .is_some_and(|t| t.id == keys[0])
        },
        "host input begin",
    )
    .await;
    // A shared lock allows independent readers while holding back all future journal appends.
    let path = chats.join(format!("{id}.karchat"));
    let lock = fs::File::open(&path).unwrap();
    lock.lock_shared().unwrap();
    let before = fs::metadata(&path).unwrap().len();
    let history = store.history(id).unwrap();
    let first = history.turns.last().unwrap();
    assert_eq!(first.id, keys[0]);
    assert!(!first.input_sent && first.native_turn_id.is_none());
    if accepted {
        group.signal(Signal::SIGCONT);
        wait(
            || fixture.calls().contains(&HELD),
            "model received the held input from the real vendor",
        )
        .await;
    }
    cli.kill().await.unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        cli.wait().await.unwrap().signal(),
        Some(Signal::SIGKILL as i32)
    );
    drop(group);
    assert_eq!(fs::metadata(&path).unwrap().len(), before);
    drop(lock);
    fixture.release();
    assert_eq!(
        fixture.calls(),
        if accepted {
            vec![SEED, HELD]
        } else {
            vec![SEED]
        }
    );
    let store = ChatStore::open(&chats).unwrap();
    let status = || {
        store
            .queued_inputs(id)
            .unwrap()
            .into_iter()
            .map(|i| i.status)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        status(),
        [
            InputStatus::Uncertain,
            InputStatus::Pending,
            InputStatus::Pending
        ]
    );

    // Read the actual vendor snapshot independently of ChatStore and its recovery projection.
    let opener = observe::opener(vendor, &bin, &opts, &root, keys[0]).await;
    if !accepted {
        assert!(opener.is_none(), "frozen provider received an input");
    }
    let resume = || store.resume_with(id, |o| o.env.clone_from(&opts.env));
    let restored = resume().await.unwrap();
    assert_eq!(restored.info().native_id, root);
    assert_eq!(restored.info().workspace, work);
    restored.drain_queue().await.unwrap();
    let head_status = if let Some(ref native) = opener {
        let history = store.history(id).unwrap();
        let t = history.turns.iter().find(|t| t.id == keys[0]).unwrap();
        assert_eq!(t.native_turn_id.as_deref(), Some(native.as_str()));
        assert!(t.input_sent);
        InputStatus::Sent
    } else {
        assert_eq!(
            status(),
            [
                InputStatus::Uncertain,
                InputStatus::Pending,
                InputStatus::Pending
            ]
        );
        assert!(store.queued_inputs(id).unwrap()[0].needs_acknowledgement);
        assert!(!fixture.calls().contains(&NEXT));
        let cmd = Uuid::new_v4().to_string();
        for _ in 0..2 {
            let result = Command::new(env!("CARGO_BIN_EXE_kar"))
                .current_dir(&work)
                .envs(&opts.env)
                .env("KAR_CHAT_DIR", &chats)
                .args([
                    "chat",
                    "--json",
                    "acknowledge-uncertain",
                    &keys[0].to_string(),
                    "--chat",
                    &id.to_string(),
                    "--command-id",
                    &cmd,
                ])
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let receipt: Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(receipt["command_id"], cmd);
            assert!(receipt["uncertain_head"].is_null());
        }
        restored.drain_queue().await.unwrap();
        assert!(store.queued_inputs(id).unwrap()[0].uncertainty_acknowledged);
        InputStatus::Uncertain
    };
    restored.close().await.unwrap();
    let again = resume().await.unwrap();
    again.drain_queue().await.unwrap();
    again.close().await.unwrap();
    assert_eq!(
        status(),
        [head_status, InputStatus::Sent, InputStatus::Sent]
    );
    assert_eq!(
        store.enqueue(id, keys[0], HELD).unwrap().status,
        head_status
    );
    let history = store.history(id).unwrap();
    assert_eq!(
        history.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
        [seed, keys[0], keys[1], keys[2]]
    );
    assert_eq!(
        fixture.calls(),
        if accepted {
            vec![SEED, HELD, NEXT, LAST]
        } else {
            vec![SEED, NEXT, LAST]
        }
    );
    eprintln!(
        "{} {} / {} {}: SIGKILL, accepted={accepted}, root={root}, host UUID={}, independent native opener={opener:?}, status={head_status:?}, loopback order={:?}",
        vendor,
        String::from_utf8_lossy(&version.stdout).trim(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        keys[0],
        fixture.calls()
    );
}
