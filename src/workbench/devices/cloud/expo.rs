use super::*;
use std::io::Read;

const CREATE: &str = "mutation KarCreateSimulator($input: CreateDeviceRunSessionInput!) { deviceRunSession { createDeviceRunSession(deviceRunSessionInput: $input) { id } } }";
const POLL: &str = "query KarSimulator($id: ID!) { deviceRunSessions { byId(deviceRunSessionId: $id) { id status remoteConfig { __typename ... on AgentDeviceRunSessionRemoteConfig { agentDeviceRemoteSessionUrl agentDeviceRemoteSessionToken webPreviewToken previewApiUrl } } } } }";
const ACTIVE: &str = "query KarActiveSimulators($appId: String!, $filter: DeviceRunSessionFilterInput, $after: String) { app { byId(appId: $appId) { deviceRunSessionsPaginated(first: 50, filter: $filter, after: $after) { edges { node { id status name } } pageInfo { hasNextPage endCursor } } } } }";
const STOP: &str = "mutation KarStopSimulator($id: ID!) { deviceRunSession { ensureDeviceRunSessionStopped(deviceRunSessionId: $id) { id status } } }";

pub(super) fn catalog() -> Vec<Device> {
    [
        ("ios", "iPhone (Expo Simulator)"),
        ("android", "Android (Expo Simulator)"),
    ]
    .into_iter()
    .map(|(p, name)| Device::new(&format!("eas-{p}"), p, name, "Cloud".into(), false, false))
    .collect()
}
fn project_file(path: &Path) -> Result<Option<String>> {
    let mut s = String::new();
    match File::open(path) {
        Ok(file) => {
            file.take(256 * 1024 + 1).read_to_string(&mut s)?;
            if s.len() > 256 * 1024 {
                return Err(bad("Expo app config exceeds size limit"));
            }
            Ok(Some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn config_id(v: &Value) -> Option<Uuid> {
    v.get("expo")
        .unwrap_or(v)
        .pointer("/extra/eas/projectId")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}
fn workspace_project(cwd: &Path) -> Result<Option<String>> {
    let mut dirs = vec![cwd.to_owned()];
    for parent in ["apps", "packages"] {
        match std::fs::read_dir(cwd.join(parent)) {
            Ok(rows) => {
                for row in rows {
                    let e = row?;
                    if e.file_type()?.is_dir() {
                        dirs.push(e.path());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut ids = BTreeSet::new();
    for dir in dirs {
        for filename in [
            "app.config.ts",
            "app.config.mts",
            "app.config.js",
            "app.config.mjs",
            "app.config.cjs",
        ] {
            if let Some(source) = project_file(&dir.join(filename))? {
                // Only infer billing ownership from an inspectable literal. Computed configs need the operator's app choice.
                let literal = source
                    .trim()
                    .strip_prefix("export default ")
                    .or_else(|| source.trim().strip_prefix("module.exports = "))
                    .and_then(|s| {
                        serde_json::from_str::<Value>(s.trim_end_matches(';').trim()).ok()
                    });
                let Some(id) = literal.as_ref().and_then(config_id) else {
                    return Ok(None);
                };
                ids.insert(id);
                break;
            }
        }
        for filename in ["app.config.json", "app.json"] {
            if let Some(source) = project_file(&dir.join(filename))? {
                let Ok(v) = serde_json::from_str::<Value>(&source) else {
                    return Ok(None);
                };
                if let Some(id) = config_id(&v) {
                    ids.insert(id);
                }
                break;
            }
        }
    }
    Ok((ids.len() == 1).then(|| ids.first().unwrap().to_string()))
}
pub(super) fn project(cwd: &Path, host: &Host) -> Result<String> {
    match workspace_project(cwd){Ok(Some(id))=>Ok(id),Ok(None)=>host.app_id.clone().ok_or_else(||bad("choose an Expo app for this host or set a unique extra.eas.projectId in the workspace")),Err(e)=>host.app_id.clone().ok_or(e)}
}
async fn query_result(
    cloud: &Cloud,
    l: &Lease,
    doc: &str,
    variables: Value,
) -> std::result::Result<Value, (Error, bool)> {
    let response = l
        .auth
        .apply(cloud.client.post(&cloud.graph))
        .json(&json!({"query":doc,"variables":variables}))
        .send()
        .await
        .map_err(|_| (bad("Expo request ended before a confirmed response"), false))?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err((bad("Expo rejected the account credential"), true));
    }
    let bytes = bounded(response, 2 * 1024 * 1024)
        .await
        .map_err(|e| (e, false))?;
    let reply: Value =
        serde_json::from_slice(&bytes).map_err(|_| (bad("invalid Expo response"), false))?;
    let errors = reply["errors"].as_array().filter(|e| !e.is_empty());
    if errors.is_some() || !status.is_success() || !reply["data"].is_object() {
        let rejected = !status.is_server_error()
            && reply["data"].is_null()
            && errors.is_some_and(|es| es.iter().all(|e| e["extensions"]["errorType"] == "USER"));
        return Err((
            bad("Expo did not confirm the simulator operation"),
            rejected,
        ));
    }
    Ok(reply["data"].clone())
}
async fn query(cloud: &Cloud, l: &Lease, doc: &str, variables: Value) -> Result<Value> {
    query_result(cloud, l, doc, variables)
        .await
        .map_err(|(e, _)| e)
}
fn terminal(s: &Value) -> bool {
    matches!(s.as_str(), Some("STOPPED" | "ERRORED"))
}
fn prefix(l: &Lease) -> &'static str {
    if l.platform == "ios" {
        ""
    } else {
        "/vendor/serve-emu"
    }
}
async fn status(c: &Cloud, l: &Lease, id: &str) -> Result<Value> {
    let v = query(c, l, POLL, json!({"id":id})).await?;
    let row = &v["deviceRunSessions"]["byId"];
    if row["id"] != id {
        return Err(bad("Expo status returned a different session"));
    }
    Ok(row.clone())
}
pub(super) async fn alive(c: &Cloud, l: &Lease) -> Result<bool> {
    Ok(!terminal(&status(c, l, l.id()).await?["status"]))
}
pub(super) async fn start(c: &Cloud, l: &mut Lease) -> Result<()> {
    let created=query_result(c,l,CREATE,json!({"input":{"appId":l.app,"platform":l.platform.to_ascii_uppercase(),"type":"AGENT_DEVICE","name":l.session(),
        "maxRunTimeMinutes":l.host.age(),"maxIdleTimeMinutes":l.host.idle(),"corsOrigins":["http://localhost"]}})).await.map_err(|(e,rejected)|{l.slot_freed=rejected;e})?;
    l.resource = Some(required(
        &created["deviceRunSession"]["createDeviceRunSession"],
        "id",
    )?);
    c.save(l)?;
    let mut failures = 0;
    let config = tokio::time::timeout(Duration::from_secs(15 * 60), async {
        loop {
            match status(c, l, l.id()).await {
                Ok(v) => {
                    failures = 0;
                    if terminal(&v["status"]) {
                        return Err(bad("Expo simulator ended before it was ready"));
                    }
                    if v["status"] == "IN_PROGRESS"
                        && v["remoteConfig"]["__typename"] == "AgentDeviceRunSessionRemoteConfig"
                    {
                        return Ok(v["remoteConfig"].clone());
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    failures += 1;
                    if failures >= 4 {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    })
    .await
    .map_err(|_| bad("Expo simulator startup timed out"))??;
    let mut preview = url::Url::parse(&required(&config, "previewApiUrl")?)
        .map_err(|_| bad("invalid Expo preview URL"))?;
    preview.set_fragment(None);
    preview.set_query(None);
    let preview = safe_url(preview.as_str())?.origin().ascii_serialization();
    let token = required(&config, "webPreviewToken")?;
    l.agent_url = Some(safe_url(&required(&config, "agentDeviceRemoteSessionUrl")?)?.to_string());
    l.agent_token = Some(required(&config, "agentDeviceRemoteSessionToken")?);
    let endpoint = format!("{preview}{}/api", prefix(l));
    let mut last = None;
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let result = json_response(c.client.get(&endpoint).bearer_auth(&token))
            .await
            .and_then(|v| {
                required(
                    &v,
                    if l.platform == "ios" {
                        "device"
                    } else {
                        "serial"
                    },
                )
            });
        match result {
            Ok(id) => {
                l.remote_device = Some(id);
                l.preview = Some(preview);
                l.preview_token = Some(token);
                return Ok(());
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}
async fn stop_id(c: &Cloud, l: &Lease, id: &str) -> Result<bool> {
    if let Ok(v) = query(c, l, STOP, json!({"id":id})).await {
        let row = &v["deviceRunSession"]["ensureDeviceRunSessionStopped"];
        if row["id"] == id && terminal(&row["status"]) {
            return Ok(true);
        }
    }
    Ok(terminal(&status(c, l, id).await?["status"]))
}
pub(super) async fn stop(c: &Cloud, l: &mut Lease) -> Result<bool> {
    if let Some(id) = &l.resource {
        return stop_id(c, l, id).await;
    }
    let mut seen = BTreeSet::new();
    let mut after = None;
    let mut found = false;
    loop {
        let result=query(c,l,ACTIVE,json!({"appId":l.app,"after":after,"filter":{"statuses":["NEW","QUEUED","STARTING","IN_PROGRESS"]}})).await?;
        let page = &result["app"]["byId"]["deviceRunSessionsPaginated"];
        for edge in page["edges"]
            .as_array()
            .ok_or_else(|| bad("Expo active session listing is incomplete"))?
        {
            if edge["node"]["name"] == l.session() {
                found = true;
                if l.slot_freed {
                    l.slot_freed = false;
                    c.save(l)?;
                }
                if !stop_id(c, l, &required(&edge["node"], "id")?).await? {
                    return Ok(false);
                }
            }
        }
        if page["pageInfo"]["hasNextPage"] != true {
            break;
        }
        let next = required(&page["pageInfo"], "endCursor")?;
        if !seen.insert(next.clone()) {
            return Err(bad("Expo active session listing repeated its cursor"));
        }
        after = Some(next);
    }
    // An unacknowledged create can appear later. Keep checking by its exact name for the full provider horizon.
    if !found && !l.slot_freed && now() >= l.created + 5 * 60_000 {
        l.slot_freed = true;
        c.save(l)?;
    }
    Ok(found || now() >= l.created + 180 * 60_000)
}
pub(super) async fn screen(client: &Client, l: &Lease) -> Result<Vec<u8>> {
    let preview = l
        .preview
        .as_deref()
        .ok_or_else(|| bad("Expo preview unavailable"))?;
    let device = l
        .remote_device
        .as_deref()
        .ok_or_else(|| bad("Expo preview device unavailable"))?;
    let token = l
        .preview_token
        .as_deref()
        .ok_or_else(|| bad("Expo preview token unavailable"))?;
    png(client
        .get(format!("{preview}{}/api/screenshot", prefix(l)))
        .query(&[("device", device)])
        .bearer_auth(token))
    .await
}
