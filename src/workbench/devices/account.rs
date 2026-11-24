use super::*;
use reqwest::Client;
use std::{io::Read, time::Duration};
fn bad(msg: &str) -> Error {
    Error::Refused(msg.into())
}
fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| bad("could not initialize device host client"))
}
async fn read(builder: reqwest::RequestBuilder, label: &str) -> Result<Value> {
    let mut response = builder
        .send()
        .await
        .map_err(|_| bad(&format!("{label} could not be reached")))?;
    if !response.status().is_success() {
        return Err(bad(&format!(
            "{label} refused the identity check (HTTP {})",
            response.status().as_u16()
        )));
    }
    let mut data = vec![];
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| bad("incomplete identity reply"))?
    {
        if data.len() + chunk.len() > 1024 * 1024 {
            return Err(bad("identity reply too large"));
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| bad("invalid identity response"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Expo {
    access_token: Option<String>,
    host_id: Option<String>,
}
fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}
pub(super) fn login() -> Option<String> {
    let path = PathBuf::from(std::env::var_os("EXPO_HOME").unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".expo")
            .into_os_string()
    }))
    .join("state.json");
    let mut data = vec![];
    File::open(path)
        .ok()?
        .take(1024 * 1024)
        .read_to_end(&mut data)
        .ok()?;
    serde_json::from_slice::<Value>(&data).ok()?["auth"]["sessionSecret"]
        .as_str()
        .map(str::to_owned)
}
pub(super) async fn eas(State(app): State<Workbench>, Json(q): Json<Expo>) -> Reply<Json<Value>> {
    let request = client()?.post("https://api.expo.dev/graphql");
    let saved = if let Some(id) = &q.host_id {
        let s = app.0.devices.live.lock().await;
        Some(
            s.settings
                .cloud_hosts
                .iter()
                .find(|h| h.id == *id && h.kind == cloud::Provider::Expo)
                .ok_or_else(|| bad("unknown Expo host"))?
                .token
                .clone(),
        )
        .flatten()
    } else {
        None
    };
    let (request, source) = if let Some(token) = nonempty(q.access_token) {
        (request.bearer_auth(token), "settings")
    } else if let Some(token) = nonempty(saved) {
        (request.bearer_auth(token), "settings")
    } else if let Some(token) = nonempty(std::env::var("EXPO_TOKEN").ok()) {
        (request.bearer_auth(token), "env")
    } else if let Some(secret) = nonempty(login()) {
        (request.header("expo-session", secret), "eas-login")
    } else {
        return Err(bad("supply an Expo access token or sign in with EAS").into());
    };
    let v=read(request.json(&json!({"query":"query KarDeviceViewer { meActor { __typename ... on UserActor { username } ... on Robot { firstName } accounts { appsPaginated(first: 100) { edges { node { id fullName } } } } } }"})),"Expo").await?;
    let actor = &v["data"]["meActor"];
    if v.get("errors").is_some() || !actor.is_object() {
        return Err(bad("Expo refused the account check").into());
    }
    let name = ["username", "firstName", "__typename"]
        .iter()
        .find_map(|k| actor[*k].as_str())
        .ok_or_else(|| bad("missing Expo identity"))?;
    let mut apps = vec![];
    for account in actor["accounts"].as_array().into_iter().flatten() {
        for edge in account["appsPaginated"]["edges"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let (Some(id), Some(full)) = (
                edge["node"]["id"].as_str(),
                edge["node"]["fullName"].as_str(),
            ) {
                apps.push(json!({"id":id,"full_name":full}));
            }
        }
    }
    Ok(Json(
        json!({"signed_in_as":name,"credential_source":source,"apps":apps}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Gateway {
    gateway_url: Option<String>,
    agent_token: Option<String>,
    host_id: Option<String>,
}
pub(super) async fn simlock(
    State(app): State<Workbench>,
    Json(q): Json<Gateway>,
) -> Reply<Json<Value>> {
    let machine = std::env::var("AGENT_DEVICE_DAEMON_BASE_URL")
        .ok()
        .and_then(|s| {
            s.trim_end_matches('/')
                .strip_suffix("/v1/agent-device")
                .map(str::to_string)
        });
    let saved = if let Some(id) = &q.host_id {
        let s = app.0.devices.live.lock().await;
        Some(
            s.settings
                .cloud_hosts
                .iter()
                .find(|h| h.id == *id && h.kind == cloud::Provider::Simlock)
                .cloned()
                .ok_or_else(|| bad("unknown Simlock host"))?,
        )
    } else {
        None
    };
    let gateway = nonempty(q.gateway_url)
        .or_else(|| saved.as_ref().and_then(|h| h.gateway_url.clone()))
        .or_else(|| machine.clone())
        .ok_or_else(|| bad("no Simlock gateway configured"))?;
    let gateway = gateway.trim_end_matches('/');
    let url = url::Url::parse(gateway).map_err(|_| bad("invalid Simlock gateway URL"))?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))))
    {
        return Err(bad("Simlock requires an HTTPS or loopback gateway URL").into());
    }
    let (token, source) = if let Some(token) = nonempty(q.agent_token) {
        (token, "entered")
    } else if let Some(token) = saved
        .as_ref()
        .filter(|h| {
            h.gateway_url
                .as_deref()
                .is_some_and(|u| u.trim_end_matches('/') == gateway)
        })
        .and_then(|h| nonempty(h.token.clone()))
    {
        (token, "settings")
    } else if machine.as_deref() == Some(gateway) {
        (
            nonempty(std::env::var("AGENT_DEVICE_DAEMON_AUTH_TOKEN").ok())
                .ok_or_else(|| bad("no agent token for this gateway"))?,
            "env",
        )
    } else {
        return Err(bad("no agent token for this gateway").into());
    };
    let v = read(
        client()?
            .get(format!("{gateway}/v1/identity"))
            .bearer_auth(token),
        "Simlock",
    )
    .await?;
    if v["role"] != "agent" {
        return Err(bad("Simlock requires an agent role token").into());
    }
    let name = v["name"]
        .as_str()
        .ok_or_else(|| bad("missing Simlock identity"))?;
    Ok(Json(
        json!({"signed_in_as":name,"token_source":source,"gateway_url":gateway}),
    ))
}
