use reqwest::{Client, header};
use serde_json::Value;
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

pub(crate) async fn serve(state: &Path, projects: &Path, native: &Path) -> (Child, String) {
    serve_with_env(state, projects, native, &[]).await
}

pub(crate) async fn serve_with_env(
    state: &Path,
    projects: &Path,
    native: &Path,
    vars: &[(&str, &std::ffi::OsStr)],
) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["workbench", "--state"])
        .arg(state)
        .arg("--root")
        .arg(projects)
        .env("KAR_CODEX_BIN", std::env::current_exe().unwrap())
        .env("KAR_WORKBENCH_TOKEN", "kar-workbench-fixture")
        .env("FIXTURE_NATIVE_DB", native)
        .envs(vars.iter().copied())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let line = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    (child, ready["url"].as_str().unwrap().to_owned())
}

pub(crate) fn client() -> Client {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer kar-workbench-fixture".parse().unwrap(),
    );
    Client::builder().default_headers(headers).build().unwrap()
}

pub(crate) async fn json(req: reqwest::RequestBuilder) -> Value {
    let r = req.send().await.unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "HTTP {status}: {text}");
    serde_json::from_str(&text).unwrap()
}
