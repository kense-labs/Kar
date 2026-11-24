use super::*;

struct Feed {
    response: reqwest::Response,
    buffer: Vec<u8>,
}
impl Feed {
    async fn open(req: RequestBuilder) -> Result<Self> {
        Self::authenticated(req, TOKEN).await
    }
    async fn authenticated(req: RequestBuilder, token: &str) -> Result<Self> {
        Ok(Self {
            response: req.bearer_auth(token).send().await?.error_for_status()?,
            buffer: vec![],
        })
    }
    async fn matching(&mut self, f: impl Fn(&Value) -> bool) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
                    let frame = self.buffer.drain(..end + 2).collect::<Vec<_>>();
                    for s in std::str::from_utf8(&frame)?.lines() {
                        if let Some(data) = s.strip_prefix("data:") {
                            let row: Value = serde_json::from_str(data.trim())?;
                            if f(&row) {
                                return Ok(row);
                            }
                        }
                    }
                } else if let Some(chunk) = self.response.chunk().await? {
                    self.buffer.extend(chunk);
                } else {
                    return Err("unexpected end of terminal stream".into());
                }
            }
        })
        .await?
    }
}
async fn attach(c: &Client, term: &str, viewer: Uuid, cols: u16, rows: u16) -> Result<Feed> {
    Feed::open(
        c.post(format!("{term}/attach"))
            .json(&json!({"viewer_id":viewer,"cols":cols,"rows":rows})),
    )
    .await
}
async fn marked(c: &Client, term: &str, id: Uuid, viewer: Uuid, data: &[u8]) -> Result<Value> {
    api(c.post(format!("{term}/input")).json(
        &json!({"id":id,"viewer_id":viewer,"data_base64":STANDARD.encode(data),"reply":true}),
    ))
    .await
}
fn grid(row: &Value, cols: u16, rows: u16) -> bool {
    row["cols"] == cols && row["rows"] == rows
}
#[tokio::test]
async fn multiple_viewers_share_grid_query_writes_and_metadata_without_observer_votes() -> Result<()>
{
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("roots");
    let project = root.join("project");
    fs::create_dir_all(&project)?;
    let server = Server::start(&tmp.path().join("state"), &root).await?;
    let (a, b, reader) = (Client::new(), Client::new(), Client::new());
    let p = server.register(&a, &project).await?;
    let mut directory = Feed::open(reader.get(format!("{p}/terminals/metadata"))).await?;
    let initial = directory.matching(|v| v["type"] == "snapshot").await?;
    assert_eq!(initial["terminals"], json!([]));
    let id = Uuid::new_v4();
    let term = format!("{p}/terminals/{id}");
    assert_eq!(
        reader
            .get(format!("{term}/observe"))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let (wide, narrow) = (Uuid::new_v4(), Uuid::new_v4());
    let mut big = attach(&a, &term, wide, 120, 40).await?;
    let creation = directory
        .matching(|v| v["type"] == "upsert" && v["terminal"]["status"]["state"] == "running")
        .await?;
    assert_eq!(creation["terminal"]["id"], id.to_string());
    assert!(creation.get("data_base64").is_none());
    big.matching(|e| e["type"] == "snapshot" && grid(&e["page"]["terminal"], 120, 40))
        .await?;
    let mut small = attach(&b, &term, narrow, 80, 24).await?;
    small
        .matching(|e| grid(&e["page"]["terminal"], 80, 24))
        .await?;
    big.matching(|e| grid(&e["page"]["terminal"], 80, 24))
        .await?;
    let mut observer =
        Feed::open(reader.get(format!("{term}/observe?max_history_bytes=0"))).await?;
    let seen = observer.matching(|e| e["type"] == "snapshot").await?;
    assert!(seen["viewer_id"].is_null());
    let size = |who: Option<Uuid>, cols: u16, rows: u16| {
        a.put(format!("{term}/size"))
            .json(&json!({"viewer_id":who,"cols":cols,"rows":rows}))
    };
    assert!(grid(&api(size(None, 200, 60)).await?, 80, 24));
    assert!(grid(&api(size(Some(narrow), 90, 50)).await?, 90, 40));
    let mut retry = attach(&b, &term, narrow, 80, 24).await?;
    retry
        .matching(|e| grid(&e["page"]["terminal"], 90, 40))
        .await?;
    drop(small);
    let da = b"\x1b[?1;2c";
    let cmd = format!(
        "stty raw -echo; printf '\\033[c'; dd bs=1 count={} of=query-bytes 2>/dev/null; stty sane; printf 'capture%s\\n' done\n",
        2 * da.len() + 1
    );
    write(&a, &term, Uuid::new_v4(), &cmd).await?;
    output(&b, &term, 0, "\x1b[c").await?;
    directory
        .matching(|v| {
            v["type"] == "upsert"
                && v["terminal"]["has_running_subprocess"] == true
                && v["terminal"]["label"] == "dd"
        })
        .await?;
    let first = Uuid::new_v4();
    assert_eq!(
        marked(&b, &term, first, narrow, da).await?["delivered"],
        true
    );
    assert_eq!(
        marked(&b, &term, first, narrow, da).await?["delivered"],
        true
    );
    let code = api(a.post(format!("{}/access/pairings", server.base))).await?;
    let device: Value = b
        .post(format!("{}/access/pair", server.base))
        .json(&json!({"credential":code["credential"],"label":"terminal tablet"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = device["token"].as_str().unwrap();
    // A separate authorized device may pick the same view UUID without inheriting the first device's votes.
    let mut tablet = Feed::authenticated(
        b.post(format!("{term}/attach"))
            .json(&json!({"viewer_id":narrow,"cols":52,"rows":11})),
        token,
    )
    .await?;
    tablet
        .matching(|e| grid(&e["page"]["terminal"], 52, 11))
        .await?;
    let remote_reply:Value=b.post(format!("{term}/input")).bearer_auth(token)
        .json(&json!({"id":Uuid::new_v4(),"viewer_id":narrow,"reply":true,"data_base64":STANDARD.encode(da)})).send().await?.error_for_status()?.json().await?;
    assert_eq!(remote_reply["suppressed"], true);
    a.delete(format!(
        "{}/access/devices/{}",
        server.base,
        device["id"].as_str().unwrap()
    ))
    .bearer_auth(TOKEN)
    .send()
    .await?
    .error_for_status()?;
    big.matching(|e| grid(&e["page"]["terminal"], 90, 40))
        .await?;
    let duplicate = Uuid::new_v4();
    assert_eq!(
        marked(&a, &term, duplicate, wide, da).await?["suppressed"],
        true
    );
    write(&a, &term, Uuid::new_v4(), "x").await?;
    drop(retry);
    big.matching(|e| grid(&e["page"]["terminal"], 120, 40))
        .await?;
    assert_eq!(
        marked(&b, &term, Uuid::new_v4(), narrow, da).await?["suppressed"],
        true
    );
    assert_eq!(
        marked(&a, &term, duplicate, wide, da).await?["suppressed"],
        true
    );
    assert_eq!(
        marked(&a, &term, Uuid::new_v4(), wide, da).await?["delivered"],
        true
    );
    output(&reader, &term, 0, "capturedone").await?;
    assert_eq!(
        fs::read(project.join("query-bytes"))?,
        [&da[..], b"x", &da[..]].concat()
    );
    let settled = write(&a, &term, Uuid::new_v4(), "exit 3\n").await?;
    assert_eq!(settled["delivered"], true);
    directory
        .matching(|v| v["type"] == "upsert" && v["terminal"]["status"]["code"] == 3)
        .await?;
    let mut exited = Feed::open(reader.get(format!("{term}/observe"))).await?;
    let old = exited.matching(|e| e["type"] == "snapshot").await?;
    assert_eq!(old["page"]["terminal"]["status"]["code"], 3);
    api(a.delete(&term)).await?;
    directory
        .matching(|v| v["type"] == "remove" && v["id"] == id.to_string())
        .await?;
    drop(directory);
    drop(observer);
    drop(exited);
    drop(big);
    let mut reopened = Feed::open(reader.get(format!("{p}/terminals/metadata"))).await?;
    assert_eq!(
        reopened.matching(|v| v["type"] == "snapshot").await?["terminals"],
        json!([])
    );
    Ok(())
}
