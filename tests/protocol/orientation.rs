use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn snapshot(cwd: &Path, args: &[&str], case: &str) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.arg("orient")
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .env("KAR_INVENTORY_BATCH", "1")
        .env("ACCOUNT_CASE", case)
        .env("HOME", cwd)
        .env("CLAUDE_CONFIG_DIR", cwd.join("config"))
        .env_remove("KAR_STALL_INVENTORY");
    for rt in kar::runtimes() {
        let program = match rt.id.as_str() {
            "claude" | "codex" => cwd.join(&rt.id),
            _ => cwd.join("absent"),
        };
        cmd.env(format!("KAR_{}_BIN", rt.id.to_ascii_uppercase()), program);
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

pub(super) fn exercise() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    for id in ["codex", "claude"] {
        fs::copy(std::env::current_exe().unwrap(), cwd.join(id)).unwrap();
    }
    // Account fixtures refuse a model session, so an accidental open cannot satisfy these calls.
    let local = snapshot(&cwd, &["pi,claude,pi,cybion"], "out");
    assert_eq!(local["depth"], "local");
    let rows = local["runtimes"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["pi", "claude", "cybion"]
    );
    assert_eq!(
        rows[0]["providers"],
        json!({"kind":"skipped","because":"not_installed"})
    );
    assert!(rows[0].get("session_capabilities").is_none());
    assert_eq!(rows[1]["auth"], json!({"kind":"skipped","because":"depth"}));
    assert_eq!(rows[1]["installation"]["value"]["kind"], "available");
    assert!(rows[1]["session_capabilities"].is_object() && rows[1].get("models").is_none());
    assert_eq!(rows[2]["auth"]["kind"], "unsupported");
    assert_eq!(rows[2]["installation"]["value"]["via"], "bundled");
    let deep = snapshot(&cwd, &["claude,pi", "--depth", "models"], "out");
    let claude = &deep["runtimes"][0];
    assert_eq!(claude["auth"]["value"]["kind"], "logged_out");
    for key in ["account_usage", "models"] {
        assert_eq!(
            claude[key],
            json!({"kind":"skipped","because":"logged_out"})
        );
    }
    let customization = snapshot(&cwd, &["codex", "--customization"], "skills_partial");
    let result = &customization["runtimes"][0]["customization"];
    assert_eq!(result["kind"], "failed");
    assert!(
        !result
            .to_string()
            .contains("never-serialize-this-private-key")
    );
}
