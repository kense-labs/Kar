use super::*;
use axum::response::{Html, Redirect};
#[derive(Serialize, Deserialize)]
struct Csrf {
    actor: String,
    request: String,
    until: u64,
}
struct Valid {
    q: RequestData,
    client: Client,
    issuer: String,
    key: String,
}
enum InputError {
    Page(Failure),
    Callback(String),
}
fn callback(q: &RequestData, issuer: &str, k: &str, value: &str) -> String {
    let mut url = Url::parse(&q.redirect_uri).expect("verified callback");
    url.query_pairs_mut()
        .append_pair(k, value)
        .append_pair("iss", issuer);
    if let Some(state) = &q.state {
        url.query_pairs_mut().append_pair("state", state);
    }
    url.into()
}
fn validate(auth: &Auth, q: RequestData) -> Result<Valid, InputError> {
    let key = signing(&auth.app).map_err(InputError::Page)?;
    let issuer = urls(&auth.app).map_err(InputError::Page)?;
    let client: Client = unpack(&key, "client-id", &q.client_id).ok_or(InputError::Page(
        Failure("invalid_client", "Unknown client ID"),
    ))?;
    if !client
        .redirects
        .iter()
        .any(|uri| matches_redirect(uri, &q.redirect_uri))
    {
        return Err(InputError::Page(Failure(
            "invalid_request",
            "Unregistered client callback",
        )));
    }
    let err = if q.response_type != "code" {
        Some("unsupported_response_type")
    } else if q.code_challenge_method != "S256"
        || q.code_challenge.len() != 43
        || !q
            .code_challenge
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        Some("invalid_request")
    } else if !matches_resource(q.resource.as_deref(), &issuer) {
        Some("invalid_target")
    } else if q.scope.as_ref().is_some_and(|s| {
        s.split_whitespace().any(|v| {
            !matches!(
                v,
                READ | OPERATE | "settings:write" | "source-control:write"
            )
        })
    }) {
        Some("invalid_scope")
    } else {
        None
    };
    if let Some(err) = err {
        return Err(InputError::Callback(callback(&q, &issuer, "error", err)));
    }
    Ok(Valid {
        q,
        client,
        issuer,
        key,
    })
}
fn actor(auth: &Auth, h: &HeaderMap) -> Option<String> {
    if auth.owner(bearer(h)) {
        return Some(digest(bearer(h)));
    }
    super::super::browser::approval_actor(auth, h)
}
fn csrf(v: &Valid, actor: &str) -> OResult<String> {
    signed(
        &v.key,
        "browser-consent",
        &Csrf {
            actor: actor.to_owned(),
            request: digest(&serde_json::to_string(&v.q).map_err(|_| internal())?),
            until: now() + 300_000,
        },
    )
}
fn permits(q: &RequestData, mode: &str) -> bool {
    MODES.contains(&mode)
        && q.scope.as_ref().is_none_or(|s| {
            scopes(mode)
                .iter()
                .all(|v| s.split_whitespace().any(|t| t == *v))
        })
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&#39;")
}
fn modes(v: &Valid) -> Vec<&'static str> {
    MODES.iter().copied().filter(|m| permits(&v.q, m)).collect()
}
fn info(v: &Valid, auth: &Auth, h: &HeaderMap) -> OResult<Value> {
    let mut body = json!({"clientName":v.client.name,"redirectHost":Url::parse(&v.q.redirect_uri).ok().map(|u|u.authority().to_owned()),"environmentHost":Url::parse(&v.issuer).ok().map(|u|u.authority().to_owned())});
    if let Some(a) = actor(auth, h) {
        body["csrfToken"] = csrf(v, &a)?.into();
        body["oneClickAccess"] = json!(
            modes(v)
                .into_iter()
                .filter(|m| super::super::browser::approval_mode(auth, h, m))
                .collect::<Vec<_>>()
        );
    }
    Ok(body)
}
async fn authorize(
    State(auth): State<Auth>,
    h: HeaderMap,
    Query(q): Query<RequestData>,
) -> OResult<Response> {
    let v = match validate(&auth, q) {
        Ok(v) => v,
        Err(InputError::Page(e)) => return Err(e),
        Err(InputError::Callback(url)) => return Ok(Redirect::to(&url).into_response()),
    };
    let i = info(&v, &auth, &h)?;
    let opts = modes(&v)
        .into_iter()
        .map(|m| format!("<option>{m}</option>"))
        .collect::<String>();
    let code = if i["csrfToken"].is_null() {
        r#"<label>One-time Kar pairing code <input type="password" name="code" autocomplete="off"></label>"#
    } else {
        ""
    };
    let payload = B64.encode(serde_json::to_vec(&v.q).map_err(|_| internal())?);
    Ok(Html(format!(r#"<!doctype html><meta charset="utf-8"><title>Authorize Kar access</title><h1>Authorize Kar access</h1><p>Client name supplied by the application: <strong>{}</strong></p><p>Kar environment: {}<br>Return to: {}</p><form method="post" action="/oauth/mcp/browser-decision"><input type="hidden" name="request" value="{}"><input type="hidden" name="csrf" value="{}"><label>Access level <select name="access">{}</select></label>{}<button name="action" value="allow">Allow</button> <button name="action" value="deny">Deny</button></form>"#,esc(&v.client.name),esc(&v.issuer),esc(&v.q.redirect_uri),payload,esc(i["csrfToken"].as_str().unwrap_or("")),opts,code)).into_response())
}
async fn details(
    State(auth): State<Auth>,
    h: HeaderMap,
    Json(q): Json<RequestData>,
) -> OResult<Json<Value>> {
    match validate(&auth, q) {
        Ok(v) => {
            current_origin(&h, &v.issuer)?;
            Ok(Json(info(&v, &auth, &h)?))
        }
        Err(InputError::Page(e)) => Err(e),
        Err(InputError::Callback(url)) => Ok(Json(json!({"redirectTo":url}))),
    }
}
#[derive(Deserialize)]
#[serde(tag = "_tag", rename_all = "kebab-case")]
enum Decision {
    Deny,
    PairingCode {
        access: String,
        code: String,
    },
    BrowserSession {
        access: String,
        #[serde(rename = "csrfToken")]
        csrf: String,
    },
}
#[derive(Deserialize)]
struct DecisionBody {
    authorization: RequestData,
    decision: Decision,
}
fn decide(auth: &Auth, h: HeaderMap, b: DecisionBody) -> OResult<String> {
    let v = match validate(auth, b.authorization) {
        Ok(v) => v,
        Err(InputError::Page(e)) => return Err(e),
        Err(InputError::Callback(url)) => return Ok(url),
    };
    current_origin(&h, &v.issuer)?;
    let (mode, grant) = match b.decision {
        Decision::Deny => return Ok(callback(&v.q, &v.issuer, "error", "access_denied")),
        Decision::PairingCode { code, access } => (access, Some(code)),
        Decision::BrowserSession { access, csrf } => {
            let c: Csrf = unpack(&v.key, "browser-consent", &csrf).ok_or_else(invalid)?;
            if !super::super::browser::approval_mode(auth, &h, &access)
                || c.until <= now()
                || actor(auth, &h) != Some(c.actor)
                || c.request != digest(&serde_json::to_string(&v.q).map_err(|_| internal())?)
            {
                return Err(invalid());
            }
            (access, None)
        }
    };
    if !permits(&v.q, &mode) {
        return Err(Failure(
            "invalid_scope",
            "Access level exceeds the requested scope",
        ));
    }
    let raw = credential().map_err(|_| internal())?;
    auth.app
        .0
        .access
        .update(|db| {
            if let Some(g) = grant {
                super::super::browser::consume_approval(db, &g, &mode, v.client.peer)?;
            }
            db.oauth.codes.retain(|_, v| v.until > now());
            if db.oauth.codes.len() >= 4096 {
                return Err(AccessError::Bad("Too many pending codes".into()));
            }
            db.oauth.codes.insert(
                digest(&raw),
                Code {
                    request: v.q.clone(),
                    issuer: v.issuer.clone(),
                    until: now() + 60_000,
                    mode,
                    name: v.client.name,
                },
            );
            Ok(())
        })
        .map_err(|_| Failure("access_denied", "Invalid or expired approval credential"))?;
    Ok(callback(&v.q, &v.issuer, "code", &raw))
}
async fn decision(
    State(auth): State<Auth>,
    h: HeaderMap,
    Json(b): Json<DecisionBody>,
) -> OResult<Json<Value>> {
    Ok(Json(json!({"redirectTo":decide(&auth,h,b)?})))
}
async fn browser_decision(
    State(auth): State<Auth>,
    h: HeaderMap,
    b: axum::body::Bytes,
) -> OResult<Redirect> {
    let p = form(&h, &b)?;
    let v = |k: &str| p.get(k).cloned().unwrap_or_default();
    let authorization = serde_json::from_slice(&B64.decode(v("request")).map_err(|_| invalid())?)
        .map_err(|_| invalid())?;
    let access = v("access");
    let decision = match v("action").as_str() {
        "deny" => Decision::Deny,
        "allow" if !v("code").is_empty() => Decision::PairingCode {
            access,
            code: v("code"),
        },
        "allow" => Decision::BrowserSession {
            access,
            csrf: v("csrf"),
        },
        _ => return Err(invalid()),
    };
    Ok(Redirect::to(&decide(
        &auth,
        h,
        DecisionBody {
            authorization,
            decision,
        },
    )?))
}
pub(super) fn routes() -> Router<Auth> {
    Router::new()
        .route("/oauth/mcp/authorize", get(authorize))
        .route("/oauth/mcp/approval", post(details))
        .route("/oauth/mcp/decision", post(decision))
        .route("/oauth/mcp/browser-decision", post(browser_decision))
}
