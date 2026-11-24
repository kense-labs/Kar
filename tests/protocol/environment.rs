use super::{ApprovalMode, Protocol, rig};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

const DROP: &str = "KAR_TEST_ENV_DROP";
const REPLACE: &str = "KAR_TEST_ENV_REPLACE";
const EMPTY: &str = "KAR_TEST_ENV_EMPTY";
const KEEP: &str = "KAR_TEST_ENV_KEEP";
const CAPTURE: &str = "KAR_TEST_ENV_CAPTURE";

fn sample() -> Value {
    [
        DROP,
        REPLACE,
        EMPTY,
        KEEP,
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
    ]
    .into_iter()
    .filter_map(|k| Some((k.into(), Value::String(std::env::var(k).ok()?))))
    .collect::<serde_json::Map<_, _>>()
    .into()
}

pub(super) fn leaf() {
    println!("{}", sample());
}

/// Executed by the native protocol fixture before its handshake.
pub(super) fn capture() {
    let Some(file) = std::env::var_os(CAPTURE) else {
        return;
    };
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("environment-leaf")
        .output()
        .unwrap();
    assert!(output.status.success());
    let leaf: Value = serde_json::from_slice(&output.stdout).unwrap();
    fs::write(file, json!({"native":sample(),"tool":leaf}).to_string()).unwrap();
}

pub(super) async fn sessions() {
    for protocol in [
        Protocol::Codex,
        Protocol::Claude,
        Protocol::Qwen,
        Protocol::Acp,
        Protocol::Pi,
    ] {
        let (temp, runtime, mut opts) = rig(protocol, ApprovalMode::Deny);
        let capture = temp.path().join("native-environment");
        opts.remove_env(DROP).unwrap();
        opts.env
            .insert(REPLACE.into(), "session replacement".into());
        opts.env.insert(EMPTY.into(), "".into());
        opts.env.insert(CAPTURE.into(), capture.clone().into());
        let session = runtime.open(opts).await.unwrap();
        let actual: Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
        let mut expected = json!({(REPLACE):"session replacement",(EMPTY):"",(KEEP):"unchanged"});
        if protocol != Protocol::Claude {
            expected["CLAUDECODE"] = "fixture".into();
            expected["CLAUDE_CODE_ENTRYPOINT"] = "fixture".into();
        }
        assert_eq!(actual["native"], expected, "{protocol:?}");
        assert_eq!(actual["tool"], expected, "{protocol:?}");
        assert_eq!(std::env::var(DROP).unwrap(), "inherited");
        assert_eq!(std::env::var(REPLACE).unwrap(), "inherited");
        for r in session.records() {
            let packet = r.native.to_string();
            assert!(!packet.contains(CAPTURE) && !packet.contains("session replacement"));
        }
        session.close().await.unwrap();
    }
}

pub(super) fn exercise(executable: &Path) {
    let child = Command::new(executable)
        .arg("environment-sessions")
        .env(DROP, "inherited")
        .env(REPLACE, "inherited")
        .env(KEEP, "unchanged")
        .env(EMPTY, "inherited")
        .env("CLAUDECODE", "fixture")
        .env("CLAUDE_CODE_ENTRYPOINT", "fixture")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
}
