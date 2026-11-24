use super::*;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD as STD, URL_SAFE_NO_PAD as B64},
};
use ring::{
    rand::SystemRandom,
    signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey},
};
const SPKI: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
pub(super) fn normalize_key(raw: &str) -> AccessResult<String> {
    let s = raw
        .trim()
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|s| s.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or_else(|| bad("Relay mint key must be an Ed25519 SPKI PEM"))?;
    let der = STD
        .decode(s.split_whitespace().collect::<String>())
        .map_err(|_| bad("Invalid relay public key"))?;
    let bytes = der
        .strip_prefix(SPKI)
        .filter(|v| v.len() == 32)
        .ok_or_else(|| bad("Expected Ed25519 public key"))?;
    Ok(B64.encode(bytes))
}
fn signing(auth: &Auth) -> AccessResult<Ed25519KeyPair> {
    let saved = auth
        .app
        .0
        .access
        .registry
        .lock()
        .unwrap()
        .environment
        .cloud
        .signing
        .clone();
    let encoded = match saved {
        Some(v) => v,
        None => auth.app.0.access.update(|db| {
            if let Some(k) = &db.environment.cloud.signing {
                return Ok(k.clone());
            }
            let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| bad("Could not create environment signing key"))?;
            let key = B64.encode(doc.as_ref());
            db.environment.cloud.signing = Some(key.clone());
            Ok(key)
        })?,
    };
    let b = B64
        .decode(encoded)
        .map_err(|_| bad("Invalid stored environment key"))?;
    Ed25519KeyPair::from_pkcs8(&b).map_err(|_| bad("Invalid stored environment key"))
}
fn token(key: &Ed25519KeyPair, typ: &str, p: &Value) -> AccessResult<String> {
    let encode = |v: &Value| {
        serde_json::to_vec(v)
            .map(|b| B64.encode(b))
            .map_err(Error::from)
    };
    let data = format!(
        "{}.{}",
        encode(&json!({"typ":typ,"alg":"EdDSA"}))?,
        encode(p)?
    );
    Ok(format!(
        "{data}.{}",
        B64.encode(key.sign(data.as_bytes()).as_ref())
    ))
}
fn base(id: Uuid, issuer: &str) -> Value {
    let at = now() / 1000;
    json!({"iss":format!("kar-env:{id}"),"aud":issuer,"sub":id,"jti":Uuid::new_v4(),"iat":at,"exp":at+300,"environmentId":id})
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkProof {
    challenge: String,
    relay_issuer: String,
    endpoint: Endpoint,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Endpoint {
    provider_kind: String,
    http_base_url: String,
    ws_base_url: String,
}
async fn link_proof(
    State(auth): State<Auth>,
    h: HeaderMap,
    uri: Uri,
    Json(req): Json<LinkProof>,
) -> AccessResult<Json<String>> {
    permits(&auth, &h, &Method::POST, &uri, "relay:configure")?;
    let issuer = relay_url(&req.relay_issuer)?;
    let ep = &req.endpoint;
    let http = Url::parse(&ep.http_base_url).map_err(|_| bad("Malformed endpoint"))?;
    let mut ws = http.clone();
    ws.set_scheme("wss")
        .map_err(|_| bad("Malformed endpoint"))?;
    if !bounded(&req.challenge, 16384)
        || !matches!(ep.provider_kind.as_str(), "cloudflare_tunnel" | "manual")
        || http.scheme() != "https"
        || http.username() != ""
        || http.password().is_some()
        || http.fragment().is_some()
        || http.query().is_some()
        || http.path() != "/"
        || Url::parse(&ep.ws_base_url).ok() != Some(ws)
    {
        return Err(bad("Expected corresponding HTTPS and WSS endpoint origins"));
    }
    let id = environment_id(&auth)?;
    let k = signing(&auth)?;
    let mut p = base(id, &issuer);
    let bound = auth
        .app
        .0
        .mcp
        .origin()
        .and_then(|s| Url::parse(&s).ok())
        .ok_or_else(|| bad("Workbench requires a bound loopback address"))?;
    let mut der = SPKI.to_vec();
    der.extend(k.public_key().as_ref());
    p["origin"] =
        json!({"localHttpHost":bound.host_str(),"localHttpPort":bound.port_or_known_default()});
    p["endpoint"] = json!(ep);
    p["environmentPublicKey"] = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        STD.encode(der)
    )
    .into();
    p["descriptor"] = descriptor(id);
    p["challenge"] = req.challenge.into();
    p["scopes"] = if ep.provider_kind == "cloudflare_tunnel" {
        json!(["agent_activity_notifications", "managed_tunnels"])
    } else {
        json!(["agent_activity_notifications"])
    };
    Ok(Json(token(&k, "kar-env-link+jwt", &p)?))
}
#[derive(Deserialize)]
struct Envelope {
    proof: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    iss: String,
    aud: String,
    sub: String,
    jti: String,
    iat: u64,
    exp: u64,
    environment_id: Uuid,
    nonce: String,
    scope: Vec<String>,
    client_proof_key_thumbprint: Option<String>,
    cnf: Option<Confirmation>,
}
#[derive(Deserialize)]
struct Confirmation {
    jkt: String,
}
fn verify(l: &Link, id: Uuid, raw: &str, kind: &str) -> AccessResult<Payload> {
    if raw.len() > 16384 {
        return Err(AccessError::Invalid);
    }
    let (data, sig) = raw.rsplit_once('.').ok_or(AccessError::Invalid)?;
    let (h, p) = data.split_once('.').ok_or(AccessError::Invalid)?;
    let dec = |s: &str| B64.decode(s).map_err(|_| AccessError::Invalid);
    let head: Value = serde_json::from_slice(&dec(h)?).map_err(|_| AccessError::Invalid)?;
    if head["typ"] != format!("kar-cloud-{kind}+jwt")
        || head["alg"] != "EdDSA"
        || head.get("crit").is_some()
    {
        return Err(AccessError::Invalid);
    }
    UnparsedPublicKey::new(&ED25519, dec(&l.mint_key)?)
        .verify(data.as_bytes(), &dec(sig)?)
        .map_err(|_| AccessError::Invalid)?;
    let p: Payload = serde_json::from_slice(&dec(p)?).map_err(|_| AccessError::Invalid)?;
    let scope = if kind == "health" {
        "environment:status"
    } else {
        "environment:connect"
    };
    let time = now() / 1000;
    if p.iss != l.issuer
        || p.aud != format!("kar-env:{id}")
        || p.sub != l.user
        || p.environment_id != id
        || p.scope.as_slice() != [scope]
        || p.exp <= time
        || p.iat > time + 60
        || p.exp <= p.iat
        || p.exp - p.iat > 300
        || !bounded(&p.jti, 256)
        || !bounded(&p.nonce, 256)
    {
        return Err(AccessError::Invalid);
    }
    Ok(p)
}
async fn answer(
    State(auth): State<Auth>,
    uri: Uri,
    Json(e): Json<Envelope>,
) -> AccessResult<Json<Value>> {
    let kind = if uri.path().ends_with("/health") {
        "health"
    } else {
        "mint"
    };
    let l = link(&auth).map_err(|_| AccessError::Invalid)?;
    let id = environment_id(&auth)?;
    let p = verify(&l, id, &e.proof, kind)?;
    let thumb = if kind == "mint" {
        let thumb = p
            .client_proof_key_thumbprint
            .as_ref()
            .filter(|k| k.len() == 43 && B64.decode(k).is_ok_and(|v| v.len() == 32))
            .ok_or(AccessError::Invalid)?;
        if p.cnf.as_ref().map(|c| &c.jkt) != Some(thumb) {
            return Err(AccessError::Invalid);
        }
        Some(thumb.clone())
    } else {
        None
    };
    let key = signing(&auth)?;
    auth.app.0.access.update(|db|{
        let s=&mut db.environment.cloud;
        if s.link.as_ref().is_none_or(|v|v.id!=l.id){return Err(AccessError::Invalid)}
        s.used.retain(|_,t|*t>now());
        if s.used.len()>=65536{return Err(bad("Relay replay ledger is full"))}
        for (field,v) in [("jti",&p.jti),("nonce",&p.nonce)]{
            if s.used.insert(digest(&format!("{kind}:{field}:{v}")),p.exp*1000).is_some(){return Err(AccessError::Invalid)}
        }
        let mut reply=base(id,&l.issuer);reply["requestNonce"]=p.nonce.into();
        let mut out=if let Some(thumb)=thumb{
            let until=now()+300000;let raw=credential()?;
            if !s.trusted.contains(&thumb) && s.require_approval.unwrap_or(true){
                s.pending.retain(|_,v|v.until>now());
                let rec=s.pending.entry(thumb.clone()).or_insert_with(||Pending{id:Uuid::new_v4(),until});rec.until=until;
            }
            let grant=Grant{id:Uuid::new_v4(),cloud:Some(Binding{link:l.id,thumb:thumb.clone()}),until,scopes:l.scopes.clone(),label:"Kar Connect device".into(),access:None,peer:false,peer_tasks:Default::default()};
            db.environment.grants.retain(|_,g|g.until>now());db.environment.grants.insert(digest(&raw),grant);
            reply["credential"]=raw.clone().into();reply["clientProofKeyThumbprint"]=thumb.into();reply["exp"]=(until/1000).into();
            json!({"credential":raw,"expiresAt":date(until)})
        }else{
            reply["descriptor"]=descriptor(id);reply["status"]="online".into();reply["checkedAt"]=date(now()).into();
            json!({"environmentId":id,"descriptor":reply["descriptor"],"status":"online","checkedAt":reply["checkedAt"]})
        };
        out["proof"]=token(&key,&format!("kar-env-{kind}+jwt"),&reply)?.into();Ok(Json(out))
    })
}
pub(super) fn routes() -> Router<Auth> {
    Router::new()
        .route("/api/connect/link-proof", post(link_proof))
        .route("/api/kar-connect/health", post(answer))
        .route("/api/kar-connect/mint-credential", post(answer))
}
