//! The catalog control channel does not send any user input or query private account details.
use super::*;
use crate::{
    account::{AccountWire, AuthStatus, AuthStatusOptions},
    forms::ModelEntry,
    providers::ProviderErrorKind,
};
use uuid::Uuid;

async fn control(
    wire: &mut AccountWire,
    method: &str,
    mut request: Value,
    daemon: bool,
    session: &str,
) -> Result<Value> {
    let id = Uuid::new_v4().to_string();
    let daemon_scope = method.starts_with("daemon.") || daemon && method == "get_models";
    if daemon_scope {
        request["type"] = if method == "get_models" {
            "daemon.get_models"
        } else {
            method
        }
        .into();
    } else {
        request["subtype"] = method.into();
    }
    let mut packet = json!({"type":"control_request","request_id":id,"request":request});
    if daemon && !daemon_scope {
        packet["session_id"] = session.into();
    }
    wire.send(packet).await?;
    loop {
        let mut v = wire.next().await?;
        if v["type"] != "control_response" || v["response"]["request_id"] != id {
            continue;
        }
        if let Some(reported) = v["session_id"].as_str()
            && reported != session
        {
            return Err(Error::Protocol(
                "Qoder catalog response belongs to another session".into(),
            ));
        }
        if method == "get_models" {
            if v["response"]["subtype"] != "success" {
                return Err(Error::Refused(
                    "Qoder refused its live model catalog".into(),
                ));
            }
            return Ok(v["response"]["response"]["models"].take());
        }
        return super::private_reply(method, &v, Some(session));
    }
}

pub(crate) async fn read(rt: &Runtime, opts: &SessionOptions) -> Result<Vec<ModelEntry>> {
    let auth = rt
        .auth_status(&AuthStatusOptions {
            installation: opts.installation_options(),
            timeout: Some(opts.handshake_timeout),
        })
        .await?;
    if matches!(auth, AuthStatus::LoggedOut { .. }) {
        return Err(ProviderErrorKind::AuthenticationFailed.error("Qoder is signed out"));
    }
    let rt = rt.selected(opts).await?;
    let cwd = opts.cwd.canonicalize()?;
    let mut listing = SessionOptions::new(&cwd);
    listing.env = opts.env.clone();
    listing.unset_env = opts.unset_env.clone();
    listing.handshake_timeout = opts.handshake_timeout;
    let mut cmd = listing.command(&rt.program)?;
    cmd.current_dir(&cwd);
    let session = Uuid::new_v4().to_string();
    let mut files = vec![];
    let launch = super::prepare(&rt, &listing, &cwd, &session, &mut cmd, &mut files, None).await?;
    let mut wire = AccountWire::open(cmd)?;
    let result=async {
        let daemon=launch.daemon_args.is_some();
        if let Some(args)=launch.daemon_args {
            control(&mut wire,"daemon.initialize",json!({"daemon_protocol_version":2,"sdk_version":"kar","capabilities":["multi_session_v2","daemon_queries_v1"],"auth":super::auth(&listing)?}),true,&session).await?;
            control(&mut wire,"daemon.open_session",json!({"worker_args":args,"cwd":cwd}),true,&session).await?;
        }
        control(&mut wire,"initialize",json!({}),daemon,&session).await?;
        let rows=control(&mut wire,"get_models",json!({"fetchStrategy":"live"}),daemon,&session).await?;
        project(&rows,&launch.version)
    }.await;
    wire.finish().await;
    result
}

fn project(raw: &Value, version: &Version) -> Result<Vec<ModelEntry>> {
    let rows = raw
        .as_array()
        .ok_or_else(|| Error::Protocol("Qoder omitted its live model array".into()))?;
    let mut entries = Vec::new();
    for row in rows {
        let Some(id) = row["value"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let mut metadata = serde_json::Map::new();
        for key in ["displayName", "defaultEffort"] {
            if let Some(value) = row[key].as_str() {
                metadata.insert(key.into(), value.into());
            }
        }
        if let Some(name) = super::model_name(row, rows, version).filter(|s| *s != id) {
            metadata.insert("resolvedId".into(), name.into());
        }
        let efforts = row["efforts"].as_array().map(|values| {
            values
                .iter()
                .filter_map(|e| {
                    e.as_str().or_else(|| {
                        e["effort"]
                            .as_str()
                            .or_else(|| e["id"].as_str())
                            .filter(|s| !s.is_empty())
                    })
                })
                .map(str::to_owned)
                .collect()
        });
        entries.push(ModelEntry {
            id: id.into(),
            default: row["isDefault"] == true,
            effort_levels: efforts,
            disabled: (row["isEnabled"] == false)
                .then(|| json!({"reason":"Qoder reports this model disabled"})),
            metadata,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_preserves_selector_and_only_unambiguous_version_specific_name() {
        let rows = json!([{"value":"one","modelId":"one","displayName":"Name","efforts":["low",{"id":"high"}],"defaultEffort":"low","isDefault":true,"isEnabled":false,"private":"hidden","serverModel":{"display_name":"legacy"}},{"value":"two","modelId":"two","displayName":"Name"}]);
        let recent = project(&rows, &Version::new(1, 1, 82)).unwrap();
        assert_eq!(recent[0].id, "one");
        assert!(recent[0].default && recent[0].disabled.is_some());
        assert_eq!(recent[0].metadata["defaultEffort"], "low");
        assert_eq!(recent[0].effort_levels.as_ref().unwrap(), &["low", "high"]);
        assert!(!recent[0].metadata.contains_key("resolvedId"));
        assert!(!recent[0].metadata.contains_key("private"));
        assert_eq!(
            project(&rows, &Version::new(1, 1, 81)).unwrap()[0].metadata["resolvedId"],
            "legacy"
        );
        assert_eq!(
            project(&json!([rows[0]]), &Version::new(1, 1, 82)).unwrap()[0].metadata["resolvedId"],
            "Name"
        );
    }
}
