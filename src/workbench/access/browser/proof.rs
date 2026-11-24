use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
#[derive(Deserialize)]
struct Header {
    typ: String,
    alg: String,
    jwk: Jwk,
    crit: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
    d: Option<String>,
}
#[derive(Deserialize)]
struct Claims {
    jti: String,
    iat: i64,
    htu: String,
    htm: String,
    ath: Option<String>,
}
pub(super) struct Proof {
    pub(super) thumb: String,
    replay: String,
    until: u64,
}
impl Proof {
    pub(super) fn consume(&self, db: &mut Registry) -> AccessResult<()> {
        let p = &mut db.environment.proofs;
        p.retain(|_, t| *t > now());
        if p.len() >= 65536 || p.insert(self.replay.clone(), self.until).is_some() {
            return Err(AccessError::Invalid);
        }
        Ok(())
    }
}
fn parse<T: serde::de::DeserializeOwned>(s: &str) -> AccessResult<T> {
    let bytes = B64.decode(s).map_err(|_| AccessError::Invalid)?;
    serde_json::from_slice(&bytes).map_err(|_| AccessError::Invalid)
}
fn verify_raw(
    auth: &Auth,
    jwt: &str,
    method: &Method,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
) -> AccessResult<Proof> {
    if jwt.len() > 4096 {
        return Err(AccessError::Invalid);
    }
    let (signed, sig) = jwt.rsplit_once('.').ok_or(AccessError::Invalid)?;
    let (head, claims) = signed.split_once('.').ok_or(AccessError::Invalid)?;
    let header: Header = parse(head)?;
    let c: Claims = parse(claims)?;
    let j = header.jwk;
    if header.alg != "ES256"
        || !header.typ.eq_ignore_ascii_case("dpop+jwt")
        || header.crit.is_some_and(|v| !v.is_empty())
        || j.crv != "P-256"
        || j.kty != "EC"
        || j.d.is_some()
    {
        return Err(AccessError::Invalid);
    }
    let x = B64.decode(&j.x).map_err(|_| AccessError::Invalid)?;
    let y = B64.decode(&j.y).map_err(|_| AccessError::Invalid)?;
    if x.len() != 32 || y.len() != 32 {
        return Err(AccessError::Invalid);
    }
    let mut public = vec![4];
    public.extend(x);
    public.extend(y);
    let signature = B64.decode(sig).map_err(|_| AccessError::Invalid)?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public)
        .verify(signed.as_bytes(), &signature)
        .map_err(|_| AccessError::Invalid)?;
    let canonical = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        j.x, j.y
    );
    let thumb = B64.encode(Sha256::digest(canonical));
    let origin = crate::workbench::access::oauth::origin(&auth.app).ok_or(AccessError::Invalid)?;
    if !c.htm.eq_ignore_ascii_case(method.as_str())
        || c.htu != format!("{origin}{path}")
        || key.is_some_and(|s| s != thumb)
        || (now() / 1000).abs_diff(c.iat.try_into().map_err(|_| AccessError::Invalid)?) > 300
        || c.jti.is_empty()
        || c.jti.len() > 256
        || token.is_some_and(|s| c.ath.as_ref() != Some(&B64.encode(Sha256::digest(s))))
    {
        return Err(AccessError::Invalid);
    }
    Ok(Proof {
        replay: digest(&format!("{thumb}:{}", c.jti)),
        thumb,
        until: ((c.iat + 301) as u64) * 1000,
    })
}
pub(super) fn verify(
    auth: &Auth,
    h: &HeaderMap,
    method: &Method,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
) -> AccessResult<Proof> {
    let mut headers = h.get_all("dpop").iter();
    let proof = headers
        .next()
        .and_then(|s| s.to_str().ok())
        .ok_or(AccessError::Invalid)?;
    if headers.next().is_some() {
        return Err(AccessError::Invalid);
    }
    verify_raw(auth, proof, method, path, token, key)
}
pub(super) fn ticket(auth: &Auth, req: &Request, ticket: &str, key: &str) -> AccessResult<Proof> {
    let query = url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "dpop")
        .map(|(_, v)| v)
        .collect::<Vec<_>>();
    if req.headers().contains_key("dpop") && query.is_empty() {
        return verify(
            auth,
            req.headers(),
            req.method(),
            req.uri().path(),
            Some(ticket),
            Some(key),
        );
    }
    if query.len() != 1 || req.headers().contains_key("dpop") {
        return Err(AccessError::Invalid);
    }
    verify_raw(
        auth,
        &query[0],
        req.method(),
        req.uri().path(),
        Some(ticket),
        Some(key),
    )
}
