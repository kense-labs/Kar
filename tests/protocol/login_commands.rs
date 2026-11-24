use super::{SECRET, opts};
use kar::account::{KimiRegion, LoginEvent, LoginOptions};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, BufRead},
    path::Path,
    time::Duration,
};

pub(super) fn peer(id: &str, case: &str, args: &[&str]) {
    let saved = Path::new("paired");
    match (id, args) {
        ("cursor", ["login"]) => {
            assert_eq!(env::var("NO_OPEN_BROWSER").unwrap(), "1");
            // The native CLI prints a standalone URL after a marker when it opened a browser.
            println!("https://unrelated.test/{SECRET}");
            println!("Matching code: {SECRET}");
            println!("If your browser didn't open, use this link:");
            println!("https://example.test/loginDeepControl?uuid=chat");
            println!(
                "Open a browser and navigate to this link: https://example.test/loginDeepControl?uuid=chat"
            );
            println!("Matching code: 123 456");
            println!("Matching code: 123 456");
            if case == "reject" {
                eprintln!("Login error: {SECRET}");
                return;
            }
            println!("\u{2713} Login successful!");
        }
        ("cursor", ["status", "--format", "json"]) => {
            return println!(
                "{}",
                if saved.exists() {
                    json!({"status":"authenticated","isAuthenticated":true,"userInfo":{"email":"mock@example.test"}})
                } else {
                    json!({"status":"unauthenticated","isAuthenticated":false})
                }
            );
        }
        ("qoder", ["login"]) => {
            assert_eq!(env::var("BROWSER").unwrap(), "www-browser");
            println!("stdout noise {SECRET}");
            println!("https://example.test/qoder");
            println!("https://example.test/qoder");
            if case == "reject" {
                println!("Login failed: {SECRET}");
                return;
            }
            println!("Login successful!");
        }
        ("grok", ["login", "--device-auth"]) => {
            assert!(Path::new(&env::var_os("BROWSER").unwrap()).is_file());
            eprintln!("{SECRET}");
            println!("https://accounts.x.ai/oauth2/device?user_code=CODE");
            println!("https://accounts.x.ai/oauth2/device?user_code=CODE");
            if case == "reject" {
                std::process::exit(1);
            }
        }
        ("kimi", ["login", "--region", "global"]) => {
            let dir = env::split_paths(&env::var_os("PATH").unwrap())
                .next()
                .unwrap();
            let opener = if cfg!(windows) {
                "powershell.exe"
            } else if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            assert!(dir.join(opener).is_file());
            fs::write("helper", dir.to_str().unwrap()).unwrap();
            println!("Logged in to fake.stdout.");
            eprintln!("Opening browser for Kimi device login: https://example.test/kimi");
            for _ in 0..2 {
                eprintln!("If the browser did not open, paste the URL above and enter code: CODE");
            }
            if case == "reject" {
                eprintln!("Login failed: {SECRET}");
                return;
            }
            eprintln!("Logged in to Kimi Code.");
        }
        ("qoder", ["status", "--output", "json"]) => {
            return println!("{}", json!({"logged_in":saved.exists()}));
        }
        ("grok", ["models"]) => {
            assert!(saved.exists());
            return println!("You are logged in with grok.com.");
        }
        ("kimi", ["acp"]) => {
            let stdin = io::stdin();
            let mut lines = stdin.lock().lines();
            let mut read =
                || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
            let init = read();
            println!(
                "{}",
                json!({"jsonrpc":"2.0","id":init["id"],"result":{"protocolVersion":1,"authMethods":[{"id":"login"}]}})
            );
            let auth = read();
            assert_eq!(auth["params"]["methodId"], "login");
            assert!(saved.exists());
            return println!("{}", json!({"jsonrpc":"2.0","id":auth["id"],"result":{}}));
        }
        (
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
            let reply = |req: &Value, answer: Value| {
                println!(
                    "{}",
                    json!({"type":"control_response","response":{"subtype":"success","request_id":req["request_id"],"response":answer}})
                )
            };
            let callback =
                |request| println!("{}", json!({"type":"control_request","request":request}));
            let init = read();
            assert_eq!(init["request"]["subtype"], "initialize");
            if Path::new("started").exists() {
                return reply(
                    &init,
                    if saved.exists() {
                        json!({"account":{"private":SECRET}})
                    } else {
                        json!({"account":null})
                    },
                );
            }
            assert!(Path::new(&env::var_os("BROWSER").unwrap()).is_file());
            fs::write("started", "").unwrap();
            reply(&init, json!({"account":null}));
            let auth = read();
            assert_eq!(auth["request"], json!({"subtype":"authenticate"}));
            callback(
                json!({"subtype":"auth_url_callback","authState":{"authUrl":"https://example.test/codebuddy","private":SECRET}}),
            );
            if case == "reject" {
                callback(
                    json!({"subtype":"auth_result_callback","success":false,"error":{"type":"cancelled","message":SECRET}}),
                );
                return;
            }
            callback(json!({"subtype":"auth_result_callback","success":true}));
            std::thread::sleep(Duration::from_millis(80));
            if case != "out" {
                fs::write(saved, "").unwrap();
            }
            if case == "early_exit" {
                return;
            }
            return reply(&auth, json!({"success":true}));
        }
        _ => panic!("unexpected {id} login peer invocation {args:?}"),
    }
    if case != "out" {
        fs::write(saved, "").unwrap();
    }
}

pub(super) async fn exercise() {
    for (id, case, kind, reason) in [
        ("qoder", "ok", "logged_in", ""),
        ("cursor", "ok", "logged_in", ""),
        ("cursor", "reject", "failed", "rejected"),
        ("cursor", "out", "failed", "not_logged_in"),
        ("cursor", "key", "logged_in", ""),
        ("qoder", "reject", "failed", "rejected"),
        ("qoder", "out", "failed", "not_logged_in"),
        ("kimi", "ok", "logged_in", ""),
        ("kimi", "reject", "failed", "rejected"),
        ("grok", "ok", "logged_in", ""),
        ("grok", "reject", "failed", "rejected"),
        ("codebuddy", "ok", "logged_in", ""),
        ("codebuddy", "early_exit", "logged_in", ""),
        ("codebuddy", "out", "failed", "not_logged_in"),
        ("codebuddy", "reject", "cancelled", ""),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = kar::runtime(id).unwrap();
        rt.program = env::current_exe().unwrap();
        let mut installation = opts(dir.path(), id, &format!("login_{case}")).installation;
        if case == "key" {
            installation
                .env
                .insert("CURSOR_API_KEY".into(), Some(SECRET.into()));
        }
        let options = LoginOptions {
            installation,
            timeout: Some(Duration::from_secs(6)),
            kimi_region: (id == "kimi").then_some(KimiRegion::Global),
            open_browser: Some(false),
            ..Default::default()
        };
        let mut events = vec![];
        let result = rt
            .login(&options, &mut |event: LoginEvent| {
                events.push(serde_json::to_value(event).unwrap());
                Ok(())
            })
            .await
            .unwrap();
        let result = serde_json::to_value(result).unwrap();
        if cfg!(windows) && id == "grok" {
            assert_eq!(result["reason"], "browser_suppression_unavailable");
            assert!(events.is_empty());
            continue;
        }
        assert_eq!(result["kind"], kind, "{id}/{case} {result}");
        assert_eq!(
            result["reason"].as_str().unwrap_or_default(),
            reason,
            "{id}/{case} {result}"
        );
        assert!(!result.to_string().contains(SECRET));
        if id == "cursor" {
            assert_eq!(events.len(), if case == "key" { 3 } else { 2 });
            if case == "key" {
                assert!(
                    events[1]["message"]
                        .as_str()
                        .unwrap()
                        .contains("CURSOR_API_KEY")
                );
            }
            assert_eq!(
                events[0]["url"],
                "https://example.test/loginDeepControl?uuid=chat"
            );
            assert!(
                events.last().unwrap()["message"]
                    .as_str()
                    .unwrap()
                    .ends_with("123 456")
            );
        } else {
            assert_eq!(events.len(), 1, "{id}/{case} {result}");
        }
        assert!(events.iter().all(|e| !e.to_string().contains(SECRET)));
        if matches!(id, "grok" | "kimi") {
            assert_eq!(events[0]["userCode"], "CODE");
        }
        if id == "kimi" {
            assert!(!Path::new(&fs::read_to_string(dir.path().join("helper")).unwrap()).exists());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let iso = opts(dir.path(), "qoder", "login_ok");
    let mut cli = std::process::Command::new(env!("CARGO_BIN_EXE_kar"));
    for (k, v) in iso.installation.env {
        if let Some(v) = v {
            cli.env(k, v);
        } else {
            cli.env_remove(k);
        }
    }
    let out = cli
        .current_dir(dir.path())
        .env("KAR_QODER_BIN", env::current_exe().unwrap())
        .args(["account", "qoder", "login", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events: Vec<Value> = out
        .stdout
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "auth_url");
    assert_eq!(events[1]["kind"], "logged_in");
}
