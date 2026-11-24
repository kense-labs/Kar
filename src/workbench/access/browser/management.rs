use super::*;
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Pairing {
    label: Option<String>,
    scopes: Option<BTreeSet<String>>,
    access: Option<String>,
    purpose: Option<String>,
}
pub(super) fn authorize(
    auth: &Auth,
    h: &HeaderMap,
    method: &Method,
    path: &str,
    scope: &str,
) -> AccessResult<(Option<Uuid>, BTreeSet<String>)> {
    if auth.owner(bearer(h)) {
        return Ok((None, all()));
    }
    let d = credentials(auth, h, method, path)?;
    let a = d.account.ok_or(AccessError::Denied)?;
    if !a.scopes.contains(scope) {
        return Err(AccessError::Denied);
    }
    Ok((Some(d.id), a.scopes))
}
async fn issue(
    State(auth): State<Auth>,
    h: HeaderMap,
    Json(p): Json<Pairing>,
) -> AccessResult<Json<Value>> {
    let (_, available) = authorize(
        &auth,
        &h,
        &Method::POST,
        "/api/auth/pairing-token",
        "access:write",
    )?;
    let allowed = if let Some(access) = &p.access {
        if !oauth::MODES.contains(&access.as_str()) {
            return Err(bad("Unknown MCP access level"));
        }
        oauth::scopes(access)
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        available.clone()
    };
    let scopes = p.scopes.unwrap_or(allowed);
    if scopes.is_empty() || !scopes.is_subset(&available) || !scopes.is_subset(&all()) {
        return Err(AccessError::Denied);
    }
    if p.purpose.as_deref().is_some_and(|s| s != "peer-link")
        || p.purpose.is_some() && p.access.is_none()
    {
        return Err(bad("Invalid pairing purpose"));
    }
    let label = p.label.unwrap_or_else(|| "Kar device".into());
    if label.is_empty() || label.len() > 120 || label.chars().any(char::is_control) {
        return Err(bad("Invalid label"));
    }
    let id = Uuid::new_v4();
    let raw = credential()?;
    let until = now() + 300000;
    auth.app.0.access.update(|db| {
        db.environment.grants.retain(|_, g| g.until > now());
        db.environment.grants.insert(
            digest(&raw),
            Grant {
                id,
                cloud: None,
                until,
                scopes,
                label: label.clone(),
                access: p.access.clone(),
                peer: p.purpose.is_some(),
            },
        );
        Ok(())
    })?;
    Ok(Json(
        json!({"id":id,"credential":raw,"label":label,"expiresAt":date(until),"access":p.access,"purpose":p.purpose}),
    ))
}
#[derive(Deserialize)]
struct Revoke {
    #[serde(alias = "sessionId")]
    id: Uuid,
}
async fn revoke(
    State(auth): State<Auth>,
    uri: Uri,
    h: HeaderMap,
    Json(r): Json<Revoke>,
) -> AccessResult<Json<Value>> {
    authorize(&auth, &h, &Method::POST, uri.path(), "access:write")?;
    let revoked = auth.app.0.access.update(|db| {
        Ok(if uri.path().ends_with("pairing-links/revoke") {
            let old = db.environment.grants.len();
            db.environment.grants.retain(|_, g| g.id != r.id);
            old > db.environment.grants.len()
        } else {
            let old = oauth::clients(db).len();
            oauth::revoke_id(db, r.id);
            db.devices.remove(&r.id).is_some() || oauth::clients(db).len() < old
        })
    })?;
    Ok(Json(json!({"revoked":revoked})))
}
async fn others(State(auth): State<Auth>, h: HeaderMap) -> AccessResult<Json<Value>> {
    let (id, _) = authorize(
        &auth,
        &h,
        &Method::POST,
        "/api/auth/clients/revoke-others",
        "access:write",
    )?;
    let count = auth.app.0.access.update(|db| {
        super::connect::clear_other_clients(db, id);
        let old = db.devices.len();
        db.devices.retain(|k, _| Some(*k) == id);
        let n = old - db.devices.len();
        Ok(n + oauth::revoke_all(db))
    })?;
    Ok(Json(json!({"revokedCount":count})))
}
async fn list(State(auth): State<Auth>, h: HeaderMap) -> AccessResult<Json<Value>> {
    let (current, _) = authorize(&auth, &h, &Method::GET, "/api/auth/clients", "access:read")?;
    let db = auth.app.0.access.registry.lock().unwrap();
    let mut clients = oauth::clients(&db);
    clients.extend(db.devices.values().filter(|d|d.account.as_ref().is_none_or(|a|a.until>now())).map(|d|json!({"id":d.id,"label":d.label,"current":current==Some(d.id),"session":status(d)})));
    Ok(Json(json!({"clients":clients})))
}
pub(super) fn routes() -> Router<Auth> {
    Router::new()
        .route("/api/auth/pairing-token", post(issue))
        .route("/api/auth/pairing-links/revoke", post(revoke))
        .route("/api/auth/clients", get(list))
        .route("/api/auth/clients/revoke", post(revoke))
        .route("/api/auth/clients/revoke-others", post(others))
}
