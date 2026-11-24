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
        for name in ["codex", "qwen", "claude"] {
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
        if !self.live {
            cmd.env("CODEX_HOME", self.dir.join("primary-account"));
            let config = self.dir.join("instances.toml");
            if config.exists() {
                cmd.env("KAR_AGENT_SESSION_CONFIG", config);
            }
        }
        let providers: &[&str] = if self.live {
            &["codex", "qwen"]
        } else {
            &["codex", "qwen", "claude"]
        };
        for &name in providers {
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
async fn fork_prior_codex_turn(
    f: &Fixture,
    selected: &Value,
    expected_native: &Value,
    child_prompt: &str,
) -> Value {
    let before = f.history().await;
    assert_eq!(before["chat"]["runtime"], "qwen");
    let child = Uuid::new_v4();
    let request = json!({"id":child,"turn":selected["id"]});
    let result = body(f.post("forks", &request)).await;
    let fork = &result["history"];
    assert_eq!(fork["chat"]["runtime"], "codex");
    let old_route = &before["handoffs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["from"]["native_id"] == *expected_native)
        .unwrap()["from"];
    assert_eq!(fork["chat"]["model"], old_route["model"]);
    assert_eq!(fork["chat"]["settings"], old_route["settings"]);
    assert_eq!(
        fork["chat"]["forked_from"]["native_source"],
        *expected_native
    );
    assert_eq!(
        fork["chat"]["forked_from"]["native_workspace"],
        json!(f.workspace)
    );
    assert_eq!(fork["chat"]["id"], child.to_string());
    assert_ne!(fork["chat"]["native_id"], *expected_native);
    assert_eq!(
        fork["turns"].as_array().unwrap().last().unwrap()["id"],
        selected["id"]
    );
    let child_url = f
        .endpoint("messages")
        .replace(&f.chat.to_string(), &child.to_string());
    let answer = body(
        f.client
            .post(child_url)
            .json(&json!({"id":Uuid::new_v4(),"text":child_prompt})),
    )
    .await;
    assert_eq!(answer["status"], "completed");
    let replay = body(f.post("forks", &request)).await;
    assert_eq!(
        replay["history"]["chat"]["native_id"],
        fork["chat"]["native_id"]
    );
    let after = f.history().await;
    assert_eq!(before["chat"], after["chat"]);
    assert_eq!(before["turns"], after["turns"]);
    assert_eq!(before["handoffs"], after["handoffs"]);
    answer
}

async fn protocol() {
    let mut f = Fixture::new(false).await;
    let original = f.history().await["chat"]["native_id"].clone();
    let first_codex = f.send(Uuid::new_v4(), "fact-one and fact-two").await;
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
    fork_prior_codex_turn(&f, &first_codex, &original, "continue old Codex fork").await;
    let back = f.handoff(&request("codex")).await;
    assert_eq!(back["target"]["native_id"], original);
    assert_eq!(back["resumed_target"], true);
    assert_eq!(
        (back["from_turn"].as_u64(), back["through_turn"].as_u64()),
        (Some(3), Some(4))
    );
    let unanchored = f
        .post(
            "forks",
            &json!({"id":Uuid::new_v4(),"turn":first,"policy":"native"}),
        )
        .send()
        .await
        .unwrap();
    assert!(!unanchored.status().is_success());
    assert!(unanchored.text().await.unwrap().contains("qwen"));
    assert_eq!(f.history().await["chat"]["native_id"], original);
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
    workflow_stop(&mut f).await;
    f.stop().await;
}
async fn portable_fork() {
    let mut f = Fixture::new(false).await;
    f.chat = Uuid::new_v4();
    body(
        f.client
            .post(format!("{}/projects/{}/chats", f.base, f.project))
            .json(&json!({"id":f.chat,"runtime":"qwen"})),
    )
    .await;
    f.send(Uuid::new_v4(), "portable-write-first").await;
    let cut = f.send(Uuid::new_v4(), "portable-write-second").await;
    f.send(Uuid::new_v4(), "portable-write-future").await;
    let child = Uuid::new_v4();
    let request = json!({"id":child,"turn":cut["id"]});
    let fork = body(f.post("forks", &request)).await;
    let history = &fork["history"];
    assert_eq!(history["chat"]["forked_from"]["mode"], "portable_context");
    assert!(history["chat"]["forked_from"].get("anchor").is_none());
    assert_eq!(history["turns"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(f.workspace.join("portable-state")).unwrap(),
        "portable-write-second"
    );
    let strict = json!({"id":child,"turn":cut["id"],"policy":"native"});
    assert!(
        !f.post("forks", &strict)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    f.restart().await;
    assert_eq!(
        body(f.post("forks", &request)).await["history"]["chat"]["id"],
        child.to_string()
    );
    let parent = f.chat;
    f.chat = child;
    let id = Uuid::new_v4();
    let before = f.calls().len();
    let answer = f.send(id, "continue the branch").await;
    let calls = f.calls();
    assert_eq!(calls.len(), before + 1);
    let wire = calls.last().unwrap()["text"].as_str().unwrap();
    assert!(wire.contains("portable-write-first") && wire.contains("portable-write-second"));
    assert!(!wire.contains("portable-write-future") && wire.ends_with("continue the branch"));
    f.restart().await;
    assert_eq!(f.send(id, "continue the branch").await, answer);
    f.send(Uuid::new_v4(), "second child request").await;
    let calls = f.calls();
    assert_eq!(calls.len(), before + 2);
    assert_eq!(calls.last().unwrap()["text"], "second child request");
    f.chat = parent;
    assert_eq!(f.history().await["turns"].as_array().unwrap().len(), 3);
    f.stop().await;
}
async fn workflow_stop(f: &mut Fixture) {
    let chats = format!("{}/projects/{}/chats", f.base, f.project);
    f.chat = Uuid::new_v4();
    body(
        f.client
            .post(&chats)
            .json(&json!({"id":f.chat,"runtime":"claude"})),
    )
    .await;
    let stop = f.endpoint("tasks/job/stop-workflow");
    f.send(Uuid::new_v4(), "start native workflow").await;
    let first = f.history().await["tasks"][0]["started_at"].clone();
    assert!(first["stream_id"].is_string());
    let stop_job = |occ: &Value| f.client.post(&stop).json(&json!({"started_at":occ}));
    body(stop_job(&first)).await;
    assert!(!stop_job(&first).send().await.unwrap().status().is_success());
    f.send(Uuid::new_v4(), "start native workflow").await;
    let next = f.history().await["tasks"][0]["started_at"].clone();
    assert_ne!(next, first);
    assert!(!stop_job(&first).send().await.unwrap().status().is_success());
    let unknown = f
        .client
        .post(f.endpoint("tasks/missing/stop-workflow"))
        .json(&json!({"started_at":next}));
    assert!(!unknown.send().await.unwrap().status().is_success());
    body(stop_job(&next)).await;
    assert_eq!(
        f.calls().iter().filter(|v| v["stopped"] == "job").count(),
        2
    );
}
/// Two named accounts of the same runtime and model have distinct native sessions.
async fn named_instances() {
    let mut f = Fixture::new(false).await;
    let second = f.dir.join("second-account");
    fs::write(
        f.dir.join("instances.toml"),
        format!(
            "[instances.second]\nruntime = \"codex\"\nhome = {}\n",
            json!(second)
        ),
    )
    .unwrap();
    f.restart().await;
    f.send(Uuid::new_v4(), "first account fact").await;
    let first = f.history().await["chat"]["native_id"].clone();
    let switch = json!({"id":Uuid::new_v4(),"runtime":"codex","instance_id":"second"});
    let second_receipt = f.handoff(&switch).await;
    let second_id = &second_receipt["target"]["native_id"];
    assert!(second_id.is_string());
    assert_ne!(second_id, &first);
    assert_eq!(second_receipt["target"]["instance"]["id"], "second");
    f.send(Uuid::new_v4(), "second account fact").await;
    assert_eq!(f.calls().last().unwrap()["instance_home"], json!(second));
    f.restart().await;
    assert_eq!(f.handoff(&switch).await["target"]["native_id"], *second_id);
    let back = f
        .handoff(&json!({"id":Uuid::new_v4(),"runtime":"codex","instance_id":"codex"}))
        .await;
    assert_eq!(back["target"]["native_id"], first);
    f.send(Uuid::new_v4(), "continue first account").await;
    assert_eq!(
        f.calls().last().unwrap()["instance_home"],
        json!(f.dir.join("primary-account"))
    );
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
    let verify_fork = std::env::var("KAR_HANDOFF_FORK_LIVE").as_deref() == Ok("1");
    if verify_fork {
        let branch_answer = fork_prior_codex_turn(
        &f,
        &tool,
        &a,
        "Without tools repeat both color-prefixed facts and any silver-prefixed fact you remember.",
    )
    .await;
        assert!(
            assistant(&branch_answer).contains(&fact_one)
                && assistant(&branch_answer).contains(&fact_two)
        );
        assert!(!assistant(&branch_answer).contains(&only_b));
    }
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
    assert_eq!(
        f.calls().len(),
        if verify_fork { 5 } else { 4 },
        "unexpected model input count"
    );
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
                    protocol().await;
                    portable_fork().await;
                    named_instances().await
                }
            },
        )
        .await
        .expect("workbench handoff acceptance timed out");
    });
}
