use super::*;
const BASE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const HUMAN: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
pub(super) fn verification(thumb: &str) -> String {
    let mut out = String::new();
    for (i, b) in thumb.bytes().take(8).enumerate() {
        if i == 4 {
            out.push('-')
        }
        out.push(HUMAN[BASE.iter().position(|c| *c == b).unwrap_or(0) % HUMAN.len()] as char)
    }
    out
}
async fn list(State(auth): State<Auth>, h: HeaderMap, uri: Uri) -> AccessResult<Json<Value>> {
    permits(&auth, &h, &Method::GET, &uri, "access:read")?;
    let db = auth.app.0.access.registry.lock().unwrap();
    let s = &db.environment.cloud;
    Ok(Json(
        json!({"pending":s.pending.iter().filter(|(_,v)|v.until>now()).map(|(k,v)|json!({"pairingLinkId":v.id,"proofKeyThumbprint":k,"verificationCode":verification(k),"expiresAt":date(v.until)})).collect::<Vec<_>>(),"trusted":s.trusted.iter().map(|k|json!({"proofKeyThumbprint":k,"verificationCode":verification(k)})).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Approve {
    pairing_link_id: Uuid,
}
async fn approve(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
    Json(p): Json<Approve>,
) -> AccessResult<Json<Value>> {
    permits(&auth, &h, &Method::POST, &uri, "access:write")?;
    let approved = auth.app.0.access.update(|db| {
        let s = &mut db.environment.cloud;
        let k = s
            .pending
            .iter()
            .find(|(_, v)| v.id == p.pairing_link_id && v.until > now())
            .map(|(k, _)| k.clone());
        if let Some(k) = k {
            s.pending.remove(&k);
            s.trusted.insert(k);
            Ok(true)
        } else {
            Ok(false)
        }
    })?;
    Ok(Json(json!({"approved":approved})))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Revoke {
    proof_key_thumbprint: String,
}
async fn revoke(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
    Json(r): Json<Revoke>,
) -> AccessResult<Json<Value>> {
    permits(&auth, &h, &Method::POST, &uri, "access:write")?;
    let revoked = auth.app.0.access.update(|db| {
        let k = &r.proof_key_thumbprint;
        let s = &mut db.environment.cloud;
        let pending = s.pending.remove(k).is_some();
        let trusted = s.trusted.remove(k);
        let old = db.devices.len();
        db.devices.retain(|_, d| {
            !d.account
                .as_ref()
                .is_some_and(|a| a.cloud.is_some() && a.proof.as_ref() == Some(k))
        });
        db.environment
            .grants
            .retain(|_, g| g.cloud.as_ref().is_none_or(|b| b.thumb != *k));
        Ok(pending || trusted || db.devices.len() != old)
    })?;
    Ok(Json(json!({"revoked":revoked})))
}
pub(super) fn routes() -> Router<Auth> {
    Router::new()
        .route("/api/auth/connect-devices", get(list))
        .route("/api/auth/connect-devices/approve", post(approve))
        .route("/api/auth/connect-devices/revoke", post(revoke))
}
