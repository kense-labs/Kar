use super::server;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

fn marker() -> PathBuf {
    PathBuf::from(std::env::var_os("FIXTURE_NATIVE_DB").unwrap()).join("auth-session")
}
pub(super) fn terminal() {
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    assert_eq!(std::env::var("FIXTURE_LOGIN_ENV").unwrap(), "terminal");
    println!("Enter fixture phrase:");
    std::io::stdout().flush().unwrap();
    let mut data = String::new();
    std::io::stdin().read_line(&mut data).unwrap();
    assert_eq!(data.trim(), "test phrase");
    fs::write(marker(), "terminal").unwrap();
}
pub(super) fn peer() {
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    let mut rows = std::io::stdin().lock().lines();
    let mut authenticated = false;
    while let Some(line) = rows.next() {
        let frame: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = &frame["id"];
        let p = &frame["params"];
        let result = match frame["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"auth":{"logout":{}},"sessionCapabilities":{"delete":{}}},
                "authMethods":[{"id":"web","name":"Browser login","type":"agent"},{"id":"tty","name":"TTY login","type":"terminal","args":["terminal"],"env":{"FIXTURE_LOGIN_ENV":"terminal","KAR_WORKBENCH_TOKEN":"forbidden"}},
                {"id":"key","name":"Credentials","type":"env_var","vars":[{"name":"FIXTURE_LOGIN_KEY"},{"name":"KAR_WORKBENCH_TOKEN"}]},{"id":"auto","name":"Headless"}]})
            }
            "authenticate" if p["methodId"] == "web" => {
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0","id":"consent","method":"elicitation/create","params":{"mode":"url","elicitationId":"test","url":"https://example.org/login","message":"Open the fixture?"}})
                );
                std::io::stdout().flush().unwrap();
                let yes: Value = serde_json::from_str(&rows.next().unwrap().unwrap()).unwrap();
                assert_eq!(yes["id"], "consent");
                if yes["result"]["action"] != "accept" {
                    println!(
                        "{}",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Declined"}})
                    );
                    std::io::stdout().flush().unwrap();
                    continue;
                }
                fs::write(marker(), "browser").unwrap();
                json!({})
            }
            "authenticate" => {
                assert_eq!(p["methodId"], "auto");
                authenticated = true;
                json!({})
            }
            "logout" => {
                if marker().is_file() {
                    fs::remove_file(marker()).unwrap();
                }
                json!({})
            }
            "session/new" | "session/load" => {
                if !marker().exists()
                    && !authenticated
                    && std::env::var("FIXTURE_LOGIN_KEY").as_deref() != Ok("private-test-key")
                {
                    println!(
                        "{}",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Authentication required"}})
                    );
                    std::io::stdout().flush().unwrap();
                    continue;
                }
                json!({"sessionId":"auth-id"})
            }
            "session/delete" => json!({}),
            "session/prompt" => {
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"auth-id","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"signed-in peer"}}}})
                );
                json!({"stopReason":"end_turn"})
            }
            other => panic!("unexpected auth peer call {other}"),
        };
        println!("{}", json!({"jsonrpc":"2.0","id":id,"result":result}));
        std::io::stdout().flush().unwrap();
    }
}
async fn wait(client: &Client, flow: &str, expected: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = server::json(client.get(flow)).await;
            if state["status"] == expected {
                return state;
            }
            assert!(
                state["status"] != "failed" && state["status"] != "complete",
                "{state}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}
async fn initiate(client: &Client, auth: &str, method: &str, vars: Value) -> String {
    let flow = server::json(
        client
            .post(format!("{auth}/login"))
            .json(&json!({"method_id":method,"credentials":vars})),
    )
    .await;
    format!("{auth}/flows/{}", flow["flow"].as_str().unwrap())
}
async fn logout(client: &Client, auth: &str) {
    let f = server::json(client.post(format!("{auth}/logout"))).await;
    wait(
        client,
        &format!("{auth}/flows/{}", f["flow"].as_str().unwrap()),
        "complete",
    )
    .await;
}
pub(super) async fn exercise(client: &Client, base: &str, p: &str, exe: &Path, token: &str) {
    let mut secret_headers=reqwest::header::HeaderMap::new();
    secret_headers.insert(reqwest::header::AUTHORIZATION,"Bearer kar-workbench-fixture".parse().unwrap());
    secret_headers.insert("kar-auth-client",uuid::Uuid::new_v4().simple().to_string().parse().unwrap());
    let scoped_client=Client::builder().default_headers(secret_headers).build().unwrap();
    let account=format!("{base}/projects/{p}/accounts/codex");
    let methods=server::json(client.get(format!("{account}/methods"))).await;
    assert_eq!(methods,json!({"runtime":"codex","login":true,"logout":true,"quota":true}));
    assert_eq!(client.post(format!("{account}/login")).send().await.unwrap().status(),StatusCode::BAD_REQUEST);
    let unsupported=server::json(client.get(format!("{base}/projects/{p}/accounts/fx/quota"))).await;
    assert_eq!(unsupported["kind"],"unsupported");
    let other=client;
    let client=&scoped_client;
    let input = json!({"source":"local","name":"Authentication fixture","command_path":exe,"command_args":["auth-fixture"],"auth_method_id":"auto"});
    let profile = server::json(
        client
            .put(format!("{base}/acp-registry/profiles/sign-in"))
            .json(&input),
    )
    .await;
    let rt = profile["runtime"].as_str().unwrap();
    let auth = format!("{base}/projects/{p}/acp-registry/{rt}/auth");
    let methods = server::json(client.get(format!("{auth}/methods"))).await;
    assert_eq!(methods["methods"].as_array().unwrap().len(), 4);
    assert_eq!(methods["default_method_id"], "auto");
    assert_eq!(methods["logout"], true);
    assert_eq!(
        methods["methods"][2]["environment"],
        json!(["FIXTURE_LOGIN_KEY"])
    );
    assert_eq!(
        Client::new()
            .post(format!("{auth}/login"))
            .bearer_auth(token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for accept in [false, true] {
        let f = initiate(client, &auth, "web", json!({})).await;
        assert_eq!(other.get(&f).send().await.unwrap().status(),StatusCode::NOT_FOUND);
        assert_eq!(other.delete(&f).send().await.unwrap().status(),StatusCode::NOT_FOUND);
        let state = wait(client, &f, "browser").await;
        assert_eq!(state["url"], "https://example.org/login");
        assert_eq!(
            Client::new()
                .get(&f)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        server::json(
            client.post(&f).json(
                &json!({"type":"browser","interaction":state["interaction"],"accept":accept}),
            ),
        )
        .await;
        wait(client, &f, if accept { "complete" } else { "failed" }).await;
    }
    logout(client, &auth).await;
    let f = initiate(client, &auth, "tty", json!({})).await;
    let state = wait(client, &f, "terminal").await;
    server::json(client.post(&f).json(&json!({"type":"terminal","interaction":state["interaction"],"data_base64":STANDARD.encode(b"test phrase\n"),"size":[80,24]}))).await;
    wait(client, &f, "complete").await;
    logout(client, &auth).await;
    let wrong = initiate(client, &auth, "key", json!({"KAR_WORKBENCH_TOKEN":"no"})).await;
    wait(client, &wrong, "failed").await;
    let f = initiate(
        client,
        &auth,
        "key",
        json!({"FIXTURE_LOGIN_KEY":"private-test-key"}),
    )
    .await;
    wait(client, &f, "complete").await;
    let f = initiate(client, &auth, "key", json!({})).await;
    wait(client, &f, "complete").await;
    assert_eq!(
        server::json(client.delete(format!("{auth}/credentials"))).await["removed"],
        true
    );
    let f = initiate(client, &auth, "key", json!({})).await;
    wait(client, &f, "failed").await;
    let chat = Uuid::new_v4();
    server::json(
        client
            .post(format!("{base}/projects/{p}/chats"))
            .json(&json!({"id":chat,"runtime":rt})),
    )
    .await;
    let answer = server::json(
        client
            .post(format!("{base}/projects/{p}/chats/{chat}/messages"))
            .json(&json!({"id":Uuid::new_v4(),"text":"headless"})),
    )
    .await;
    assert!(answer.to_string().contains("signed-in peer"));
}
