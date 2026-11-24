//! Private URL credentials and per-attempt receipts. The decision, receipt and run share one commit.
mod admission;
mod rules;
use super::{Data, Engine, Schedule, Workbench, Workflow, missing};
use crate::{
    Error, Result,
    workbench::http::{ApiError, blocking},
};
pub(super) use admission::Window;
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
pub(super) use rules::Rules;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2_mac::Sha256;
use std::{collections::BTreeMap, io};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Receipt {
    #[serde(flatten)]
    summary: Summary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    missing: Vec<String>,
    headers: BTreeMap<String, String>,
    query: String,
    body: String,
    body_truncated: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Summary {
    id: Uuid,
    workflow: Uuid,
    received_ms: i64,
    method: String,
    content_type: Option<String>,
    body_bytes: usize,
    outcome: String,
    signature_verified: bool,
    run_id: Option<Uuid>,
    error: Option<String>,
}
#[derive(Deserialize, Default)]
pub(super) struct Options {
    #[serde(default)]
    accept_any_outcome: bool,
}
impl Options {
    fn visible(&self, row: &Receipt) -> bool {
        self.accept_any_outcome
            || matches!(
                row.summary.outcome.as_str(),
                "accepted"
                    | "dispatch_failed"
                    | "rejected_signature"
                    | "disabled"
                    | "rate_limited"
                    | "expired"
            )
    }
}
type Reply = std::result::Result<Json<Value>, ApiError>;
fn workflow(d: &Data, project: Uuid, id: Uuid) -> Result<&Workflow> {
    d.workflows
        .get(&id)
        .filter(|w| w.project == project)
        .ok_or_else(missing)
}
pub(super) async fn rules_get(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Reply {
    blocking(move || {
        app.project(p)?;
        let db = app.0.workflows.db.lock().unwrap();
        workflow(&db, p, id)?;
        Ok(json!(
            db.webhook_rules.get(&id).cloned().unwrap_or_default()
        ))
    })
    .await
}
pub(super) async fn rules_put(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    Json(mut rules): Json<Rules>,
) -> Reply {
    rules.validate()?;
    rules.signature.header.make_ascii_lowercase();
    blocking(move || {
        app.project(p)?;
        app.0.workflows.update(|d| {
            if !matches!(workflow(d, p, id)?.definition.schedule, Schedule::Webhook) {
                return Err(Error::Refused("workflow does not accept webhooks".into()));
            }
            if (rules.signature.header != "x-kar-signature"
                || !matches!(rules.signature.encoding, rules::Encoding::Hex)
                || rules.signature.prefix != "sha256=")
                && !d.secrets.contains_key(&id)
            {
                return Err(Error::Refused(
                    "configure the webhook signing secret first".into(),
                ));
            }
            d.webhook_rules.insert(id, rules.clone());
            Ok(json!(rules))
        })
    })
    .await
}
pub(super) async fn rotate(
    State(app): State<Workbench>,
    Path((project, id)): Path<(Uuid, Uuid)>,
) -> Reply {
    blocking(move || {
        app.project(project)?;
        app.0.workflows.update(|d| {
            if !matches!(
                workflow(d, project, id)?.definition.schedule,
                Schedule::Webhook
            ) {
                return Err(Error::Refused(
                    "only webhook workflows have a URL token".into(),
                ));
            }
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            d.webhook_tokens.insert(id, token.clone());
            Ok(json!({"path":format!("/hooks/workflows/{project}/{id}/{token}")}))
        })
    })
    .await
}
pub(super) async fn list(
    State(app): State<Workbench>,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Query(options): Query<Options>,
) -> Reply {
    blocking(move || {
        app.project(project)?;
        let data = app.0.workflows.db.lock().unwrap();
        workflow(&data, project, id)?;
        let deliveries = data
            .deliveries
            .get(&id)
            .into_iter()
            .flatten()
            .rev()
            .filter(|r| options.visible(r))
            .map(|r| &r.summary)
            .collect::<Vec<_>>();
        Ok(json!({"deliveries":deliveries}))
    })
    .await
}
pub(super) async fn detail(
    State(app): State<Workbench>,
    Path((project, id, delivery)): Path<(Uuid, Uuid, Uuid)>,
    Query(options): Query<Options>,
) -> Reply {
    blocking(move || {
        app.project(project)?;
        let data = app.0.workflows.db.lock().unwrap();
        workflow(&data, project, id)?;
        let row = data
            .deliveries
            .get(&id)
            .into_iter()
            .flatten()
            .find(|r| r.summary.id == delivery && options.visible(r))
            .ok_or_else(missing)?;
        Ok(json!({"delivery":row}))
    })
    .await
}

fn redact(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "cookie" | "set-cookie")
        || name.starts_with("x-kar-relay-")
        || [
            "token",
            "secret",
            "signature",
            "key",
            "password",
            "auth",
            "credential",
            "hmac",
            "jwt",
            "assertion",
        ]
        .iter()
        .any(|part| name.contains(part))
}
fn redacted_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| {
            if let Some((key, _)) = pair.split_once('=') {
                let key_decoded = percent_encoding::percent_decode_str(key).decode_utf8_lossy();
                if redact(&key_decoded) {
                    return format!("{key}=[redacted]");
                }
            }
            pair.to_owned()
        })
        .collect::<Vec<_>>()
        .join("&")
}
struct Request {
    received_ms: i64,
    project: Uuid,
    workflow: Uuid,
    token: Option<String>,
    headers: HeaderMap,
    query: String,
    body: Bytes,
}
impl Request {
    fn receipt(
        &self,
        now: i64,
        verified: bool,
        outcome: &str,
        run_id: Option<Uuid>,
        error: Option<String>,
        rules: &Rules,
    ) -> Receipt {
        let n = self.body.len().min(64 * 1024);
        let bytes = &self.body[..n];
        let truncated = n < self.body.len();
        let bytes = match std::str::from_utf8(bytes) {
            Err(e) if truncated && e.error_len().is_none() => &bytes[..e.valid_up_to()],
            _ => bytes,
        };
        Receipt {
            missing: Vec::new(),
            summary: Summary {
                id: Uuid::new_v4(),
                workflow: self.workflow,
                received_ms: now,
                method: "POST".into(),
                content_type: self
                    .headers
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_owned),
                body_bytes: self.body.len(),
                outcome: outcome.into(),
                signature_verified: verified,
                run_id,
                error,
            },
            query: redacted_query(&self.query),
            headers: self
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        if redact(name.as_str())
                            || rules.signature.header.eq_ignore_ascii_case(name.as_str())
                        {
                            "[redacted]".into()
                        } else {
                            value.to_str().unwrap_or("").to_owned()
                        },
                    )
                })
                .collect(),
            body: String::from_utf8_lossy(bytes).into_owned(),
            body_truncated: truncated,
        }
    }
    fn signature_matches(&self, secret: &str, signature: &rules::Signature) -> bool {
        let Some(sig) = self
            .headers
            .get(&signature.header)
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
        else {
            return false;
        };
        let Some(prefix) = sig.get(..signature.prefix.len()) else {
            return false;
        };
        if !prefix.eq_ignore_ascii_case(&signature.prefix) {
            return false;
        }
        let data = &sig[prefix.len()..];
        let tag = match signature.encoding {
            rules::Encoding::Hex
                if data.len() == 64 && data.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                (0..32)
                    .map(|i| u8::from_str_radix(&data[2 * i..2 * i + 2], 16).unwrap())
                    .collect::<Vec<_>>()
            }
            rules::Encoding::Base64 => match base64::engine::general_purpose::STANDARD.decode(data)
            {
                Ok(v) => v,
                Err(_) => return false,
            },
            _ => return false,
        };
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("variable length key");
        mac.update(&self.body);
        mac.verify_slice(&tag).is_ok()
    }
    fn input(&self) -> Result<(String, Value)> {
        let value = serde_json::from_slice::<Value>(&self.body);
        if let Ok(v) = &value
            && let Some(key) = v.get("id").and_then(Value::as_str)
        {
            if key.trim().is_empty() || key.len() > 256 {
                return Err(Error::Refused("invalid delivery identifier".into()));
            }
            if self.token.is_none() || v.get("input").is_some() {
                return Ok((key.into(), v.get("input").cloned().unwrap_or_default()));
            }
        }
        if self.token.is_none() {
            return Err(Error::Refused(
                "expected a signed delivery with an id".into(),
            ));
        }
        Ok((
            Uuid::new_v4().to_string(),
            value.unwrap_or_else(|_| {
                Value::String(String::from_utf8_lossy(&self.body).into_owned())
            }),
        ))
    }
    fn deliver(&self, engine: &Engine) -> Result<Result<admission::Decision>> {
        engine.update_checked(
            |d| {
                let w = workflow(d, self.project, self.workflow)?;
                if !matches!(w.definition.schedule, Schedule::Webhook) {
                    return Err(missing());
                }
                if let Some(token) = &self.token
                    && !d
                        .webhook_tokens
                        .get(&self.workflow)
                        .is_some_and(|saved| bool::from(saved.as_bytes().ct_eq(token.as_bytes())))
                {
                    return Err(missing());
                }
                let secret = d.secrets.get(&self.workflow);
                let rules = d.webhook_rules.get(&w.id).cloned().unwrap_or_default();
                let verified = secret.is_some_and(|s| self.signature_matches(s, &rules.signature));
                // The tokenless path is for signed deliveries only. Refusals here have no
                // authenticated task identity and cannot evict its delivery history.
                if self.token.is_none() && !verified {
                    return Err(invalid_signature());
                }
                Ok((w.clone(), verified, secret.is_some(), rules))
            },
            |d, (w, verified, signed, rules)| {
                let now = engine.clock.now_ms();
                let (outcome, result) = if signed && !verified {
                    ("rejected_signature", Err(invalid_signature()))
                } else if !w.definition.enabled {
                    ("disabled", Err(Error::Refused("workflow disabled".into())))
                } else {
                    match self.admit(d, &w, &rules, now) {
                        Ok(decision) => (decision.outcome, Ok(decision)),
                        Err(e) => ("dispatch_failed", Err(e)),
                    }
                };
                let mut receipt = self.receipt(
                    self.received_ms,
                    verified,
                    outcome,
                    result.as_ref().ok().and_then(|r| r.run),
                    result
                        .as_ref()
                        .err()
                        .map(ToString::to_string)
                        .or_else(|| result.as_ref().ok().and_then(|r| r.error.clone())),
                    &rules,
                );
                if let Ok(r) = &result {
                    receipt.missing = r.missing.clone();
                }
                let rows = d.deliveries.entry(w.id).or_default();
                let bucket = |r: &Receipt| match r.summary.outcome.as_str() {
                    "duplicate" | "coalesced" | "rate_limited" | "queue_full" => 1,
                    "filtered" => 2,
                    _ => 0,
                };
                let kind = bucket(&receipt);
                if kind != 0
                    && rows
                        .iter()
                        .filter(|r| {
                            bucket(r) == kind && now.saturating_sub(r.summary.received_ms) < 60000
                        })
                        .count()
                        >= 10
                {
                    return Ok(result);
                }
                if rows.iter().filter(|r| bucket(r) == kind).count()
                    >= if kind == 0 { 50 } else { 20 }
                    && let Some(i) = rows.iter().position(|r| bucket(r) == kind)
                {
                    rows.remove(i);
                }
                rows.push(receipt);
                Ok(result)
            },
        )
    }
}
fn invalid_signature() -> Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "invalid webhook signature").into()
}
async fn deliver(app: Workbench, req: Request) -> Response {
    let result = blocking(move || {
        app.project(req.project)?;
        let decision = req.deliver(&app.0.workflows)??;
        if let Some(id) = decision.run {
            return Ok(json!({"run_id":id}));
        }
        Ok(json!({"outcome":decision.outcome}))
    })
    .await;
    match result {
        Ok(body)
            if matches!(
                body.0["outcome"].as_str(),
                Some("rate_limited" | "queue_full")
            ) =>
        {
            (StatusCode::TOO_MANY_REQUESTS, body).into_response()
        }
        Ok(body) if body.0["outcome"] == "expired" => (StatusCode::GONE, body).into_response(),
        r => r.into_response(),
    }
}
pub(super) async fn signed(
    State(app): State<Workbench>,
    Path((project, workflow)): Path<(Uuid, Uuid)>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    {
        let db = app.0.workflows.db.lock().unwrap();
        let header = db
            .webhook_rules
            .get(&workflow)
            .map_or("x-kar-signature", |r| r.signature.header.as_str());
        if !headers.contains_key(header) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let received_ms = app.0.workflows.clock.now_ms();
    deliver(
        app,
        Request {
            received_ms,
            project,
            workflow,
            token: None,
            headers,
            query: uri.query().unwrap_or_default().to_owned(),
            body,
        },
    )
    .await
}
pub(super) async fn tokenized(
    State(app): State<Workbench>,
    Path((project, workflow, token)): Path<(Uuid, Uuid, String)>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let received_ms = app.0.workflows.clock.now_ms();
    deliver(
        app,
        Request {
            received_ms,
            project,
            workflow,
            token: Some(token),
            headers,
            query: uri.query().unwrap_or_default().to_owned(),
            body,
        },
    )
    .await
}
