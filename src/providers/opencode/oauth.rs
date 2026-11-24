use super::{
    registry::{self, Integration, NativeMethod},
    server::Server,
};
use crate::providers::ProviderErrorKind as Kind;
use crate::{
    Error, Result,
    account::LoginCancellation,
    providers::{Choice, Prompt, ProviderEvent, ProviderInteraction, interruptible, until},
};
use reqwest::{Method, Url};
use serde_json::{Value, json};
use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

struct Attempt {
    server: Arc<Server>,
    provider: String,
    id: String,
    active: bool,
    handle: tokio::runtime::Handle,
}
impl Attempt {
    fn parts(&self) -> [&str; 5] {
        ["integration", &self.provider, "connect", "oauth", &self.id]
    }
    async fn cancel(&mut self) {
        let _ = self
            .server
            .request(Method::DELETE, &self.parts(), None)
            .await;
        self.active = false;
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.active {
            let s = self.server.clone();
            let p = self.provider.clone();
            let id = self.id.clone();
            self.handle.spawn(async move {
                let _ = s
                    .request(
                        Method::DELETE,
                        &["integration", &p, "connect", "oauth", &id],
                        None,
                    )
                    .await;
            });
        }
    }
}
fn http_url(text: &str) -> Option<Url> {
    let u = Url::parse(text).ok()?;
    (matches!(u.scheme(), "http" | "https")
        && u.host().is_some()
        && u.username().is_empty()
        && u.password().is_none())
    .then_some(u)
}
fn returns_to_host(url: &Url) -> bool {
    url.query_pairs()
        .filter(|(k, _)| matches!(k.as_ref(), "redirect_uri" | "redirect"))
        .any(|(_, v)| {
            http_url(&v).is_some_and(|r| match r.host() {
                Some(url_host) => {
                    let h = url_host.to_string();
                    h.trim_end_matches('.').eq_ignore_ascii_case("localhost")
                        || h.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                }
                None => false,
            })
        })
}
pub(super) async fn login(
    server: Arc<Server>,
    integration: &Integration,
    host: &mut impl ProviderInteraction,
    refuse_loopback: bool,
    cancel: &LoginCancellation,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let body = until(
        deadline,
        interruptible(cancel, async {
            let methods: Vec<_> = integration
                .methods
                .iter()
                .filter_map(|m| match m {
                    NativeMethod::Oauth { id, label, form } => {
                        Some((id, label.as_ref().unwrap_or(id), form))
                    }
                    _ => None,
                })
                .collect();
            let method = match methods.as_slice() {
                [] => {
                    return Err(
                        Kind::MethodUnavailable.error("Provider does not offer OAuth sign-in")
                    );
                }
                [only] => *only,
                many => {
                    let choice = registry::ask(
                        host,
                        Prompt::Select {
                            message: format!("Sign in to {}", integration.display()),
                            options: many
                                .iter()
                                .map(|(id, label, _)| Choice {
                                    id: (*id).clone(),
                                    label: (*label).clone(),
                                    description: None,
                                })
                                .collect(),
                        },
                    )
                    .await?;
                    *many
                        .iter()
                        .find(|(id, _, _)| id.as_str() == choice)
                        .ok_or_else(|| {
                            Kind::MethodUnavailable.error("Unknown provider sign-in method")
                        })?
                }
            };
            let mut body = json!({"methodID":method.0});
            let answers = registry::answers(method.2, host).await?;
            if !answers.is_empty() {
                body["answer"] = Value::Object(answers);
            }
            Ok(body)
        }),
    )
    .await?;
    if cancel.is_cancelled() {
        return Err(crate::providers::cancelled());
    }
    let reply = until(
        deadline,
        server.request(
            Method::POST,
            &["integration", &integration.id, "connect", "oauth"],
            Some(body),
        ),
    )
    .await?;
    let data = &reply["data"];
    let id = data["attemptID"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("OpenCode omitted the sign-in attempt ID".into()))?;
    let mut attempt = Attempt {
        server,
        provider: integration.id.clone(),
        id: id.into(),
        active: true,
        handle: tokio::runtime::Handle::current(),
    };
    let flow = async {
        let url = data["url"]
            .as_str()
            .and_then(http_url)
            .ok_or_else(|| Error::Protocol("OpenCode returned an invalid sign-in URL".into()))?;
        let paste = data["mode"] == "code" || data["paste"] == true;
        if refuse_loopback && !paste && returns_to_host(&url) {
            return Err(Kind::LoopbackRequired
                .error("This sign-in requires a browser on the OpenCode host"));
        }
        let info = data["instructions"].as_str().unwrap_or_default();
        static DEVICE: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"(?i)\benter code:\s*(\S+)\s*$").unwrap());
        let event = if let Some(code) = DEVICE.captures(info).and_then(|c| c.get(1)) {
            let expires = data["time"]["created"]
                .as_u64()
                .zip(data["time"]["expires"].as_u64())
                .and_then(|(start, end)| end.checked_sub(start))
                .map(|ms| ms.saturating_add(500) / 1000);
            ProviderEvent::DeviceCode {
                interval_seconds: None,
                user_code: code.as_str().into(),
                verification_uri: url.into(),
                expires_in_seconds: expires,
            }
        } else {
            ProviderEvent::AuthUrl {
                url: url.into(),
                instructions: (!info.is_empty()).then(|| info.to_owned()),
            }
        };
        host.event(event).map_err(|_| {
            Kind::InteractionFailed.error("Host could not display the sign-in instructions")
        })?;
        if cancel.is_cancelled() {
            return Err(crate::providers::cancelled());
        }
        let parts = attempt.parts();
        let poll = async {
            loop {
                let v = attempt.server.request(Method::GET, &parts, None).await?;
                match v["data"]["status"].as_str() {
                    Some("complete") => return Ok(()),
                    Some("failed") => {
                        return Err(Kind::Rejected.error("OpenCode reports that sign-in failed"));
                    }
                    Some("expired") => {
                        return Err(Kind::Expired.error("OpenCode sign-in expired"));
                    }
                    Some(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                    None => return Err(Error::Protocol("Missing OpenCode sign-in status".into())),
                }
            }
        };
        if !paste {
            return poll.await;
        }
        let enter = async {
            let code = registry::ask(
                host,
                Prompt::ManualCode {
                    placeholder: None,
                    message: if info.is_empty() {
                        "Paste the authorization code".into()
                    } else {
                        info.into()
                    },
                },
            )
            .await?;
            let mut path = parts.to_vec();
            path.push("complete");
            attempt
                .server
                .request(Method::POST, &path, Some(json!({"code":code.trim()})))
                .await?;
            Ok::<(), Error>(())
        };
        tokio::pin!(poll);
        tokio::select! {biased; finished=&mut poll=>finished, pasted=enter=>{pasted?;poll.await}}
    };
    let result = until(deadline, interruptible(cancel, flow)).await;
    if result.is_ok() {
        attempt.active = false;
    } else {
        attempt.cancel().await;
    }
    result
}
