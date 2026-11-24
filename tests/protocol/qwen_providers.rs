use kar::{
    Error, InstallationOptions,
    providers::{ProviderAuthOptions, QwenConnect, QwenScope},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead},
    time::Duration,
};

pub(super) fn peer(args: &[String]) {
    if args == ["--version"] {
        return println!("0.26.2");
    }
    if args == ["--help"] {
        return println!("Usage: qwen");
    }
    assert_eq!(args, ["--acp"]);
    let case = std::env::var("OAR_QWEN_PROVIDER_TEST").unwrap();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut read = || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
    let init = read();
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["clientCapabilities"]["terminal"], false);
    println!(
        "{}",
        json!({"jsonrpc":"2.0","id":init["id"],"result":{"protocolVersion":1}})
    );
    let req = read();
    let id = &req["id"];
    eprintln!("private-native-diagnostic");
    println!(
        "{}",
        json!({"jsonrpc":"2.0","method":"unrelated","params":{"token":"private-native-diagnostic"}})
    );
    println!(
        "{}",
        json!({"jsonrpc":"2.0","id":"other","error":{"code":-32603,"message":"private-native-diagnostic"}})
    );
    match req["method"].as_str().unwrap() {
        "qwen/providers/connect" => {
            let p = &req["params"];
            assert!(p.get("baseUrl").is_none());
            assert_eq!(p["scope"], "workspace");
            fs::write("dispatched", "1").unwrap();
            match case.as_str() {
                "refusal" => println!(
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"data":{"token":"private-native-diagnostic"},"message":"private-native-diagnostic"}})
                ),
                "lost" => {}
                "wait" => std::thread::sleep(Duration::from_secs(10)),
                _ => println!(
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"result":{"success":true,"providerId":if case=="wrong"{"other"}else{"native"},"providerLabel":"xy","authType":"openai"}})
                ),
            }
        }
        "qwen/providers/list" => {
            let config = if case == "invalid" {
                Value::Null
            } else {
                json!({"hasApiKey":true,"xy-private":"prefix xy","apiKey":"private-native-diagnostic"})
            };
            println!(
                "{}",
                json!({"jsonrpc":"2.0","id":id,"result":{"providers":[{"id":"native","protocol":"openai","label":"xy","existingConfig":config,"models":[{"id":"xy"}]}]}})
            );
        }
        _ => panic!("unexpected method"),
    }
}
pub(super) async fn exercise() {
    let bin = std::env::current_exe().unwrap();
    for case in ["ok", "invalid", "wrong", "refusal", "lost", "wait"] {
        let temp = tempfile::tempdir().unwrap();
        let h = temp.path();
        let mut env: std::collections::BTreeMap<_, _> =
            std::env::vars_os().map(|(k, _)| (k, None)).collect();
        for k in ["HOME", "USERPROFILE", "QWEN_HOME"] {
            env.insert(k.into(), Some(h.as_os_str().into()));
        }
        env.insert("OAR_QWEN_BIN".into(), Some(bin.as_os_str().into()));
        env.insert("OAR_QWEN_PROVIDER_TEST".into(), Some(case.into()));
        let auth = kar::runtime("qwen")
            .unwrap()
            .qwen_provider_auth(ProviderAuthOptions {
                installation: InstallationOptions {
                    env,
                    cwd: Some(h.into()),
                },
                timeout: if case == "wait" {
                    Duration::from_millis(700)
                } else {
                    Duration::from_secs(5)
                },
                ..Default::default()
            })
            .await
            .unwrap();
        if case == "invalid" {
            assert!(matches!(
                auth.list_providers().await,
                Err(Error::Protocol(_))
            ));
            continue;
        }
        let config = QwenConnect {
            provider_id: "native".into(),
            api_key: Some(" xy ".into()),
            scope: Some(QwenScope::Workspace),
            ..Default::default()
        };
        let response = auth.connect(&config).await;
        assert!(h.join("dispatched").exists());
        if case == "ok" {
            let r = response.unwrap();
            assert_eq!(r.provider_label, "[redacted]");
            let list = serde_json::to_string(&auth.list_providers().await.unwrap()).unwrap();
            assert!(list.contains("hasApiKey"));
            assert!(!list.contains("xy"));
            assert!(!list.contains("private-native-diagnostic"));
        } else {
            let e = response.unwrap_err();
            assert!(!e.to_string().contains("private-native-diagnostic"));
            if case == "refusal" {
                assert!(matches!(e, Error::ProviderRpc { code: -32602, .. }));
            }
        }
    }
}
