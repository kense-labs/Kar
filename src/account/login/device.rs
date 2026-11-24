use super::{
    LoginCancellation, LoginEvent, LoginFailure, LoginInteraction, LoginResult, Wire, emit, link,
};
use crate::{Error, Result};
use serde_json::{Value, json};

struct Completion {
    id: Option<String>,
    success: bool,
}
impl Completion {
    fn read(frame: &Value) -> Option<Self> {
        if frame["method"] != "account/login/completed" || frame.get("id").is_some() {
            return None;
        }
        let p = &frame["params"];
        let id = match p.get("loginId") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_str()?.to_owned()),
        };
        Some(Self {
            id,
            success: p["success"].as_bool()?,
        })
    }
    fn matches(&self, id: &str) -> bool {
        self.id.as_ref().is_none_or(|s| s == id)
    }
    fn accepted(self) -> std::result::Result<(), LoginResult> {
        if self.success {
            Ok(())
        } else {
            Err(LoginResult::failed(
                LoginFailure::Rejected,
                "Codex reported a failed device sign-in",
            ))
        }
    }
}

async fn refuse(wire: &mut Wire, frame: &Value) -> Result<()> {
    if frame["method"].is_string() && !frame["id"].is_null() {
        wire.send(json!({"id":frame["id"],"error":{"code":-32601,"message":"Not available during account management"}})).await?;
    }
    Ok(())
}
async fn reply(wire: &mut Wire, id: u64, mut early: Option<&mut Vec<Completion>>) -> Result<Value> {
    loop {
        let mut frame = wire.next().await?;
        if frame["method"].is_string() {
            if let Some(early) = early.as_deref_mut()
                && let Some(item) = Completion::read(&frame)
            {
                if early.len() >= 256 {
                    return Err(Error::Protocol(
                        "Too many early completion notifications".into(),
                    ));
                }
                early.push(item);
            }
            refuse(wire, &frame).await?;
        } else if frame["id"].as_u64() == Some(id) {
            if !frame["error"].is_null() {
                return Err(Error::Refused("Codex rejected the account request".into()));
            }
            return frame
                .get_mut("result")
                .map(Value::take)
                .ok_or_else(|| Error::Protocol("Missing Codex response".into()));
        }
    }
}
pub(super) async fn account_reply(wire: &mut Wire, id: u64) -> Result<Value> {
    reply(wire, id, None).await
}

pub(super) async fn codex(
    wire: &mut Wire,
    host: &mut impl LoginInteraction,
    cancel: &LoginCancellation,
) -> std::result::Result<(), LoginResult> {
    wire.send(json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"kar","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}})).await?;
    reply(wire, 1, None).await?;
    wire.send(json!({"method":"initialized","params":{}}))
        .await?;
    wire.send(json!({"id":2,"method":"account/login/start","params":{"type":"chatgptDeviceCode"}}))
        .await?;
    let mut early = vec![];
    let started = reply(wire, 2, Some(&mut early)).await?;
    let field = |k: &str| started[k].as_str().map(str::trim).filter(|s| !s.is_empty());
    let (Some(id), Some(code), Some(uri)) = (
        field("loginId"),
        field("userCode"),
        link(&started["verificationUrl"]),
    ) else {
        return Err(LoginResult::failed(
            LoginFailure::ProcessFailed,
            "Codex did not return the device-pairing details",
        ));
    };
    if let Some(done) = early.into_iter().find(|e| e.matches(id)) {
        return done.accepted();
    }
    emit(
        host,
        cancel,
        LoginEvent::DeviceCode {
            user_code: code.to_owned(),
            verification_uri: uri,
        },
    )?;
    emit(host,cancel,LoginEvent::Info {message:"Allow device-code sign-in in ChatGPT security settings. A workspace administrator can enable it for workspace accounts.".into()})?;
    loop {
        let frame = wire.next().await?;
        if let Some(done) = Completion::read(&frame)
            && done.matches(id)
        {
            return done.accepted();
        }
        refuse(wire, &frame).await?;
    }
}

pub(super) async fn factory(
    wire: &mut Wire,
    host: &mut impl LoginInteraction,
    cancel: &LoginCancellation,
) -> std::result::Result<(), LoginResult> {
    let mut caps = Wire::capabilities();
    caps["elicitation"] = json!({"url":{}});
    let init = wire.initialize(caps).await?;
    if !init["authMethods"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v["id"] == "device-pairing"))
    {
        return Err(LoginResult::failed(
            LoginFailure::ProcessFailed,
            "This Droid build does not offer device pairing",
        ));
    }
    wire.send(json!({"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"device-pairing"}})).await?;
    loop {
        let frame = wire.next().await?;
        if frame["jsonrpc"] != "2.0" {
            continue;
        }
        if frame["method"].is_string() && !frame["id"].is_null() {
            let p = &frame["params"];
            let reply = if frame["method"] == "elicitation/create"
                && p["mode"] == "url"
                && let Some(url) = link(&p["url"])
            {
                let shown = emit(
                    host,
                    cancel,
                    LoginEvent::AuthUrl {
                        url,
                        instructions: Some("Open the link to pair your Factory account.".into()),
                    },
                );
                let action = if shown.is_ok() { "accept" } else { "decline" };
                wire.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":{"action":action}}))
                    .await?;
                shown?;
                continue;
            } else {
                json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Only URL pairing is available"}})
            };
            wire.send(reply).await?;
        } else if frame["id"] == 2 {
            if !frame["error"].is_null() {
                return Err(LoginResult::failed(
                    LoginFailure::Rejected,
                    "Droid rejected device pairing",
                ));
            }
            if frame.get("result").is_none() {
                return Err(LoginResult::failed(
                    LoginFailure::ProcessFailed,
                    "Droid omitted the authentication response",
                ));
            }
            return Ok(());
        }
    }
}

pub(super) async fn codebuddy(
    wire: &mut Wire,
    host: &mut impl LoginInteraction,
    cancel: &LoginCancellation,
    reported_success: &mut bool,
) -> std::result::Result<(), LoginResult> {
    wire.control(json!({"subtype":"initialize"})).await?;
    let id = uuid::Uuid::new_v4().to_string();
    wire.send(
        json!({"type":"control_request","request_id":id,"request":{"subtype":"authenticate"}}),
    )
    .await?;
    let mut failure = None;
    loop {
        let f = match wire.next().await {
            Ok(frame) => frame,
            Err(e) => return Err(failure.unwrap_or_else(|| e.into())),
        };
        let request = &f["request"];
        if f["type"] == "control_request" {
            match request["subtype"].as_str() {
                Some("auth_url_callback") => {
                    if let Some(url) = link(&request["authState"]["authUrl"]) {
                        emit(
                            host,
                            cancel,
                            LoginEvent::AuthUrl {
                                url,
                                instructions: Some("Open this URL to sign in to CodeBuddy.".into()),
                            },
                        )?;
                    }
                }
                Some("auth_result_callback") if request["success"] == true => {
                    *reported_success = true;
                }
                Some("auth_result_callback")
                    if request["success"] == false && request["error"].is_object() =>
                {
                    failure = Some(buddy_failure(&request["error"]));
                }
                _ => {}
            }
        } else if f["type"] == "control_response" && f["response"]["request_id"] == id {
            let answer = &f["response"]["response"];
            return if f["response"]["subtype"] == "success" && answer["success"] == true {
                Ok(())
            } else {
                Err(failure.unwrap_or_else(|| buddy_failure(&answer["error"])))
            };
        }
    }
}
fn buddy_failure(v: &Value) -> LoginResult {
    match v["type"].as_str() {
        Some("cancelled") => LoginResult::Cancelled,
        Some("timeout") => LoginResult::timed_out(),
        _ => LoginResult::failed(
            LoginFailure::ProcessFailed,
            "CodeBuddy could not complete authentication",
        ),
    }
}
