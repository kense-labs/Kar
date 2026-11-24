//! macOS acceptance via the normal workbench HTTP process and real PTYs.
#![cfg(target_os = "macos")]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use uuid::Uuid;

const TOKEN: &str = "kar-workbench-terminal-test";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Server {
    child: Child,
    base: String,
}
impl Server {
    async fn start(state: &Path, roots: &Path) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kar"))
            .args(["workbench", "--state"])
            .arg(state)
            .arg("--root")
            .arg(roots)
            .env("KAR_WORKBENCH_TOKEN", TOKEN)
            .env("SHELL", "/bin/sh")
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .spawn()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await?;
        let ready: Value = serde_json::from_str(&line)?;
        Ok(Self {
            child,
            base: ready["url"].as_str().unwrap().trim_end_matches('/').into(),
        })
    }
    async fn kill(mut self) -> Result<()> {
        self.child.kill().await?;
        self.child.wait().await?;
        Ok(())
    }
    async fn register(&self, c: &Client, path: &Path) -> Result<String> {
        let p = api(c
            .post(format!("{}/projects", self.base))
            .json(&json!({"root":path,"name":path.file_name().unwrap().to_str().unwrap()})))
        .await?;
        Ok(format!(
            "{}/projects/{}",
            self.base,
            p["id"].as_str().unwrap()
        ))
    }
}
async fn api(req: RequestBuilder) -> Result<Value> {
    Ok(req
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
async fn terminal(c: &Client, scope: &str) -> Result<String> {
    let id = Uuid::new_v4();
    api(c.post(format!("{scope}/terminals")).json(&json!({"id":id}))).await?;
    Ok(format!("{scope}/terminals/{id}"))
}
async fn write(c: &Client, term: &str, id: Uuid, text: &str) -> Result<Value> {
    api(c
        .post(format!("{term}/input"))
        .json(&json!({"id":id,"data_base64":STANDARD.encode(text)})))
    .await
}
async fn output(c: &Client, term: &str, mut after: u64, until: &str) -> Result<(u64, Vec<u8>)> {
    let mut data = Vec::new();
    for _ in 0..160 {
        let page = api(c.get(format!("{term}/output?after={after}&wait_ms=500"))).await?;
        assert_eq!(page["start"].as_u64(), Some(after));
        assert_eq!(page["missed"], false);
        data.extend(STANDARD.decode(page["data_base64"].as_str().unwrap())?);
        after = page["next"].as_u64().unwrap();
        if String::from_utf8_lossy(&data).contains(until) {
            return Ok((after, data));
        }
    }
    Err(format!("terminal did not produce {until:?}").into())
}

#[tokio::test]
async fn terminal_cursor_resize_retries_exit_and_host_restart() -> Result<()> {
    let t = tempfile::tempdir()?;
    let root = t.path().join("roots");
    let primary = root.join("project");
    let work = root.join("linked-worktree");
    fs::create_dir_all(&primary)?;
    let git = |args: &[&str]| -> Result<()> {
        let result = std::process::Command::new("git")
            .current_dir(&primary)
            .args(args)
            .output()?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
        }
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&[
        "-c",
        "user.name=Kar",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "--allow-empty",
        "-qm",
        "initial",
    ])?;
    git(&[
        "worktree",
        "add",
        "--detach",
        work.to_str().unwrap(),
        "HEAD",
    ])?;
    let state = t.path().join("state");
    let server = Server::start(&state, &root).await?;
    let (a, b) = (Client::new(), Client::new());
    let p = server.register(&a, &primary).await?;
    let id = Uuid::new_v4();
    api(a
        .post(format!("{p}/terminals"))
        .json(&json!({"id":id,"worktree":work})))
    .await?;
    let term = format!("{p}/terminals/{id}");
    assert_eq!(
        a.get(format!("{term}/output")).send().await?.status(),
        StatusCode::UNAUTHORIZED
    );
    api(a
        .put(format!("{term}/size"))
        .json(&json!({"rows":37,"cols":103})))
    .await?;
    // Keep the marker itself out of the shell's echoed command line.
    let text = "printf X >> count; pwd -P; stty size; printf 'TOKEN=%s\\n' \"${KAR_WORKBENCH_TOKEN-unset}\"; printf 'part%s\\n' one\n";
    let id = Uuid::new_v4();
    let sent = write(&a, &term, id, text).await?;
    assert_eq!(sent["delivered"], true);
    assert_eq!(write(&b, &term, id, text).await?, sent);
    let (cursor, first) = output(&b, &term, 0, "partone").await?;
    let first = String::from_utf8_lossy(&first);
    assert!(first.contains(work.canonicalize()?.to_str().unwrap()));
    assert!(first.contains("37 103"));
    assert!(first.contains("TOKEN=unset"));
    assert_eq!(fs::read(work.join("count"))?, b"X");
    let another = Client::new();
    write(
        &another,
        &term,
        Uuid::new_v4(),
        "printf 'later%s\\n' output; exit 7\n",
    )
    .await?;
    let (_, tail) = output(&b, &term, cursor, "lateroutput").await?;
    assert!(!String::from_utf8_lossy(&tail).contains("partone"));
    for _ in 0..100 {
        if api(a.get(&term)).await?["status"]["state"] == "exited" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(api(a.get(&term)).await?["status"]["code"], 7);
    let live = terminal(&a, &p).await?;
    write(&a, &live, Uuid::new_v4(), "printf 'before%s\\n' restart\n").await?;
    output(&a, &live, 0, "beforerestart").await?;
    let path = term.strip_prefix(&server.base).unwrap().to_owned();
    let live_path = live.strip_prefix(&server.base).unwrap().to_owned();
    server.kill().await?;
    let restarted = Server::start(&state, &root).await?;
    assert_eq!(
        api(b.get(format!("{}{path}", restarted.base))).await?["status"]["code"],
        7
    );
    let old = format!("{}{live_path}", restarted.base);
    let old_status = api(b.get(&old)).await?;
    assert_eq!(old_status["status"]["state"], "interrupted");
    assert!(old_status["status"]["process_alive"].is_boolean());
    output(&b, &old, 0, "beforerestart").await?;
    api(b.delete(&old)).await?;
    Ok(())
}

// Executed as a standalone Rust HTTP server inside the test PTY.
#[test]
#[ignore = "launched by the preview scenario"]
fn fixture_http_server() {
    let name = std::env::var("KAR_TERMINAL_FIXTURE_NAME").expect("fixture role");
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    fs::write("fixture-port", l.local_addr().unwrap().port().to_string()).unwrap();
    for stream in l.incoming() {
        let mut stream = stream.unwrap();
        let mut input = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while input.read_line(&mut line).unwrap_or(0) > 0 {
            if line == "\r\n" {
                break;
            }
            line.clear();
        }
        let body = format!("<html><body>Kar preview {name}</body></html>");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    }
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\"'\"'"))
}
struct Fixture {
    term: String,
    p: String,
    target: Value,
    grant: Value,
}
async fn launch(server: &Server, c: &Client, path: &Path, name: &str) -> Result<Fixture> {
    fs::create_dir_all(path)?;
    let p = server.register(c, path).await?;
    let term = terminal(c, &p).await?;
    let shell = format!(
        "KAR_TERMINAL_FIXTURE_NAME={name} {} --ignored --exact fixture_http_server --nocapture\n",
        quote(&std::env::current_exe()?)
    );
    write(c, &term, Uuid::new_v4(), &shell).await?;
    let mut target = None;
    for _ in 0..100 {
        if let Some(port) = fs::read_to_string(path.join("fixture-port"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            let discovered = api(c.get(format!("{term}/ports"))).await?;
            target = discovered
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["port"].as_u64() == Some(port))
                .cloned();
            if target.is_some() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let target = target.ok_or("project listener not discovered")?;
    let grant = api(c
        .post(format!("{p}/previews"))
        .json(&json!({"terminal":term.rsplit('/').next(),"target":target})))
    .await?;
    Ok(Fixture {
        term,
        p,
        target,
        grant,
    })
}
async fn browser_origin_url(grant: &Value) -> Result<(Client, url::Url)> {
    let url = url::Url::parse(grant["url"].as_str().unwrap())?;
    let client = Client::builder()
        .no_proxy()
        .resolve(
            url.host_str().unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), url.port().unwrap()),
        )
        .build()?;
    Ok((client, url))
}

#[tokio::test]
async fn projects_own_distinct_http_listeners_and_revocable_tunnels() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("root");
    fs::create_dir(&root)?;
    let server = Server::start(&tmp.path().join("state"), &root).await?;
    let c = Client::new();
    let one = launch(&server, &c, &root.join("one"), "one").await?;
    let two = launch(&server, &c, &root.join("two"), "two").await?;
    let (one_browser, one_url) = browser_origin_url(&one.grant).await?;
    let (two_browser, two_url) = browser_origin_url(&two.grant).await?;
    assert!(
        one_browser
            .get(one_url.clone())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
            .contains("Kar preview one")
    );
    assert!(
        two_browser
            .get(two_url.clone())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
            .contains("Kar preview two")
    );
    let wrong = c
        .post(format!("{}/previews", one.p))
        .bearer_auth(TOKEN)
        .json(&json!({"terminal":one.term.rsplit('/').next(),"target":two.target}))
        .send()
        .await?;
    assert!(!wrong.status().is_success());
    let grant_id = one.grant["id"].as_str().unwrap();
    assert_eq!(
        c.delete(format!("{}/previews/{grant_id}", two.p))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let url = format!("{}/previews/{grant_id}/tunnel", one.p).replacen("http://", "ws://", 1);
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
    ws.send(Message::Binary(
        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            .as_slice()
            .into(),
    ))
    .await?;
    let mut response = Vec::new();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Binary(chunk) = msg {
            response.extend(chunk);
            if String::from_utf8_lossy(&response).contains("Kar preview one") {
                break;
            }
        }
    }
    assert!(String::from_utf8_lossy(&response).contains("Kar preview one"));
    api(c.delete(format!("{}/previews/{grant_id}", one.p))).await?;
    assert!(
        one_browser
            .get(one_url)
            .send()
            .await
            .map_or(true, |r| !r.status().is_success())
    );
    assert!(
        two_browser
            .get(two_url.clone())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
            .contains("Kar preview two")
    );
    api(c.delete(&two.term)).await?;
    assert!(
        two_browser
            .get(two_url)
            .send()
            .await
            .map_or(true, |r| !r.status().is_success())
    );
    api(c.delete(&one.term)).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Google Chrome for macOS; set KAR_TEST_CHROME for a different installed path"]
async fn chrome_loads_project_preview() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("root");
    fs::create_dir(&root)?;
    let server = Server::start(&tmp.path().join("state"), &root).await?;
    let c = Client::new();
    let fixture = launch(&server, &c, &root.join("browser"), "browser").await?;
    let chrome = std::env::var_os("KAR_TEST_CHROME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    let output = Command::new(chrome)
        .args([
            "--headless",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--dump-dom",
        ])
        .arg(format!(
            "--user-data-dir={}",
            tmp.path().join("chrome").display()
        ))
        .arg(fixture.grant["url"].as_str().unwrap())
        .output()
        .await?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("<body>Kar preview browser</body>"));
    api(c.delete(&fixture.term)).await?;
    Ok(())
}

#[tokio::test]
async fn clear_wakes_two_observers_and_survives_restart_without_replacing_the_pty() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("root");
    let project = root.join("project");
    fs::create_dir_all(&project)?;
    let state = tmp.path().join("state");
    let server = Server::start(&state, &root).await?;
    let (a, b) = (Client::new(), Client::new());
    let scope = server.register(&a, &project).await?;
    let one = terminal(&a, &scope).await?;
    let other = terminal(&a, &scope).await?;
    let process = api(a.get(&one)).await?["process"].clone();
    write(
        &a,
        &one,
        Uuid::new_v4(),
        "stty -echo; printf 'erase%s\\n' old\n",
    )
    .await?;
    output(&a, &one, 0, "eraseold").await?;
    write(&a, &other, Uuid::new_v4(), "printf 'keep%s\\n' other\n").await?;
    output(&a, &other, 0, "keepother").await?;
    let after = api(b.get(format!("{one}/output"))).await?["end"]
        .as_u64()
        .unwrap();
    let watch = |client: Client, one: String| async move {
        let mut cursor = after;
        loop {
            let page = api(client.get(format!("{one}/output?after={cursor}&wait_ms=5000"))).await?;
            if !page["events"].as_array().unwrap().is_empty() {
                return Result::Ok(page);
            }
            cursor = page["next"].as_u64().unwrap();
        }
    };
    let reader_a = tokio::spawn(watch(a.clone(), one.clone()));
    let reader_b = tokio::spawn(watch(b.clone(), one.clone()));
    let event = api(a.post(format!("{one}/clear"))).await?;
    let mark = event["cursor"].as_u64().unwrap();
    assert_eq!(event["type"], "cleared");
    assert!(mark > after);
    for reader in [reader_a, reader_b] {
        let page = tokio::time::timeout(Duration::from_secs(7), reader).await???;
        assert_eq!(page["events"], json!([event]));
        assert_eq!(page["oldest"], mark);
    }
    write(&b, &one, Uuid::new_v4(), "printf 'after%s\\n' clear\n").await?;
    let (_, new) = output(&a, &one, mark, "afterclear").await?;
    assert!(!String::from_utf8_lossy(&new).contains("eraseold"));
    assert_eq!(api(a.get(&one)).await?["process"], process);
    let suffixes = [one, other].map(|s| s.strip_prefix(&server.base).unwrap().to_owned());
    server.kill().await?;
    let restarted = Server::start(&state, &root).await?;
    for (index, path) in suffixes.iter().enumerate() {
        let page = api(a.get(format!("{}{path}/output", restarted.base))).await?;
        let bytes = STANDARD.decode(page["data_base64"].as_str().unwrap())?;
        let bytes = String::from_utf8_lossy(&bytes);
        if index == 0 {
            assert_eq!(page["events"], json!([event]));
            assert!(bytes.contains("afterclear") && !bytes.contains("eraseold"));
        } else {
            assert_eq!(page["events"], json!([]));
            assert!(bytes.contains("keepother"));
        }
    }
    restarted.kill().await?;
    Ok(())
}

#[tokio::test]
async fn stop_checks_live_owner_and_job_and_only_terminates_the_selected_listener() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    fs::create_dir(&root)?;
    let server = Server::start(&temp.path().join("state"), &root).await?;
    let c = Client::new();
    let one = launch(&server, &c, &root.join("one"), "one").await?;
    let two = launch(&server, &c, &root.join("two"), "two").await?;
    let request = |term: &str, target: &Value| {
        c.post(format!("{term}/ports/stop")).bearer_auth(TOKEN)
        .json(&json!({"pid":target["owner"]["pid"],"born":target["owner"]["born"],"port":target["port"]}))
    };
    assert_eq!(
        request(&one.term, &two.target).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    let mut wrong = one.target.clone();
    wrong["owner"]["born"] = 0.into();
    assert_eq!(
        request(&one.term, &wrong).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    request(&one.term, &one.target)
        .send()
        .await?
        .error_for_status()?;
    let addr = |s: &Fixture| format!("127.0.0.1:{}", s.target["port"]);
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr(&one)).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(tokio::net::TcpStream::connect(addr(&one)).await.is_err());
    let (other, url) = browser_origin_url(&two.grant).await?;
    assert!(
        other
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
            .contains("Kar preview two")
    );
    assert_eq!(
        request(&one.term, &one.target).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(api(c.get(&one.term)).await?["status"]["state"], "running");
    for fixture in [one, two] {
        api(c.delete(fixture.term)).await?;
    }
    server.kill().await?;
    Ok(())
}
