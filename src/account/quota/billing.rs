//! Private HTTP and profile reads shared by account quota adapters.
use super::*;
use reqwest::{Client, RequestBuilder, Url, header};
use std::path::PathBuf;

pub(super) fn env<'a>(probe: &'a Probe, name: &str) -> Option<&'a str> {
    probe
        .get(name)?
        .to_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(super) fn reauth(reason: ReauthReason) -> AccountUsageSnapshot {
    AccountUsageSnapshot::ReauthRequired {
        reason,
        last_account: None,
    }
}

pub(super) fn check_http_environment(probe: &Probe) -> Result<()> {
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_TLS_REJECT_UNAUTHORIZED",
    ] {
        if probe.get(key) != std::env::var_os(key).as_deref() {
            return Err(Error::Unsupported(format!(
                "Host HTTP settings differ for {key}"
            )));
        }
    }
    Ok(())
}

pub(super) fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| protocol("Could not initialize billing HTTP client"))
}

pub(super) fn validate_endpoint(value: &str) -> Result<()> {
    if Url::parse(value).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    }) {
        Ok(())
    } else {
        Err(protocol("Invalid billing endpoint URL"))
    }
}

pub(super) fn valid_header(value: &str) -> Result<&str> {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic()) {
        Ok(value)
    } else {
        Err(protocol(
            "Invalid billing credential or organization header",
        ))
    }
}

pub(super) fn get(
    client: &Client,
    base: &str,
    resource: &str,
    token: &str,
) -> Result<RequestBuilder> {
    Ok(client
        .get(format!("{base}/{resource}"))
        .header(header::ACCEPT, "application/json")
        .bearer_auth(valid_header(token)?))
}

pub(super) enum Reply {
    Data(Value),
    Rejected,
    Missing,
}
impl Reply {
    pub(super) fn project(
        self,
        f: impl FnOnce(Value) -> Result<AccountUsageSnapshot>,
    ) -> Result<AccountUsageSnapshot> {
        match self {
            Self::Data(data) => f(data),
            Self::Rejected => Ok(reauth(ReauthReason::CredentialsRejected)),
            Self::Missing => Ok(unsupported(UnsupportedReason::EndpointUnavailable)),
        }
    }
}

pub(super) async fn json_response(request: RequestBuilder) -> Result<Reply> {
    let mut response = request
        .send()
        .await
        .map_err(|_| protocol("Could not reach the account endpoint"))?;
    match response.status().as_u16() {
        401 | 403 => return Ok(Reply::Rejected),
        404 => return Ok(Reply::Missing),
        200..=299 => {}
        status => {
            return Err(protocol(&format!(
                "Account endpoint returned HTTP {status}"
            )));
        }
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| protocol("Could not read the account response"))?
    {
        if bytes.len().saturating_add(chunk.len()) > crate::process::OUTPUT_LIMIT as usize {
            return Err(protocol("Account response too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map(Reply::Data)
        .map_err(|_| protocol("Account endpoint returned invalid JSON"))
}

pub(super) async fn file(path: PathBuf) -> Result<Option<String>> {
    tokio::task::spawn_blocking(move || {
        super::super::profile::read_file(&path, crate::process::OUTPUT_LIMIT)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .ok_or_else(|| protocol("Could not read native account configuration"))
}
