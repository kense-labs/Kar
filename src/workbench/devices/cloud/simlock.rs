use super::*;

pub(super) async fn catalog(client: &Client, host: &Host) -> Result<Vec<Device>> {
    let reply = json_response(
        host.auth()?
            .apply(client.get(format!("{}/v1/catalog", host.gateway()?))),
    )
    .await?;
    let mut rows = vec![];
    for entry in reply["platforms"]
        .as_array()
        .ok_or_else(|| bad("invalid Simlock platform catalog"))?
    {
        let platform = match entry["platform"].as_str() {
            Some("ios") => "ios",
            Some("android") => "android",
            _ => continue,
        };
        for (model, runtimes) in entry["modelRuntimes"]
            .as_object()
            .ok_or_else(|| bad("invalid Simlock model catalog"))?
        {
            if entry["modelWorkers"][model]
                .as_array()
                .is_some_and(Vec::is_empty)
                || entry["modelClasses"][model]
                    .as_str()
                    .is_some_and(|s| !matches!(s, "phone" | "tablet"))
            {
                continue;
            }
            let Some(runtime) = runtimes
                .as_array()
                .and_then(|v| v.last())
                .and_then(Value::as_str)
            else {
                continue;
            };
            let version = format!(
                "{} {}",
                if platform == "ios" { "iOS" } else { "Android" },
                runtime
            );
            rows.push(Device::new(
                &format!("simlock:{platform}:{model}"),
                platform,
                model,
                version,
                false,
                false,
            ));
        }
    }
    Ok(rows)
}

fn backend(l: &Lease) -> String {
    format!("{}-instance", l.platform)
}
fn run(l: &Lease) -> String {
    format!("proxy-{}", l.session())
}
async fn rpc(client: &Client, l: &Lease, method: &str, params: Value) -> Result<Value> {
    let id = Uuid::new_v4().to_string();
    let request = l
        .auth
        .apply(client.post(format!("{}/v1/agent-device/rpc", l.host.gateway()?)))
        .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
    let response = json_response(request).await?;
    if response["id"] != id || response["jsonrpc"] != "2.0" || response["result"]["ok"] != true {
        return Err(bad("Simlock did not confirm the device RPC"));
    }
    Ok(response["result"]["data"].clone())
}
async fn command(
    client: &Client,
    l: &Lease,
    name: &str,
    positionals: Vec<String>,
) -> Result<Value> {
    let mut meta = json!({"tenantId":"proxy","runId":run(l),"clientId":l.session(),"leaseId":l.id(),
        "leaseBackend":backend(l),"leaseProvider":"proxy","sessionIsolation":"tenant",
        "requestId":Uuid::new_v4().to_string()});
    if let Some(key) = &l.device_key {
        meta["deviceKey"] = json!(key);
    }
    if name == "screenshot" {
        meta["clientArtifactPaths"] = json!({"path":"screenshot.png"});
    }
    rpc(
        client,
        l,
        "agent_device.command",
        json!({"session":l.session(),"command":name,"positionals":positionals,
        "flags":{"platform":l.platform,"device":l.name},"meta":meta}),
    )
    .await
}
pub(super) async fn start(cloud: &Cloud, l: &mut Lease) -> Result<()> {
    let proposed = l.key.simple().to_string();
    let result=rpc(&cloud.client,l,"agent_device.lease.allocate",json!({
        "session":l.session(),"tenantId":"proxy","runId":run(l),"clientId":l.session(),"backend":backend(l),
        "leaseProvider":"proxy","leaseId":proposed,"platform":l.platform,"device":l.name
    })).await?;
    let lease = &result["lease"];
    if lease["tenantId"] != "proxy"
        || lease["runId"] != run(l)
        || lease["backend"] != backend(l)
        || lease["clientId"]
            .as_str()
            .is_some_and(|id| id != l.session())
    {
        return Err(bad("Simlock returned a lease for a different request"));
    }
    l.resource = Some(required(lease, "leaseId")?);
    l.device_key = lease["deviceKey"].as_str().map(str::to_owned);
    cloud.save(l)?;
    command(&cloud.client, l, "open", vec![]).await?;
    Ok(())
}
pub(super) async fn renew(client: &Client, l: &Lease) -> Result<bool> {
    let r = l
        .auth
        .apply(client.post(format!(
            "{}/v1/leases/{}/renew",
            l.host.gateway()?,
            segment(l.id())
        )))
        .send()
        .await
        .map_err(|_| bad("Simlock renewal unconfirmed"))?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(false)
    } else if r.status().is_success() {
        Ok(true)
    } else {
        Err(bad("Simlock refused lease renewal"))
    }
}
pub(super) async fn stop(cloud: &Cloud, l: &Lease) -> Result<bool> {
    let proposed = l.key.simple().to_string();
    let id = l.resource.as_deref().unwrap_or(&proposed);
    let r = l
        .auth
        .apply(
            cloud
                .client
                .delete(format!("{}/v1/leases/{}", l.host.gateway()?, segment(id))),
        )
        .send()
        .await
        .map_err(|_| bad("Simlock release unconfirmed"))?;
    if r.status().is_success() {
        return Ok(true);
    }
    if r.status() != reqwest::StatusCode::NOT_FOUND {
        return Err(bad("Simlock refused lease release"));
    }
    if l.resource.is_some() {
        return Ok(true);
    }
    // Older gateways may substitute their own id. Ask the daemon to release the original
    // tenant/run without creating another lease. A negative reply cannot prove a lost create failed.
    let reply=rpc(&cloud.client,l,"agent_device.lease.release",json!({
        "session":l.session(),"tenantId":"proxy","runId":run(l),"clientId":l.session(),"leaseProvider":"proxy"
    })).await?;
    Ok(reply["released"] == true)
}
pub(super) async fn screen(client: &Client, l: &Lease) -> Result<Vec<u8>> {
    let filename = format!(
        "/tmp/agent-device-screenshot-{}-{}.png",
        now(),
        Uuid::new_v4().simple()
    );
    let data = command(client, l, "screenshot", vec![filename]).await?;
    let artifact = data["artifacts"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["field"] == "path"))
        .and_then(|a| a["artifactId"].as_str())
        .ok_or_else(|| bad("Simlock screenshot did not return an artifact"))?;
    png(l
        .auth
        .apply(client.get(format!(
            "{}/v1/agent-device/artifacts/{}",
            l.host.gateway()?,
            segment(artifact)
        )))
        .header("x-agent-device-tenant", "proxy")
        .header("x-agent-device-lease-id", l.id()))
    .await
}
