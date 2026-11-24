// Independent native queries. No Kar ChatStore or adapter projection is used to determine the expected receipt.
use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::path::Path;
use tokio::io::{AsyncWriteExt, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub(super) async fn opener(
    vendor: &str,
    bin: &Path,
    opts: &SessionOptions,
    root: &str,
    key: Uuid,
) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.envs(&opts.env)
        .current_dir(&opts.cwd)
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    if vendor == "kimi" {
        kimi(cmd, opts, root, key).await
    } else {
        codex(cmd, opts, root, key).await
    }
}

async fn rpc(
    input: &mut ChildStdin,
    output: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    input
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let line = output
            .next_line()
            .await
            .unwrap()
            .expect("app-server closed before native reply");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == id && v.get("method").is_none() {
            assert!(v.get("error").is_none(), "native {method}: {}", v["error"]);
            return v["result"].clone();
        }
    }
}

fn codex_turn(thread: &Value, root: &str, key: Uuid) -> Option<String> {
    assert_eq!(thread["id"], root);
    let turns = thread["turns"].as_array().expect("missing thread turns");
    let mut items = turns
        .iter()
        .flat_map(|t| {
            t["items"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(move |(i, item)| (t, i, item))
        })
        .filter(|(_, _, i)| {
            i["type"] == "userMessage"
                && i["clientId"].as_str().and_then(|s| s.parse::<Uuid>().ok()) == Some(key)
        });
    let (t, i, _) = items.next()?;
    let id = t["id"].as_str().filter(|s| !s.is_empty())?;
    (i == 0 && items.next().is_none() && turns.iter().filter(|t| t["id"] == id).count() == 1)
        .then(|| id.to_owned())
}
async fn codex(mut cmd: Command, opts: &SessionOptions, root: &str, key: Uuid) -> Option<String> {
    let mut child = cmd.arg("app-server").stdin(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    rpc(&mut input,&mut output,1,"initialize",json!({"clientInfo":{"name":"kar-crash-inspector","version":"1.0.0"},"capabilities":{"experimentalApi":true}})).await;
    input
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\"}\n")
        .await
        .unwrap();
    let resumed = rpc(
        &mut input,
        &mut output,
        2,
        "thread/resume",
        json!({"threadId":root,"cwd":opts.cwd}),
    )
    .await;
    let mut found = codex_turn(&resumed["thread"], root, key);
    if found.is_none() {
        let read = rpc(
            &mut input,
            &mut output,
            3,
            "thread/read",
            json!({"threadId":root,"includeTurns":true}),
        )
        .await;
        found = codex_turn(&read["thread"], root, key);
    }
    drop(input);
    drop(output);
    if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        child.kill().await.unwrap();
    }
    found
}
async fn kimi(mut cmd: Command, opts: &SessionOptions, root: &str, key: Uuid) -> Option<String> {
    let mut child = cmd
        .args(["web", "--json", "--host", "127.0.0.1", "--port", "0"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = loop {
        let line = output
            .next_line()
            .await
            .unwrap()
            .expect("Kimi Web exited before ready");
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && v["origin"].is_string()
        {
            break v;
        }
    };
    let origin = ready["origin"].as_str().unwrap();
    let port = origin
        .strip_prefix("http://127.0.0.1:")
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let token = ready["token"].as_str().unwrap();
    assert_eq!(ready["ws_url"], format!("ws://127.0.0.1:{port}/api/v1/ws"));
    let mut url = reqwest::Url::parse(origin).unwrap();
    url.path_segments_mut()
        .unwrap()
        .extend(["api", "v1", "sessions", root]);
    let base = url.as_str();
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let saved: Value = http
        .get(base)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["code"], 0);
    assert_eq!(saved["data"]["id"], root);
    if let Some(cwd) = saved["data"]["cwd"].as_str() {
        assert_eq!(Path::new(cwd).canonicalize().unwrap(), opts.cwd);
    }
    let confirmed:Value=http.post(format!("{base}:resume")).bearer_auth(token).json(&json!({"agent_config":{"model":"fixture","permission_mode":"manual"},"mcp_servers":{}})).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(confirmed["code"], 0);
    assert_eq!(confirmed["data"]["id"], root);
    let mut req = ready["ws_url"]
        .as_str()
        .unwrap()
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        format!("kimi-code.bearer.{token}").parse().unwrap(),
    );
    let mut socket = tokio_tungstenite::connect_async(req).await.unwrap().0;
    let sub = Uuid::new_v4().to_string();
    let body = json!({"type":"subscribe","session_id":root,"agent_ids":["main"],"request_id":sub});
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();
    let mut accepted = false;
    let mut snapshot = None;
    while !accepted || snapshot.is_none() {
        let msg = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else {
            if let Message::Ping(data) = msg {
                socket.send(Message::Pong(data)).await.unwrap();
            }
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        if let Some(id) = v.get("session_id") {
            assert_eq!(id, root);
        }
        if v["type"] == "response" && v["request_id"] == sub {
            assert_eq!(v["code"], 0);
            accepted = true;
        } else if v["type"] == "turns" && v["agent_id"] == "main" {
            snapshot = Some(v["turns"].as_array().unwrap().clone());
        } else if v["type"] == "turns.failed" {
            panic!("KAP could not read native history");
        }
    }
    socket.close(None).await.unwrap();
    http.post(format!("{origin}/api/v1/shutdown"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        child.kill().await.unwrap();
    }
    let turns = snapshot.unwrap();
    let mut occurrences = vec![];
    for turn in &turns {
        for item in turn["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"] == "user_message")
        {
            let Some(ids) = item["prompt_ids"].as_array() else {
                continue;
            };
            for id in ids.iter().filter_map(Value::as_str) {
                if id.parse::<Uuid>().ok() == Some(key) {
                    occurrences.push((
                        turn,
                        ids.len() == 1
                            && item["placement"] == "opener"
                            && item["origin"]["kind"] == "user",
                    ));
                }
            }
        }
    }
    let [(turn, true)] = occurrences.as_slice() else {
        return None;
    };
    let id = turn["turn_id"].as_str().filter(|s| !s.is_empty())?;
    (turn["kind"] == "agent" && turns.iter().filter(|t| t["turn_id"] == id).count() == 1)
        .then(|| id.to_owned())
}
