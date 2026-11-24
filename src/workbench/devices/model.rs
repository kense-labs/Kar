//! Model access to the same live host catalog and per-chat device views as the workbench.
use super::*;
use crate::workbench::mcp::DevicePermission;
use base64::{Engine as _, engine::general_purpose::STANDARD};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    host_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    host_id: Option<String>,
    device_id: Option<String>,
    platform: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    host_id: Option<String>,
    device_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseArgs {
    host_id: Option<String>,
    device_id: Option<String>,
    shutdown: Option<bool>,
}

fn host(scope: &DevicePermission, host: Option<&str>) -> Result<()> {
    if host.is_some_and(|h| !scope.hosts().contains(h)) {
        return Err(Error::Refused(
            "device host is outside this chat's grant".into(),
        ));
    }
    Ok(())
}
fn enabled(s: &Live) -> Result<()> {
    if !s.settings.enabled || !s.settings.agent_access_enabled {
        return Err(Error::Refused(
            "enable devices and agent access in the workbench device settings".into(),
        ));
    }
    Ok(())
}
fn select<'a>(
    scope: &DevicePermission,
    s: &'a Live,
    chat: Uuid,
    host: Option<&str>,
    id: Option<&str>,
) -> Vec<&'a Session> {
    s.sessions
        .iter()
        .filter(|v| {
            v.chat == chat
                && scope.hosts().contains(&v.host_id)
                && host.is_none_or(|h| v.host_id == h)
                && id.is_none_or(|i| v.device_id == i)
        })
        .collect()
}
pub(in crate::workbench) async fn call(
    app: &Workbench,
    scope: DevicePermission,
    name: &str,
    input: Value,
) -> Result<Value> {
    let write = matches!(name, "kar_device_open" | "kar_device_close");
    let (project, chat) = scope.verify(app, write)?;
    let svc = &app.0.devices;
    match name {
        "kar_device_list" | "kar_device_open" => {
            let (requested, id, platform) = if write {
                let args: OpenArgs = serde_json::from_value(input)?;
                (args.host_id, args.device_id, args.platform)
            } else {
                let args: ListArgs = serde_json::from_value(input)?;
                (args.host_id, None, None)
            };
            host(&scope, requested.as_deref())?;
            let mut live = svc.live.lock().await;
            enabled(&live)?;
            let hosts = scope
                .hosts()
                .iter()
                .filter(|h| requested.as_ref().is_none_or(|r| r == *h))
                .map(|h| live.backend(h))
                .collect::<Result<Vec<_>>>()?;
            let state = svc.refresh_hosts(&mut live, hosts).await;
            scope.verify(app, write)?;
            let permits =
                |h: &str| scope.hosts().contains(h) && requested.as_deref().is_none_or(|r| r == h);
            let available = live.devices.iter().filter(|d| permits(&d.host_id));
            if !write {
                let hosts: Vec<_> = state["hosts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|h| h["id"].as_str().is_some_and(permits))
                    .collect();
                let sessions: Vec<_> = select(&scope, &live, chat, requested.as_deref(), None)
                    .into_iter()
                    .map(|v| json!({"device_id":v.device_id,"host_id":v.host_id}))
                    .collect();
                return Ok(
                    json!({"hosts":hosts,"devices":available.collect::<Vec<_>>(),"open":sessions}),
                );
            }
            let candidates: Vec<_> = available
                .filter(|d| {
                    id.as_deref().is_none_or(|i| d.id == i)
                        && platform.as_deref().is_none_or(|p| d.platform == p)
                })
                .collect();
            let first = candidates
                .first()
                .ok_or_else(|| Error::Refused("no matching device on a granted host".into()))?;
            if candidates
                .iter()
                .any(|d| d.host_id != first.host_id || d.platform != first.platform)
            {
                return Err(Error::Refused(
                    "ambiguous device; specify host_id and platform or device_id".into(),
                ));
            }
            let d = candidates
                .iter()
                .copied()
                .find(|d| d.booted)
                .unwrap_or(first);
            let q = Open {
                project,
                chat,
                platform: d.platform.into(),
                boot: Some(true),
                target: Target {
                    host_id: Some(d.host_id.clone()),
                    device_id: d.id.clone(),
                },
            };
            drop(live);
            let session = open_checked(app, q, Some(scope.clone())).await?;
            scope.verify(app, true)?;
            let tool = session
                .agent_config
                .as_ref()
                .ok_or_else(|| Error::Refused("device agent lease unavailable".into()))?;
            Ok(
                json!({"device":{"id":session.device_id,"platform":session.platform,"host_id":session.host_id},"view_id":session.view_id,"agent_device":{"command":"kar","args":["device-agent","--config",tool]}}),
            )
        }
        "kar_device_screenshot" => {
            let q: TargetArgs = serde_json::from_value(input)?;
            host(&scope, q.host_id.as_deref())?;
            let mut live = svc.live.lock().await;
            enabled(&live)?;
            let entries = select(
                &scope,
                &live,
                chat,
                q.host_id.as_deref(),
                q.device_id.as_deref(),
            );
            let session = entries
                .last()
                .ok_or_else(|| Error::Refused("no matching device is open in this chat".into()))?;
            if q.device_id.is_some() && entries.len() != 1 {
                return Err(Error::Refused("ambiguous device; specify host_id".into()));
            }
            let key = (session.host_id.clone(), session.device_id.clone());
            let device = svc.resolve(&mut live, &key.0, &key.1).await?;
            scope.verify(app, false)?;
            let png = svc.capture(&device).await?;
            if png.len() < 24 || png[..8] != *b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
                return Err(Error::Protocol("device supplied invalid PNG".into()));
            }
            let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
            scope.verify(app, false)?;
            Ok(
                json!({"device":device,"screenshot":{"mimeType":"image/png","data":STANDARD.encode(png),"width":width,"height":height}}),
            )
        }
        "kar_device_close" => {
            let q: CloseArgs = serde_json::from_value(input)?;
            host(&scope, q.host_id.as_deref())?;
            let _operation = svc.lifecycle.lock().await;
            let mut live = svc.live.lock().await;
            enabled(&live)?;
            let selected = select(
                &scope,
                &live,
                chat,
                q.host_id.as_deref(),
                q.device_id.as_deref(),
            );
            if q.device_id.is_some() && selected.len() > 1 {
                return Err(Error::Refused("ambiguous device; specify host_id".into()));
            }
            let keys: Vec<_> = selected
                .iter()
                .map(|v| (v.host_id.clone(), v.device_id.clone()))
                .collect();
            if q.shutdown == Some(true)
                && live.sessions.iter().any(|v| {
                    v.chat != chat
                        && keys
                            .iter()
                            .any(|(h, id)| h == &v.host_id && id == &v.device_id)
                })
            {
                return Err(Error::Refused("another chat still uses this device".into()));
            }
            for (host, id) in &keys {
                scope.verify(app, true)?;
                if q.shutdown == Some(true) {
                    let d = svc.resolve(&mut live, host, id).await?;
                    scope.verify(app, true)?;
                    svc.shutdown_device(&d).await?;
                    live.devices
                        .iter_mut()
                        .filter(|d| d.id == *id && d.host_id == *host)
                        .for_each(|d| d.booted = false);
                }
                revoke_matching(&mut live, |s| {
                    s.chat == chat && s.device_id == *id && s.host_id == *host
                });
                svc.publish(&mut live);
            }
            Ok(json!({"closed":keys.len()}))
        }
        _ => Err(Error::Unsupported("unknown device operation".into())),
    }
}
