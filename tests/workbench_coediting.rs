//! The owner save and document proposal share one writer through the normal workbench process.
use anyhow::Result;
use reqwest::{Client, RequestBuilder, StatusCode, header};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

async fn launch(state: &Path, root: &Path) -> Result<(Child, String)> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(["workbench", "--state"])
        .arg(state)
        .arg("--root")
        .arg(root)
        .env("KAR_WORKBENCH_TOKEN", "document-fixture")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let ready = BufReader::new(server.stdout.take().unwrap())
        .lines()
        .next_line()
        .await?
        .unwrap();
    let info: Value = serde_json::from_str(&ready)?;
    Ok((server, info["url"].as_str().unwrap().into()))
}
fn client() -> Client {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer document-fixture".parse().unwrap(),
    );
    Client::builder().default_headers(headers).build().unwrap()
}
async fn ok(request: RequestBuilder) -> Result<Value> {
    Ok(request.send().await?.error_for_status()?.json().await?)
}

#[tokio::test]
async fn file_save_merges_an_old_whole_file_and_recovers_its_conflict_after_restart() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("workspace");
    let state = dir.path().join("private");
    std::fs::create_dir(&root)?;
    let file = root.join("notes.md");
    std::fs::write(&file, "周一\n中间\n小李\n")?;
    let (mut server, url) = launch(&state, &root).await?;
    let owner = client();
    let assistant = client();
    let project = ok(owner
        .post(format!("{url}/projects"))
        .json(&json!({"name":"team","root":root})))
    .await?;
    let scope = format!("/projects/{}", project["id"].as_str().unwrap());
    let file_route = format!("{scope}/file?path=notes.md");
    let document = format!("{scope}/documents");
    let route = |url: &str, op: &str| format!("{url}{document}/{op}?path=notes.md");
    assert_eq!(ok(owner.get(route(&url, "state"))).await?["revision"], 0);
    let file_url = format!("{url}{file_route}");
    assert_eq!(ok(owner.get(&file_url)).await?["revision"], 0);
    assert_eq!(
        owner
            .put(&file_url)
            .json(&json!({"text":"lost update"}))
            .send()
            .await?
            .status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    let human = json!({"text":"周三\n中间\n小李\n","base_revision":0,"operation_id":"human"});
    let first = ok(owner.put(&file_url).json(&human)).await?;
    assert_eq!(first["revision"], 1);
    let staged = ok(assistant
        .post(route(&url, "proposals"))
        .body("周一\n中间\n小林\n"))
    .await?;
    let proposal = json!({"author":"assistant","operation_id":"tool","base_revision":0,"action":{"type":"save","sha256":staged["sha256"]}});
    let merged = ok(assistant.post(route(&url, "operations")).json(&proposal)).await?;
    assert_eq!(merged["revision"], 2);
    assert_eq!(std::fs::read_to_string(&file)?, "周三\n中间\n小林\n");
    assert_eq!(ok(owner.get(&file_url)).await?["revision"], 2);
    let outdated = json!({"text":"周五\n中间\n小李\n","base_revision":0,"operation_id":"late"});
    let conflict = owner.put(&file_url).json(&outdated).send().await?;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let receipt: Value = conflict.json().await?;
    assert_eq!(receipt["revision"], 2);
    assert_eq!(receipt["status"], "conflict");
    let cursor = receipt["cursor"].as_u64().unwrap();
    let initial = ok(assistant.get(route(&url, "events"))).await?;
    assert_eq!(initial.as_array().unwrap().len() as u64, cursor);
    server.kill().await?;
    let (mut restarted, url) = launch(&state, &root).await?;
    let file_url = format!("{url}{file_route}");
    assert_eq!(ok(owner.get(&file_url)).await?["revision"], 2);
    assert_eq!(ok(owner.put(&file_url).json(&human)).await?, first);
    assert_eq!(
        ok(assistant.post(route(&url, "operations")).json(&proposal)).await?,
        merged
    );
    let repeated = owner.put(&file_url).json(&outdated).send().await?;
    assert_eq!(repeated.status(), StatusCode::CONFLICT);
    assert_eq!(repeated.json::<Value>().await?, receipt);
    assert_eq!(
        ok(assistant
            .get(route(&url, "events"))
            .query(&[("after", cursor)]))
        .await?,
        json!([])
    );
    let resolve = json!({"author":"owner","operation_id":"resolve","base_revision":2,"action":{"type":"resolve","conflict":receipt["conflict_id"],"decision":{"type":"keep_current"}}});
    let done = ok(owner.post(route(&url, "operations")).json(&resolve)).await?;
    assert_eq!(done["resolves"], receipt["conflict_id"]);
    assert_eq!(std::fs::read_to_string(&file)?, "周三\n中间\n小林\n");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&state, root.join("outside"))?;
        let response = owner
            .get(format!("{url}{document}/state"))
            .query(&[("path", "outside/workbench/projects.json")])
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    restarted.kill().await?;
    Ok(())
}
