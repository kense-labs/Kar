use super::*;

// The cloud bearer remains server-side. The local grant can operate only its view's device.
pub(super) async fn invoke(cloud: &Cloud, l: &Lease, q: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        #[serde(rename = "type")]
        _kind: String,
        command: String,
        #[serde(default)]
        arguments: Vec<String>,
    }
    let input: Input =
        serde_json::from_value(q.clone()).map_err(|_| bad("invalid scoped device command"))?;
    if !matches!(
        input.command.as_str(),
        "snapshot" | "press" | "tap" | "swipe" | "scroll" | "type" | "back" | "home"
    ) || input.arguments.len() > 12
        || input
            .arguments
            .iter()
            .any(|v| v.len() > 16_384 || v.contains('\0'))
    {
        return Err(bad("unsupported scoped device command or arguments"));
    }
    if l.host.kind == Provider::Simlock {
        return simlock::command(&cloud.client, l, &input.command, input.arguments).await;
    }
    let session = format!("kar-{}", Uuid::new_v4().simple());
    let result = async {
        expo_command(cloud, l, &session, "open", &[]).await?;
        expo_command(cloud, l, &session, &input.command, &input.arguments).await
    }
    .await;
    let close = expo_command(cloud, l, &session, "close", &[]).await;
    let output = result?;
    close?;
    Ok(output)
}
async fn expo_command(
    cloud: &Cloud,
    l: &Lease,
    session: &str,
    command: &str,
    args: &[String],
) -> Result<Value> {
    let url = l
        .agent_url
        .as_deref()
        .ok_or_else(|| bad("Expo agent endpoint is unavailable"))?;
    let secret = l
        .agent_token
        .as_deref()
        .ok_or_else(|| bad("Expo agent credential is unavailable"))?;
    let mut flags = json!({"platform":l.platform});
    flags[if l.platform == "ios" {
        "udid"
    } else {
        "serial"
    }] = json!(
        l.remote_device
            .as_deref()
            .ok_or_else(|| bad("Expo agent device is unavailable"))?
    );
    let id = Uuid::new_v4().to_string();
    let reply=json_response(cloud.client.post(format!("{}/rpc",url.trim_end_matches('/'))).bearer_auth(secret)
        .json(&json!({"jsonrpc":"2.0","id":id,"method":"agent_device.command","params":{"command":command,"session":session,"positionals":args,"flags":flags}}))).await?;
    if reply["id"] != id || reply["result"]["ok"] != true {
        return Err(bad("Expo device agent did not confirm the command"));
    }
    Ok(reply["result"]["data"].clone())
}
