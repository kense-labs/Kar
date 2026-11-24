use super::{SECRET, opts};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead},
    path::Path,
    time::Duration,
};

pub(super) fn peer(id: &str, case: &str, args: &[&str]) {
    if args == ["finish-later"] {
        std::thread::sleep(Duration::from_millis(900));
        fs::write("escaped-child", "").unwrap();
        return;
    }
    if id == "codex" && args == ["login", "status"] {
        fs::write("verified", "").unwrap();
        match case {
            "rejected_out" | "ok" => {
                eprintln!("Not logged in");
                std::process::exit(1);
            }
            "still" | "reject" | "timeout" => eprintln!("Logged in using ChatGPT"),
            _ => {
                eprintln!("{SECRET}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args == ["logout"] || args == ["auth", "logout"] {
        fs::write("dispatched", "").unwrap();
        assert_ne!(case, "old");
        return match case {
            "exited_parent" => {
                let _child = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("finish-later")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                std::process::exit(0);
            }
            "timeout" => {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("finish-later")
                    .spawn()
                    .unwrap();
                std::thread::sleep(Duration::from_secs(30));
                let _ = child.wait();
            }
            "reject" | "rejected_out" => {
                eprintln!("{SECRET}");
                std::process::exit(1);
            }
            "fail" => std::process::exit(2),
            _ => println!("{SECRET}"),
        };
    }
    if id == "antigravity" && args == ["models"] {
        panic!("custom GEMINI_HOME must not query default account");
    }
    assert!(matches!(
        (id, args),
        ("factory", ["exec", "--output-format", "acp"])
            | ("kimi", ["acp"])
            | ("antigravity", ["acp", "--gemini-dir", "isolated-profile"])
            | (
                "codebuddy",
                [
                    "--print",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json"
                ]
            )
    ));
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut read = || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
    let first = read();
    if id == "codebuddy" {
        let sub = first["request"]["subtype"].as_str().unwrap();
        let reply = match sub {
            "logout" => {
                fs::write("dispatched", "").unwrap();
                json!({"success":true,"secret":SECRET})
            }
            "initialize" => {
                fs::write("verified", "").unwrap();
                json!({"account":null})
            }
            _ => panic!("unexpected subtype"),
        };
        println!(
            "{}",
            json!({"type":"control_response","response":{"request_id":first["request_id"],"subtype":"success","response":reply}})
        );
        return;
    }
    assert_eq!(first["method"], "initialize");
    let auth = if case == "capability" {
        json!({"status":{}})
    } else {
        json!({"logout":{},"status":{}})
    };
    println!(
        "{}",
        json!({"jsonrpc":"2.0","id":first["id"],"result":{"protocolVersion":1,"agentCapabilities":{"auth":auth},"authMethods":[{"id":"login"}]}})
    );
    if case == "capability" {
        return;
    }
    let next = read();
    if next["method"] == "authenticate" {
        assert_eq!(id, "kimi");
        fs::write("verified", "").unwrap();
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":next["id"],"error":{"code":-32000,"message":SECRET}})
        );
        return;
    }
    assert_eq!(next["method"], "logout");
    assert_eq!(next["params"], json!({}));
    fs::write("dispatched", "").unwrap();
    if case == "absent" || case == "rejected_out" {
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":next["id"],"error":{"code":if case=="absent"{-32601}else{-32001},"message":SECRET}})
        );
    } else {
        println!("{}", json!({"jsonrpc":"2.0","id":next["id"],"result":{}}));
    }
    if id == "factory" {
        let verify = read();
        assert_eq!(verify["method"], "auth/status");
        fs::write("verified", "").unwrap();
        let result = match case {
            "still" => json!({"state":"authenticated","accounts":[]}),
            "key" => {
                json!({"state":"authenticated","accounts":[{"active":true,"label":"Factory API key"}]})
            }
            _ => json!({"state":"unauthenticated"}),
        };
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":verify["id"],"result":result})
        );
    }
}
async fn run(id: &str, case: &str, dir: &Path) -> Value {
    let mut o = opts(dir, id, &format!("logout_{case}"));
    if case == "timeout" {
        o.timeout = Some(Duration::from_millis(550));
    }
    if id == "antigravity" {
        o.installation
            .env
            .insert("GEMINI_HOME".into(), Some("isolated-profile".into()));
    }
    let mut rt = kar::runtime(id).unwrap();
    rt.program = std::env::current_exe().unwrap();
    let result = rt.logout(&o).await.unwrap();
    let v = serde_json::to_value(result).unwrap();
    assert!(!v.to_string().contains(SECRET));
    v
}
pub(super) async fn exercise() {
    for (id, case, kind, reason, dispatched, verified) in [
        ("codex", "ok", "logged_out", "", true, true),
        ("codex", "reject", "failed", "rejected", true, true),
        ("codex", "rejected_out", "logged_out", "", true, true),
        ("codex", "still", "failed", "still_logged_in", true, true),
        ("codex", "unknown", "logged_out", "", true, true),
        ("codex", "fail", "failed", "process_failed", true, true),
        (
            "claude",
            "old",
            "unsupported",
            "version_unsupported",
            false,
            false,
        ),
        ("factory", "rejected_out", "logged_out", "", true, true),
        ("factory", "still", "failed", "still_logged_in", true, true),
        ("factory", "key", "failed", "still_logged_in", true, true),
        (
            "factory",
            "capability",
            "failed",
            "process_failed",
            false,
            false,
        ),
        ("kimi", "ok", "logged_out", "", true, true),
        (
            "kimi",
            "absent",
            "unsupported",
            "version_unsupported",
            true,
            false,
        ),
        (
            "kimi",
            "capability",
            "unsupported",
            "version_unsupported",
            false,
            false,
        ),
        ("antigravity", "ok", "logged_out", "", true, false),
        ("codebuddy", "ok", "logged_out", "", true, true),
        ("codex", "timeout", "failed", "timed_out", true, true),
        ("codex", "exited_parent", "logged_out", "", true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let result = run(id, case, dir.path()).await;
        assert_eq!(result["kind"], kind, "{id}/{case}: {result}");
        assert_eq!(
            result["reason"].as_str().unwrap_or_default(),
            reason,
            "{id}/{case}"
        );
        assert_eq!(
            dir.path().join("dispatched").exists(),
            dispatched,
            "{id}/{case}"
        );
        assert_eq!(
            dir.path().join("verified").exists(),
            verified,
            "{id}/{case}"
        );
        if matches!(case, "timeout" | "exited_parent") {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            assert!(!dir.path().join("escaped-child").exists());
        }
    }
}
