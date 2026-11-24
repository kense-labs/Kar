use kar::{
    InstallationOptions,
    account::{AuthStatus, AuthStatusOptions},
};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::{self, BufRead},
    path::Path,
    time::Duration,
};

#[path = "account_login.rs"]
mod login;
#[path = "login_commands.rs"]
mod login_commands;
#[path = "account_logout.rs"]
mod logout;

#[path = "account_billing.rs"]
mod billing;
#[path = "account_quota.rs"]
mod quota;
#[path = "skill_inventory.rs"]
mod skills;

const SECRET: &str = "never-serialize-this-private-key";

pub(super) fn peer(id: &str, args: &[String]) {
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let mode = std::env::var("ACCOUNT_CASE").unwrap_or_default();
    if args == ["--version"] {
        let v = match id {
            "claude" if mode == "logout_old" => "2.0.0 (Claude Code)",
            "antigravity" if mode.starts_with("skills_") => "1.5.15",
            "pi" if mode.starts_with("skills_") => "1.6.0",
            "kimi" => "kimi 2.3.3",
            "claude" => "2.1.330 (Claude Code)",
            "codex" if mode == "login_old" => "codex-cli 0.110.0",
            "codex" => "codex-cli 0.171.0",
            "devin" => "devin 1.2.3",
            "grok" => "grok 1.0.77",
            "qoder" if mode == "old" => "1.1.69",
            _ => "1.5.10",
        };
        println!("{v}");
        return;
    }
    if args.last() == Some(&"--help") {
        if id == "claude" && mode.starts_with("quota_") && mode != "quota_old" {
            println!("  --safe-mode   Disable external integrations");
        }
        println!(
            "Usage: codex app-server\nUsage: droid exec\nUsage: kimi acp\nUsage: grok agent stdio\nUsage: qodercli\nUsage: agent acp\nUsage of agy --conversation\nUsage: codebuddy\nUsage: qwen"
        );
        return;
    }
    if let Some(case) = mode.strip_prefix("skills_") {
        return skills::peer(id, case, &args);
    }
    if let Some(case) = mode.strip_prefix("quota_") {
        if id == "kimi" {
            return billing::peer(case, &args);
        }
        return quota::peer(id, case, &args);
    }
    if let Some(case) = mode.strip_prefix("login_") {
        if matches!(id, "grok" | "qoder" | "kimi" | "codebuddy" | "cursor") {
            return login_commands::peer(id, case, &args);
        }
        return login::peer(id, case, &args);
    }
    if let Some(case) = mode.strip_prefix("logout_") {
        return logout::peer(id, case, &args);
    }
    let text = match (id, args.as_slice()) {
        ("codex", ["login", "status"]) => {
            if mode == "error" {
                eprintln!("{SECRET}");
                std::process::exit(1)
            }
            eprintln!("Logged in using ChatGPT");
            return;
        }
        ("claude", ["auth", "status", "--json"]) => {
            assert!(std::env::var_os("CLAUDECODE").is_none());
            if mode == "out" {
                println!(r#"{{"loggedIn":false}}"#);
                std::process::exit(1)
            }
            json!({"loggedIn":true,"authMethod":"claude.ai","email":"reader@example.org","subscriptionType":"native","orgId":"org","token":SECRET}).to_string()
        }
        ("qoder", ["status", "--output", "json"]) => {
            if mode == "hang" {
                std::thread::sleep(Duration::from_secs(60));
            }
            if mode == "malformed" {
                return println!(r#"{{"logged_in":"false","detail":"{SECRET}"}}"#);
            }
            json!({"logged_in":true,"private":SECRET}).to_string()
        }
        ("cursor", ["status","--format","json"]) if mode=="partial" => json!({"status":"partially-authenticated","isAuthenticated":false,"accessToken":SECRET}).to_string(),
        ("cursor", ["status","--format","json"]) => json!({"status":"authenticated","isAuthenticated":true,"userInfo":{"userId":9,"email":"cursor@example.org","refreshToken":SECRET}}).to_string(),
        ("grok", ["models"]) => format!("You are logged in with grok.com.\nBearer {SECRET}"),
        ("devin", ["auth", "status"]) if mode == "unverified" => {
            "Logged in (via Devin).\n  Unavailable: private".into()
        }
        ("devin", ["auth", "status"]) if mode == "rejected" => {
            "Logged in.\n  Failed to fetch from server: invalid API key".into()
        }
        ("devin", ["auth", "status"]) => format!(
            "Logged in (via Devin).\nUser:\n  Email: reader@example.org\nAccount:\n  Plan: team\n  Token: {SECRET}"
        ),
        ("antigravity", ["models"]) => {
            eprintln!("Please sign in.");
            std::process::exit(1);
        }
        ("kimi", ["acp"])
        | ("factory", ["exec", "--output-format", "acp"])
        | (
            "codebuddy",
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ],
        ) => {
            let stdin = io::stdin();
            let mut lines = stdin.lock().lines();
            let mut read =
                || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
            let first = read();
            if id == "codebuddy" {
                assert_eq!(first["request"]["subtype"], "initialize");
                for req in [json!("stale"), first["request_id"].clone()] {
                    println!(
                        "{}",
                        json!({"type":"control_response","response":{"request_id":req,"subtype":"success","response":{"account":{"access_token":SECRET,"email":"private@invalid"}}}})
                    );
                }
                return;
            }
            assert_eq!(first["method"], "initialize");
            assert_eq!(first["params"]["clientCapabilities"]["terminal"], false);
            let caps = match mode.as_str() {
                "unsupported" => json!({}),
                _ => json!({"auth":{"status":{}}}),
            };
            println!(
                "{}",
                json!({"jsonrpc":"2.0","id":first["id"],"result":{"protocolVersion":1,"agentCapabilities":caps,"authMethods":[{"id":"login"}]}})
            );
            if mode == "unsupported" {
                return;
            }
            let second = read();
            println!(
                "{}",
                json!({"jsonrpc":"2.0","id":second["id"],"method":"terminal/create","params":{"secret":SECRET}})
            );
            assert_eq!(read()["error"]["code"], -32601);
            let sid = &second["id"];
            if id == "factory" {
                assert_eq!(second["method"], "auth/status");
                let label = if mode == "key" {
                    "Factory API key"
                } else {
                    "Managed"
                };
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0","id":sid,"result":{"state":"authenticated","accounts":[{"active":true,"label":label,"email":"reader@example.org","token":SECRET}]}})
                );
            } else {
                assert_eq!(second["method"], "authenticate");
                assert_eq!(second["params"]["methodId"], "login");
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0","id":sid,"error":{"code":-32000,"message":SECRET}})
                );
            }
            return;
        }
        _ => panic!("Unexpected account query"),
    };
    println!("{text}");
}

fn opts(home: &Path, id: &str, mode: &str) -> AuthStatusOptions {
    let mut env: std::collections::BTreeMap<OsString, Option<OsString>> =
        std::env::vars_os().map(|(k, _)| (k, None)).collect();
    for k in [
        "HOME",
        "USERPROFILE",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "QWEN_HOME",
    ] {
        env.insert(k.into(), Some(home.as_os_str().into()));
    }
    for (k, v) in [
        ("ACCOUNT_CASE", mode),
        ("OAR_ACCOUNT_TEST", id),
        ("CLAUDECODE", "1"),
    ] {
        env.insert(k.into(), Some(v.into()));
    }
    env.insert(
        "QWEN_CODE_SYSTEM_SETTINGS_PATH".into(),
        Some(home.join("system.json").into_os_string()),
    );
    let installation = InstallationOptions {
        env,
        cwd: Some(home.into()),
    };
    AuthStatusOptions {
        installation,
        timeout: Some(Duration::from_secs(5)),
    }
}

async fn check(id: &str, options: &AuthStatusOptions, kind: &str) -> Value {
    let mut rt = kar::runtime(id).unwrap();
    rt.program = std::env::current_exe().unwrap();
    let status = rt.auth_status(options).await.unwrap();
    let value = serde_json::to_value(status).unwrap();
    assert_eq!(value["kind"], kind, "{id}: {value}");
    assert!(!value.to_string().contains(SECRET));
    value
}
pub(super) async fn exercise() {
    skills::exercise().await;
    billing::exercise().await;
    quota::exercise().await;
    logout::exercise().await;
    login::exercise().await;
    login_commands::exercise().await;
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path();
    let prefs = h.join(".claude.json");
    fs::write(&prefs,json!({"oauthAccount":{"emailAddress":"READER@example.org","organizationUuid":"org","accountUuid":"actor","planDisplayName":"Team Plus","displayName":"Reader","refreshToken":SECRET}}).to_string()).unwrap();
    let value = check("claude", &opts(h, "claude", "ok"), "logged_in").await;
    assert_eq!(value["account"]["accountId"], "actor");
    assert_eq!(value["account"]["plan"], "Team Plus");
    assert_eq!(
        check("claude", &opts(h, "claude", "out"), "logged_out").await["lastAccount"]["displayName"],
        "Reader"
    );
    fs::write(&prefs,r#"{"oauthAccount":{"emailAddress":"reader@example.org","organizationUuid":"different","accountUuid":"stale"}}"#).unwrap();
    assert!(
        check("claude", &opts(h, "claude", "ok"), "logged_in").await["account"]["accountId"]
            .is_null()
    );
    fs::write(
        h.join("auth.json"),
        json!({"tokens":{"account_id":"codex-actor","refresh_token":SECRET}}).to_string(),
    )
    .unwrap();
    fs::write(h.join("config.toml"), "cli_auth_credentials_store='file'\n").unwrap();
    assert_eq!(
        check("codex", &opts(h, "codex", "ok"), "logged_in").await["account"]["accountId"],
        "codex-actor"
    );
    fs::write(
        h.join("config.toml"),
        "[profiles.other]\ncli_auth_credentials_store='auto'\n",
    )
    .unwrap();
    assert!(
        check("codex", &opts(h, "codex", "ok"), "logged_in").await["account"]["accountId"]
            .is_null()
    );
    check("codex", &opts(h, "codex", "error"), "unknown").await;
    check("qoder", &opts(h, "qoder", "ok"), "logged_in").await;
    check("qoder", &opts(h, "qoder", "malformed"), "unknown").await;
    assert_eq!(
        check("qoder", &opts(h, "qoder", "old"), "unknown").await["code"],
        "version_unsupported"
    );
    let mut short = opts(h, "qoder", "hang");
    short.timeout = Some(Duration::from_millis(250));
    assert_eq!(
        check("qoder", &short, "unknown").await["detail"],
        "Account query timed out"
    );
    check("antigravity", &opts(h, "antigravity", "ok"), "logged_out").await;
    let mut other = opts(h, "antigravity", "ok");
    other
        .installation
        .env
        .insert("GEMINI_HOME".into(), Some(h.join("other").into_os_string()));
    assert!(
        check("antigravity", &other, "unknown").await["detail"]
            .as_str()
            .unwrap()
            .contains("GEMINI_HOME")
    );
    assert_eq!(
        check("grok", &opts(h, "grok", "ok"), "logged_in").await["account"]["method"],
        "grok.com"
    );
    assert_eq!(
        check("devin", &opts(h, "devin", "ok"), "logged_in").await["account"]["plan"],
        "team"
    );
    check("devin", &opts(h, "devin", "rejected"), "logged_out").await;
    check("devin", &opts(h, "devin", "unverified"), "unknown").await;
    assert!(
        check("codebuddy", &opts(h, "codebuddy", "ok"), "logged_in").await["account"].is_null()
    );
    assert_eq!(
        check("factory", &opts(h, "factory", "ok"), "logged_in").await["account"]["email"],
        "reader@example.org"
    );
    check("factory", &opts(h, "factory", "unsupported"), "unknown").await;
    assert_eq!(
        check("factory", &opts(h, "factory", "key"), "unknown").await["configured"],
        json!(["FACTORY_API_KEY"])
    );
    check("kimi", &opts(h, "kimi", "ok"), "logged_out").await;
    for (id, key) in [
        ("codebuddy", "CODEBUDDY_API_KEY"),
        ("devin", "WINDSURF_API_KEY"),
    ] {
        let mut o = opts(h, id, "ok");
        o.installation.env.insert(key.into(), Some(SECRET.into()));
        assert_eq!(
            check(id, &o, "unknown").await["configured"],
            json!([format!("environment: {key}")])
        );
    }
    assert_eq!(
        check("cursor", &opts(h, "cursor", "ok"), "logged_in").await["account"]["accountId"],
        "9"
    );
    assert_eq!(
        check("cursor", &opts(h, "cursor", "partial"), "unknown").await["configured"],
        json!(["stored access token"])
    );
    let mut cursor = opts(h, "cursor", "ok");
    cursor
        .installation
        .env
        .insert("CURSOR_AUTH_TOKEN".into(), Some(SECRET.into()));
    assert_eq!(
        check("cursor", &cursor, "unknown").await["configured"],
        json!(["environment: CURSOR_AUTH_TOKEN"])
    );
    let o = opts(h, "qwen", "ok");
    check("qwen", &o, "logged_out").await;
    fs::write(h.join(".env"), "export CUSTOM_KEY='synthetic#value'\n").unwrap();
    fs::write(h.join("settings.json"),r#"{/* keep URL strings intact */ "$version":4, "modelProviders":{"openai":[{"envKey":"CUSTOM_KEY"}]},"env":{"OPENAI_BASE_URL":"http://example.invalid"}}"#).unwrap();
    assert_eq!(
        check("qwen", &o, "unknown").await["configured"],
        json!(["user .env or settings.env: CUSTOM_KEY"])
    );
    fs::write(h.join("system.json"), r#"{"$version":5}"#).unwrap();
    assert!(check("qwen", &o, "unknown").await["configured"].is_null());
    let outside = kar::runtime("pi").unwrap().auth_status(&o).await;
    assert!(matches!(outside, Err(kar::Error::Unsupported(_))));
    assert!(matches!(
        serde_json::from_value::<AuthStatus>(value),
        Ok(AuthStatus::LoggedIn { .. })
    ));
}
