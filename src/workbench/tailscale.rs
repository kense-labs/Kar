//! Pure-Rust macOS Tailscale endpoint discovery & direct LAN reachability filter.
//!
//! Discovers `tailscale` CLI on macOS (`/Applications/Tailscale.app/Contents/MacOS/Tailscale`
//! and `PATH`), parses `tailscale status --json` and `tailscale serve status --json`,
//! and filters out cloud VM metadata (`169.254.169.254`) fake-LAN private addresses.

use super::{Workbench, http::ApiError};
use crate::Error;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{net::IpAddr, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleSnapshot {
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub backend_state: Option<String>,
    pub online: bool,
    pub tailnet_name: Option<String>,
    pub magic_dns_name: Option<String>,
    pub tailscale_ips: Vec<String>,
    pub https_endpoints: Vec<String>,
}

pub fn find_tailscale_binary() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("KAR_TAILSCALE_BIN") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(p);
        }
    }
    for candidate in [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("tailscale"))
            .find(|p| p.is_file())
    })
}

pub fn parse_tailscale_status_json(
    status_raw: &Value,
    serve_raw: Option<&Value>,
) -> TailscaleSnapshot {
    let backend_state = status_raw["BackendState"].as_str().map(str::to_owned);
    let online = backend_state.as_deref() == Some("Running")
        && status_raw["Self"]["Online"].as_bool().unwrap_or(true);
    let magic_dns_name = status_raw["Self"]["DNSName"]
        .as_str()
        .map(|s| s.trim_end_matches('.').to_owned())
        .filter(|s| !s.is_empty());
    let tailnet_name = status_raw["CurrentTailnet"]["Name"]
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty());
    let tailscale_ips = status_raw["TailscaleIPs"]
        .as_array()
        .or_else(|| status_raw["Self"]["TailscaleIPs"].as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let mut https_endpoints = Vec::new();
    if let Some(serve) = serve_raw
        && let Some(web) = serve["Web"].as_object()
    {
        for host_port in web.keys() {
            let url = if host_port.ends_with(":443") {
                format!("https://{}", host_port.trim_end_matches(":443"))
            } else {
                format!("https://{host_port}")
            };
            if !https_endpoints.contains(&url) {
                https_endpoints.push(url);
            }
        }
    }

    TailscaleSnapshot {
        installed: true,
        binary_path: None,
        backend_state,
        online,
        tailnet_name,
        magic_dns_name,
        tailscale_ips,
        https_endpoints,
    }
}

/// Detect if this host appears to run inside a cloud VM by probing the link-local
/// instance metadata service `169.254.169.254:80` with a fast 150ms TCP timeout.
pub async fn is_cloud_vm() -> bool {
    if std::env::var("KAR_ASSUME_CLOUD_VM").as_deref() == Ok("1") {
        return true;
    }
    tokio::time::timeout(
        Duration::from_millis(150),
        tokio::net::TcpStream::connect("169.254.169.254:80"),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

pub fn is_usable_lan_ip(ip: IpAddr, cloud_vm: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_multicast() {
                return false;
            }
            // CGNAT 100.64.0.0/10 belongs to Tailscale/carrier NAT, not direct LAN.
            let octets = v4.octets();
            if octets[0] == 100 && (octets[1] & 0b1100_0000) == 64 {
                return false;
            }
            if cloud_vm && v4.is_private() {
                return false;
            }
            v4.is_private()
        }
        IpAddr::V6(_) => false,
    }
}

pub async fn inspect_tailscale() -> TailscaleSnapshot {
    let Some(bin) = find_tailscale_binary() else {
        return TailscaleSnapshot::default();
    };
    let run_json = |args: &[&str]| {
        let bin = bin.clone();
        let owned_args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        async move {
            let mut cmd = Command::new(bin);
            cmd.args(&owned_args)
                .env_remove("KAR_WORKBENCH_TOKEN")
                .stdin(Stdio::null())
                .kill_on_drop(true);
            let out = tokio::time::timeout(Duration::from_secs(4), cmd.output())
                .await
                .ok()?
                .ok()?;
            if !out.status.success() {
                return None;
            }
            serde_json::from_slice::<Value>(&out.stdout).ok()
        }
    };
    let Some(status) = run_json(&["status", "--json"]).await else {
        return TailscaleSnapshot {
            installed: true,
            binary_path: Some(bin),
            ..Default::default()
        };
    };
    let serve = run_json(&["serve", "status", "--json"]).await;
    let mut snap = parse_tailscale_status_json(&status, serve.as_ref());
    snap.binary_path = Some(bin);
    snap
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeReq {
    local_port: u16,
    https_port: Option<u16>,
}

async fn get_tailscale(State(_app): State<Workbench>) -> Json<Value> {
    let snap = inspect_tailscale().await;
    let cloud = is_cloud_vm().await;
    Json(json!({
        "tailscale": snap,
        "cloud_vm_detected": cloud,
    }))
}

async fn configure_serve(Json(req): Json<ServeReq>) -> std::result::Result<Json<Value>, ApiError> {
    if req.local_port == 0 {
        return Err(Error::Refused("local_port must be non-zero".into()).into());
    }
    let bin = find_tailscale_binary()
        .ok_or_else(|| Error::Unsupported("Tailscale CLI is not installed on this Mac".into()))?;
    let https_port = req.https_port.unwrap_or(443);
    let mut cmd = Command::new(&bin);
    cmd.arg("serve")
        .arg("--bg")
        .arg(format!("--https={https_port}"))
        .arg(format!("http://127.0.0.1:{}", req.local_port))
        .env_remove("KAR_WORKBENCH_TOKEN")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| Error::Timeout("configuring tailscale serve".into()))?
        .map_err(Error::from)?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(Error::Refused(if detail.is_empty() {
            "tailscale serve failed".into()
        } else {
            detail
        })
        .into());
    }
    let snap = inspect_tailscale().await;
    Ok(Json(json!({"ok": true, "tailscale": snap})))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/environments/tailscale", get(get_tailscale))
        .route("/environments/tailscale/serve", post(configure_serve))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tailscale_status_magic_dns_and_https_serve_endpoints() {
        let status = json!({
            "BackendState": "Running",
            "CurrentTailnet": {"Name": "leon-net.ts.net"},
            "Self": {
                "Online": true,
                "DNSName": "macbook-pro.leon-net.ts.net.",
                "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]
            }
        });
        let serve = json!({
            "Web": {
                "macbook-pro.leon-net.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:4777"}}},
                "macbook-pro.leon-net.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:4777"}}}
            }
        });
        let snap = parse_tailscale_status_json(&status, Some(&serve));
        assert!(snap.online);
        assert_eq!(
            snap.magic_dns_name.as_deref(),
            Some("macbook-pro.leon-net.ts.net")
        );
        assert_eq!(snap.tailscale_ips.len(), 2);
        assert!(
            snap.https_endpoints
                .contains(&"https://macbook-pro.leon-net.ts.net".to_owned())
        );
        assert!(
            snap.https_endpoints
                .contains(&"https://macbook-pro.leon-net.ts.net:8443".to_owned())
        );
    }

    #[test]
    fn filters_cgnat_and_cloud_vpc_private_ips_from_lan_endpoints() {
        let lan: IpAddr = "192.168.1.25".parse().unwrap();
        let cgnat: IpAddr = "100.100.50.1".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(is_usable_lan_ip(lan, false));
        assert!(!is_usable_lan_ip(lan, true));
        assert!(!is_usable_lan_ip(cgnat, false));
        assert!(!is_usable_lan_ip(loopback, false));
    }
}
