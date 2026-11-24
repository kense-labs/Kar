//! Run the HTTP contract with protocol fixtures by default, or installed
//! Codex/Qwen executables with KAR_HANDOFF_LIVE=1. See backend-workbench-handoff.md.
#[path = "support/handoff_runtime.rs"]
mod runtime;

use reqwest::{Client, RequestBuilder, StatusCode, header};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use uuid::Uuid;

struct Fixture {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    workspace: PathBuf,
    client: Client,
    live: bool,
    base: String,
    server: Option<Child>,
    project: String,
    chat: Uuid,
}
async fn body(req: RequestBuilder) -> Value {
    let response = req.send().await.unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    assert!(
        status.is_success(),
        "HTTP {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}
impl Fixture {
    async fn new(live: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let workspace = dir.join("work/one");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir(dir.join("bin")).unwrap();
        for name in ["codex", "qwen"] {
            let dest = dir
                .join("bin")
                .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            let executable = std::env::current_exe().unwrap();
            fs::hard_link(&executable, &dest)
                .or_else(|_| fs::copy(&executable, dest).map(|_| ()))
                .unwrap();
        }
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer isolated-handoff-test".parse().unwrap(),
        );
        let mut f = Self {
            _temp: temp,
            dir,
            workspace,
            client: Client::builder().default_headers(headers).build().unwrap(),
            live,
            base: String::new(),
            server: None,
            project: String::new(),
            chat: Uuid::new_v4(),
        };
        f.start().await;
        let p = body(
            f.client
                .post(format!("{}/projects", f.base))
                .json(&json!({"name":"handoff","root":f.workspace})),
        )
        .await;
        f.project = p["id"].as_str().unwrap().into();
        f.create_chat().await;
        f
    }
    async fn start(&mut self) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
        cmd.args(["workbench", "--state"])
            .arg(self.dir.join("state"))
            .arg("--root")
            .arg(self.dir.join("work"))
            .env("KAR_HANDOFF_FIXTURE", &self.dir)
            .env("KAR_WORKBENCH_TOKEN", "isolated-handoff-test")
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        for name in ["codex", "qwen"] {
            let key = format!("KAR_{}_BIN", name.to_ascii_uppercase());
            cmd.env_remove(format!("KAR_HANDOFF_REAL_{}", name.to_ascii_uppercase()));
            if self.live {
                let real = PathBuf::from(
                    std::env::var_os(&key)
                        .unwrap_or_else(|| panic!("set {key} to the real executable")),
                );
                assert!(
                    real.is_absolute() && real.is_file(),
                    "{key} must be an absolute executable path"
                );
                cmd.env(
                    format!("KAR_HANDOFF_REAL_{}", name.to_ascii_uppercase()),
                    real,
                );
            }
            cmd.env(
                key,
                self.dir
                    .join("bin")
                    .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
            );
        }
        let mut server = cmd.spawn().unwrap();
        let ready = BufReader::new(server.stdout.take().unwrap())
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        self.base = serde_json::from_str::<Value>(&ready).unwrap()["url"]
            .as_str()
            .unwrap()
            .into();
        self.server = Some(server);
    }
    async fn stop(&mut self) {
        self.server.take().unwrap().kill().await.unwrap();
    }
    async fn restart(&mut self) {
        self.stop().await;
        self.start().await;
    }
    async fn create_chat(&self) -> Value {
        body(
            self.client
                .post(format!("{}/projects/{}/chats", self.base, self.project))
                .json(&json!({"id":self.chat,"runtime":"codex"})),
        )
        .await
    }
    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/projects/{}/chats/{}/{}",
            self.base, self.project, self.chat, path
        )
    }
    fn post(&self, path: &str, payload: &Value) -> RequestBuilder {
        self.client.post(self.endpoint(path)).json(payload)
    }
    async fn send(&self, id: Uuid, text: &str) -> Value {
        let r = body(self.post("messages", &json!({"id":id,"text":text}))).await;
        assert_eq!(r["status"], "completed", "turn failed: {}", r["failure"]);
        r
    }
    async fn handoff(&self, req: &Value) -> Value {
        body(self.post("handoffs", req)).await
    }
    async fn history(&self) -> Value {
        body(self.client.get(self.endpoint("snapshot"))).await["history"].take()
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.join("inputs"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    async fn entered(&self, name: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !self.dir.join(format!("entered-{name}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
fn request(runtime: &str) -> Value {
    json!({"id":Uuid::new_v4(),"runtime":runtime})
}
fn assistant(t: &Value) -> String {
    t["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["role"] == "assistant")
        .filter_map(|i| i["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
async fn protocol() {
    let mut f = Fixture::new(false).await;
    let original = f.history().await["chat"]["native_id"].clone();
    f.send(Uuid::new_v4(), "fact-one and fact-two").await;
    f.send(Uuid::new_v4(), "collect evidence").await;
    let marker = fs::read_to_string(f.dir.join("side-effect")).unwrap();
    let req = request("qwen");
    fs::write(f.dir.join("hold-open"), []).unwrap();
    let pending = tokio::spawn(f.post("handoffs", &req).send());
    f.entered("open").await;
    let prepared = body(f.client.get(f.endpoint("handoffs"))).await;
    assert_eq!(prepared[0]["status"], "prepared");
    assert_eq!(f.history().await["chat"]["native_id"], original);
    f.stop().await;
    fs::remove_file(f.dir.join("hold-open")).unwrap();
    let _ = pending.await;
    f.start().await;
    let ready = f.handoff(&req).await;
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["from"]["native_id"], original);
    assert_eq!(ready["from_turn"], 1);
    assert_eq!(ready["through_turn"], 2);
    let b = ready["target"]["native_id"].clone();
    assert_ne!(b, original);
    f.restart().await;
    assert_eq!(ready, f.handoff(&req).await);
    assert_eq!(f.create_chat().await["info"]["native_id"], b);
    let first = Uuid::new_v4();
    let prompt = "continue using the two facts and the tool result";
    let answered = f.send(first, prompt).await;
    assert!(
        assistant(&answered).contains("received both facts and proof"),
        "{answered}"
    );
    assert_eq!(answered, f.send(first, prompt).await);
    let again = f.send(Uuid::new_v4(), "B-only next fact").await;
    assert_eq!(again["items"][0]["source"]["session_id"], b);
    let calls = f.calls();
    let inputs: Vec<_> = calls.iter().filter(|v| v["runtime"] == "qwen").collect();
    assert_eq!(inputs.len(), 2);
    let context = inputs[0]["text"].as_str().unwrap();
    assert!(
        context.contains(&marker) && context.contains("fact-one") && context.contains("fact-two")
    );
    assert!(context.ends_with(prompt));
    assert_eq!(inputs[1]["text"], "B-only next fact");
    assert_eq!(inputs[0]["cwd"], f.workspace.to_str().unwrap());
    let back = f.handoff(&request("codex")).await;
    assert_eq!(back["target"]["native_id"], original);
    assert_eq!(back["resumed_target"], true);
    assert_eq!(
        (back["from_turn"].as_u64(), back["through_turn"].as_u64()),
        (Some(3), Some(4))
    );
    let final_id = Uuid::new_v4();
    f.send(final_id, "continue on source").await;
    let final_input = f.calls().into_iter().last().unwrap();
    assert!(
        final_input["text"]
            .as_str()
            .unwrap()
            .contains("B-only next fact")
    );
    assert!(!final_input["text"].as_str().unwrap().contains(&marker));
    assert_eq!(
        fs::read_to_string(f.dir.join("side-effect")).unwrap(),
        marker
    );
    let history = f.history().await;
    assert_eq!(history["chat"]["id"], f.chat.to_string());
    assert_eq!(history["chat"]["workspace"], f.workspace.to_str().unwrap());
    assert_eq!(history["turns"][2]["items"][0]["text"], prompt);
    assert_eq!(history["turns"][2]["id"], first.to_string());
    assert_eq!(history["turns"][4]["id"], final_id.to_string());
    assert_eq!(history["handoffs"][0]["input"], first.to_string());
    assert!(
        history["turns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| !t.to_string().contains("Kar conversation history."))
    );
    fs::write(f.dir.join("hold-turn"), []).unwrap();
    let active = tokio::spawn(
        f.post("messages", &json!({"id":Uuid::new_v4(),"text":"hold turn"}))
            .send(),
    );
    f.entered("turn").await;
    assert_eq!(
        f.post("handoffs", &request("qwen"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    fs::remove_file(f.dir.join("hold-turn")).unwrap();
    assert!(active.await.unwrap().unwrap().status().is_success());
    fs::write(f.dir.join("refuse-open"), []).unwrap();
    let refused = request("qwen");
    let failure = f.handoff(&refused).await;
    assert_eq!(failure["status"], "failed");
    assert!(
        failure["error"]
            .as_str()
            .unwrap()
            .contains("fixture rejected")
    );
    fs::remove_file(f.dir.join("refuse-open")).unwrap();
    assert_eq!(failure, f.handoff(&refused).await);
    assert_eq!(f.history().await["chat"]["native_id"], original);
    f.send(Uuid::new_v4(), &"long history ".repeat(2000)).await;
    let mut small = request("qwen");
    small["budget_bytes"] = 512.into();
    let bounded = f.handoff(&small).await;
    assert!(bounded["context"].as_str().unwrap().len() <= 512);
    assert!(bounded["omitted_items"].as_u64().unwrap() > 0);
    f.stop().await;
}
async fn live() {
    let mut f = Fixture::new(true).await;
    let secret = Uuid::new_v4().to_string();
    let fact_one = format!("amber-{}", Uuid::new_v4());
    let fact_two = format!("teal-{}", Uuid::new_v4());
    let only_b = format!("silver-{}", Uuid::new_v4());
    fs::write(f.workspace.join("proof.txt"), &secret).unwrap();
    f.send(
        Uuid::new_v4(),
        &format!("Remember {fact_one} and {fact_two}. Reply OK."),
    )
    .await;
    let tool=f.send(Uuid::new_v4(),"Use a read-only tool now to read the workspace file proof.txt. Reply with its full contents.").await;
    assert!(
        assistant(&tool).contains(&secret),
        "A did not read the proof"
    );
    assert!(
        tool["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "tool" && i.to_string().contains(&secret)),
        "required tool-result evidence absent"
    );
    let out = f.handoff(&request("qwen")).await;
    assert_eq!(out["status"], "ready", "{}", out["error"]);
    let a = out["from"]["native_id"].clone();
    let prompt = format!(
        "Without using tools, repeat both color-prefixed facts and the exact proof value. Remember {only_b}."
    );
    let b = f.send(Uuid::new_v4(), &prompt).await;
    let answer = assistant(&b);
    let native = f
        .calls()
        .into_iter()
        .find(|r| r["runtime"] == "qwen")
        .unwrap();
    for fact in [&secret, &fact_one, &fact_two] {
        assert!(
            native["text"].as_str().unwrap().contains(fact),
            "target input lacked a source fact"
        );
        assert!(answer.contains(fact), "target reply lacked a source fact");
    }
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] != "tool")
    );
    f.restart().await;
    let back = f.handoff(&request("codex")).await;
    assert_eq!(back["target"]["native_id"], a, "{}", back["error"]);
    assert_eq!(back["from_turn"], 3);
    let res = f
        .send(
            Uuid::new_v4(),
            "Without tools, repeat the silver-prefixed fact.",
        )
        .await;
    assert!(assistant(&res).contains(&only_b));
    assert!(
        res["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] != "tool")
    );
    assert_eq!(
        f.history().await["chat"]["workspace"],
        f.workspace.to_str().unwrap()
    );
    assert_eq!(f.calls().len(), 4, "unexpected model input count");
    println!(
        "Installed Codex / Qwen round trip verified through the workbench HTTP process. Four admitted wire inputs, two A facts, a native tool result, one B fact and the original A ID matched."
    );
    f.stop().await;
}
fn main() {
    if runtime::child() {
        return;
    }
    let live_mode = std::env::var("KAR_HANDOFF_LIVE").as_deref() == Ok("1");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(if live_mode { 600 } else { 120 }),
            async {
                if live_mode {
                    live().await
                } else {
                    protocol().await
                }
            },
        )
        .await
        .expect("workbench handoff acceptance timed out");
    });
}
