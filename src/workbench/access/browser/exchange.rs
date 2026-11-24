use super::*;
const GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS: &str = "urn:ietf:params:oauth:token-type:access_token";
const BOOT: &str = "urn:kar:params:oauth:token-type:environment-bootstrap";

pub(super) async fn token(State(auth): State<Auth>, h: HeaderMap, body: Bytes) -> Response {
    match exchange(&auth, h, &body) {
        Ok(result) => Json(result).into_response(),
        Err(AccessError::Invalid) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "DPoP algs=\"ES256\"")],
            Json(json!({"error":"invalid_grant"})),
        )
            .into_response(),
        Err(AccessError::Denied) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_scope"})),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}
fn exchange(auth: &Auth, h: HeaderMap, body: &[u8]) -> AccessResult<Value> {
    let p =
        super::super::oauth::form(&h, body).map_err(|_| bad("Expected unique form parameters"))?;
    let get = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
    if get("grant_type") != GRANT || get("requested_token_type") != ACCESS {
        return Err(bad("Unsupported grant or requested token type"));
    }
    let raw = get("subject_token");
    let proof = if h.contains_key("dpop") {
        Some(proof::verify(
            auth,
            &h,
            &Method::POST,
            "/oauth/token",
            None,
            None,
        )?)
    } else {
        None
    };
    let (d, token) = match get("subject_token_type") {
        BOOT => {
            let requested = p.get("scope").map(|s| {
                s.split_whitespace()
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>()
            });
            if requested
                .as_ref()
                .is_some_and(|r| r.is_empty() || !r.is_subset(&all()))
            {
                return Err(AccessError::Denied);
            }
            auth.app.0.access.update(|db| {
                let mut grant = consume(db, raw)?;
                if let Some(label) = p.get("client_label") {
                    if label.is_empty() || label.len() > 120 || label.chars().any(char::is_control)
                    {
                        return Err(bad("Invalid client label"));
                    }
                    grant.label = label.clone();
                }
                if let Some(p) = &proof {
                    p.consume(db)?
                }
                let method = if proof.is_some() {
                    "dpop-access-token"
                } else {
                    "bearer-access-token"
                };
                session(db, grant, method, requested, proof.map(|p| p.thumb))
            })?
        }
        ACCESS => {
            if p.contains_key("scope") {
                return Err(AccessError::Denied);
            }
            let proof = proof.ok_or(AccessError::Invalid)?;
            let (id, hash) = split_token(raw)?;
            auth.app.0.access.update(|db| {
                proof.consume(db)?;
                let d = db.devices.get_mut(&id).ok_or(AccessError::Invalid)?;
                let a = d.account.as_mut().ok_or(AccessError::Invalid)?;
                if a.method != "dpop-access-token"
                    || a.until <= now()
                    || d.digest != hash
                    || a.proof.as_ref() != Some(&proof.thumb)
                {
                    return Err(AccessError::Invalid);
                }
                let secret = credential()?;
                d.digest = digest(&secret);
                a.until = now() + TTL;
                Ok((d.clone(), format!("{id}.{secret}")))
            })?
        }
        _ => return Err(bad("Unsupported subject token type")),
    };
    let a = d.account.unwrap();
    Ok(
        json!({"access_token":token,"issued_token_type":ACCESS,"token_type":if a.proof.is_some(){"DPoP"}else{"Bearer"},"expires_in":TTL/1000,"scope":a.scopes.into_iter().collect::<Vec<_>>().join(" ")}),
    )
}
