use super::{SECRET, opts};
use kar::account::{LoginEvent, LoginOptions};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead},
    time::Duration,
};

fn output(v: Value) {
    println!("{v}");
}

pub(super) fn peer(id: &str, case: &str, args: &[&str]) {
    assert!(matches!(
        (id, args),
        ("codex", ["app-server", "--listen", "stdio://"])
            | ("factory", ["exec", "--output-format", "acp"])
    ));
    fs::write("started", "").unwrap();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut read = || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
    let init = read();
    assert_eq!(init["method"], "initialize");
    if id == "codex" {
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
        output(json!({"id":init["id"],"result":{}}));
        assert_eq!(read()["method"], "initialized");
        let start = read();
        assert_eq!(start["method"], "account/login/start");
        assert_eq!(start["params"], json!({"type":"chatgptDeviceCode"}));
        let completed = |id: &str, yes: bool| json!({"method":"account/login/completed","params":{"loginId":id,"success":yes,"error":SECRET}});
        output(completed("other", true));
        if case == "early" {
            output(completed("mine", true));
        }
        let url = if case == "invalid" {
            "file:///private/pair"
        } else {
            "https://example.test/verify"
        };
        output(
            json!({"id":start["id"],"result":{"loginId":"mine","userCode":"SHORT-CODE","verificationUrl":url,"access_token":SECRET}}),
        );
        if case == "invalid" {
            return;
        }
        if matches!(case, "cancel" | "timeout" | "interaction") {
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        if case != "early" {
            output(completed("other", false));
            output(completed("mine", case != "reject"));
        }
        if case == "reject" {
            return;
        }
        let verify = read();
        assert_eq!(verify["method"], "account/read");
        fs::write("confirming", "").unwrap();
        if case == "confirmation_cancel" {
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        let account = if case == "no_account" {
            Value::Null
        } else {
            json!({"type":"chatgpt","email":"person@example.org","planType":"plus","token":SECRET})
        };
        output(json!({"id":verify["id"],"result":{"account":account}}));
    } else {
        assert!(init["params"]["clientCapabilities"]["elicitation"]["url"].is_object());
        output(
            json!({"jsonrpc":"2.0","id":init["id"],"result":{"protocolVersion":1,"authMethods":[{"id":"device-pairing"}]}}),
        );
        let auth = read();
        assert_eq!(auth["method"], "authenticate");
        assert_eq!(auth["params"]["methodId"], "device-pairing");
        output(
            json!({"jsonrpc":"2.0","id":"private","method":"terminal/create","params":{"secret":SECRET}}),
        );
        assert_eq!(read()["error"]["code"], -32601);
        output(
            json!({"jsonrpc":"2.0","id":"url","method":"elicitation/create","params":{"mode":"url","url":"https://factory.test/pair?code=PAIR-CODE","message":SECRET}}),
        );
        let response = read();
        assert_eq!(
            response["result"]["action"],
            if case == "interaction" {
                "decline"
            } else {
                "accept"
            }
        );
        if case == "interaction" {
            return;
        }
        output(json!({"jsonrpc":"2.0","id":auth["id"],"result":{}}));
        let verify = read();
        assert_eq!(verify["method"], "auth/status");
        fs::write("confirming", "").unwrap();
        let account = if case == "key" {
            json!({"active":true,"label":"Factory API key","token":SECRET})
        } else {
            json!({"active":true,"email":"reader@example.org"})
        };
        output(
            json!({"jsonrpc":"2.0","id":verify["id"],"result":{"state":"authenticated","accounts":[account]}}),
        );
    }
}
pub(super) async fn exercise() {
    for (id, case, kind, reason, count) in [
        ("codex", "old", "unsupported", "version_unsupported", 0),
        ("codex", "pre_cancel", "cancelled", "", 0),
        ("codex", "ok", "logged_in", "", 2),
        ("codex", "early", "logged_in", "", 0),
        ("codex", "reject", "failed", "rejected", 2),
        ("codex", "no_account", "failed", "not_logged_in", 2),
        ("codex", "invalid", "failed", "process_failed", 0),
        ("codex", "cancel", "cancelled", "", 1),
        ("codex", "timeout", "failed", "timed_out", 2),
        ("codex", "interaction", "failed", "interaction_failed", 1),
        ("codex", "confirmation_cancel", "logged_in", "", 2),
        ("factory", "ok", "logged_in", "", 1),
        ("factory", "key", "failed", "not_logged_in", 1),
        ("factory", "interaction", "failed", "interaction_failed", 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let isolated = opts(dir.path(), id, &format!("login_{case}"));
        let options = LoginOptions {
            installation: isolated.installation,
            timeout: Some(Duration::from_secs(if case == "timeout" { 1 } else { 4 })),
            ..Default::default()
        };
        let cancel = options.cancellation.clone();
        if case == "pre_cancel" {
            cancel.cancel();
        }
        if case == "confirmation_cancel" {
            let flag = dir.path().join("confirming");
            let cancel = cancel.clone();
            tokio::spawn(async move {
                while !flag.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                cancel.cancel();
            });
        }
        let mut rt = kar::runtime(id).unwrap();
        rt.program = std::env::current_exe().unwrap();
        let mut events = vec![];
        let r = rt
            .login(&options, &mut |e: LoginEvent| {
                events.push(serde_json::to_value(e).unwrap());
                if case == "cancel" {
                    cancel.cancel();
                }
                if case == "interaction" {
                    return Err(kar::Error::Refused(SECRET.into()));
                }
                Ok(())
            })
            .await
            .unwrap();
        let result = serde_json::to_value(r).unwrap();
        assert_eq!(result["kind"], kind, "{id}/{case}: {result}");
        assert_eq!(
            result["reason"].as_str().unwrap_or_default(),
            reason,
            "{id}/{case}"
        );
        assert_eq!(events.len(), count, "{id}/{case}");
        for e in &events {
            assert!(!e.to_string().contains(SECRET));
        }
        let text = result.to_string();
        for sensitive in [SECRET, "SHORT-CODE", "PAIR-CODE"] {
            assert!(!text.contains(sensitive));
        }
        if case == "confirmation_cancel" {
            assert!(result["account"].is_null());
        }
        if case == "ok" && id == "codex" {
            assert_eq!(events[0]["userCode"], "SHORT-CODE");
            assert_eq!(result["account"]["email"], "person@example.org");
            assert_eq!(result["account"]["metering"], "plan");
        }
        if case == "ok" && id == "factory" {
            assert_eq!(events[0]["url"], "https://factory.test/pair?code=PAIR-CODE");
            assert_eq!(result["account"]["email"], "reader@example.org");
        }
        if case == "pre_cancel" || case == "old" {
            assert!(!dir.path().join("started").exists());
        }
    }
}
