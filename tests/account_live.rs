//! Real vendor account operations using private profiles and synthetic credentials.
use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn status(runtime: &str, key: Option<&str>) -> Value {
    operations(runtime, key, &["status"]).pop().unwrap()
}

fn operations(runtime: &str, key: Option<&str>, actions: &[&str]) -> Vec<Value> {
    let variable = format!("KAR_{}_BIN", runtime.to_uppercase());
    let vendor = std::env::var_os(&variable).unwrap_or_else(|| panic!("set {variable}"));
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("profile");
    fs::create_dir(&profile).unwrap();
    let synthetic = "sk-isolated-account-rust-test-12345";
    if runtime == "codex" {
        fs::write(
            profile.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        if key.is_some() {
            fs::write(
                profile.join("auth.json"),
                serde_json::json!({"OPENAI_API_KEY":synthetic}).to_string(),
            )
            .unwrap();
        }
    }
    actions
        .iter()
        .map(|action| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
            cmd.current_dir(home.path())
                .env_clear()
                .env(&variable, &vendor)
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("TMPDIR", home.path())
                .env("TEMP", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .env("XDG_DATA_HOME", home.path())
                .env("APPDATA", home.path())
                .env("LOCALAPPDATA", home.path())
                .env("FACTORY_HOME_OVERRIDE", home.path())
                .env("FACTORY_DISABLE_KEYRING", "1")
                .env("AGENT_CLI_CREDENTIAL_STORE", "file")
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
                .env(
                    "QWEN_CODE_SYSTEM_SETTINGS_PATH",
                    home.path().join("system.json"),
                );
            for name in [
                "CODEX_HOME",
                "QWEN_HOME",
                "CLAUDE_CONFIG_DIR",
                "QODER_HOME",
                "KIMI_CODE_HOME",
                "KIMI_SHARE_DIR",
            ] {
                cmd.env(name, &profile);
            }
            if let Some(path) = std::env::var_os("SystemRoot") {
                cmd.env("SystemRoot", Path::new(&path));
            }
            if let Some(key) = key {
                cmd.env(key, synthetic);
            }
            let out = cmd
                .args(["account", runtime, action, "--json"])
                .output()
                .unwrap();
            assert!(
                out.status.success() && out.stderr.is_empty(),
                "{runtime} status failed"
            );
            assert!(!String::from_utf8_lossy(&out.stdout).contains(synthetic));
            serde_json::from_slice(&out.stdout).unwrap()
        })
        .collect()
}

#[test]
#[ignore = "requires KAR_CODEX_BIN"]
fn codex_file_login() {
    assert_eq!(status("codex", None)["kind"], "logged_out");
    let login = status("codex", Some("OPENAI_API_KEY"));
    assert_eq!(login["kind"], "logged_in");
    assert_eq!(login["account"]["credential"], "api_key");
}

#[test]
#[ignore = "requires KAR_CLAUDE_BIN"]
fn claude_key_status() {
    let login = status("claude", Some("ANTHROPIC_API_KEY"));
    assert_eq!(login["kind"], "logged_in");
    assert_eq!(login["account"]["credential"], "api_key");
}

#[test]
#[ignore = "requires KAR_FACTORY_BIN"]
fn factory_unvalidated_key() {
    let status = status("factory", Some("FACTORY_API_KEY"));
    assert_eq!(status["kind"], "unknown");
    assert_eq!(status["configured"], serde_json::json!(["FACTORY_API_KEY"]));
}

#[test]
#[ignore = "requires KAR_KIMI_BIN with native ACP"]
fn kimi_empty_profile() {
    assert_eq!(status("kimi", None)["kind"], "logged_out");
}

#[test]
#[ignore = "requires KAR_QODER_BIN"]
fn qoder_empty_profile() {
    assert_eq!(status("qoder", None)["kind"], "logged_out");
}

#[test]
#[ignore = "requires KAR_CURSOR_BIN"]
fn cursor_file_store() {
    assert_eq!(status("cursor", None)["kind"], "logged_out");
    let key = status("cursor", Some("CURSOR_API_KEY"));
    assert_eq!(key["kind"], "unknown");
    assert_eq!(
        key["configured"],
        serde_json::json!(["environment: CURSOR_API_KEY"])
    );
}

#[test]
#[ignore = "requires KAR_CODEX_BIN; only removes a synthetic key in a temporary file store"]
fn codex_confirms_private_logout() {
    let v = operations(
        "codex",
        Some("OPENAI_API_KEY"),
        &["status", "logout", "status", "logout"],
    );
    assert_eq!(
        v.iter()
            .map(|v| v["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["logged_in", "logged_out", "logged_out", "logged_out"]
    );
}

#[test]
#[ignore = "requires KAR_FACTORY_BIN; no keyring and only a synthetic environment key"]
fn droid_confirms_active_environment_key() {
    let v = operations(
        "factory",
        Some("FACTORY_API_KEY"),
        &["status", "logout", "status"],
    );
    assert_eq!(v[0]["kind"], "unknown");
    assert_eq!(v[1]["reason"], "still_logged_in");
    assert_eq!(v[2]["configured"], serde_json::json!(["FACTORY_API_KEY"]));
}

#[test]
#[ignore = "requires KAR_CURSOR_BIN; only accesses a temporary file store"]
fn cursor_confirms_empty_file_store_logout() {
    let v = operations("cursor", None, &["logout", "status"]);
    assert_eq!(v[0]["kind"], "logged_out");
    assert_eq!(v[1]["kind"], "logged_out");
}

#[test]
#[ignore = "requires KAR_CODEX_BIN; uses a temporary file store"]
fn codex_usage_recognizes_non_subscription_accounts() {
    let signed_out = operations("codex", None, &["usage"]);
    assert_eq!(signed_out[0]["kind"], "reauth_required");
    let api_key = operations("codex", Some("OPENAI_API_KEY"), &["usage"]);
    assert_eq!(api_key[0]["reason"], "unsupported_auth_mode");
}

#[test]
#[ignore = "requires KAR_GROK_BIN; uses an empty temporary profile"]
fn grok_usage_signed_out() {
    assert_eq!(
        operations("grok", None, &["usage"])[0]["kind"],
        "reauth_required"
    );
}

#[test]
#[ignore = "requires KAR_ANTIGRAVITY_BIN; uses an empty temporary profile"]
fn antigravity_usage_signed_out() {
    assert_eq!(
        operations("antigravity", None, &["usage"])[0]["kind"],
        "reauth_required"
    );
}
