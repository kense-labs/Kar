//! OpenID discovery, verification and token exchange for the dynamic agent registration.
use super::*;
use reqwest::{Client, RequestBuilder};
use ring::signature;
use serde::de::DeserializeOwned;
pub(super) const SCOPE: &str = "chatgpt.tokens.use.direct";
pub(super) const RESOURCE: &str = "https://api.openai.com/v1";
const ISSUER: &str = "https://auth.openai.com";
#[derive(Deserialize)]
pub(super) struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: String,
    pub earliest_refresh_at: Option<Value>,
}
impl Tokens {
    pub fn expires(&self) -> Result<u64> {
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || self.access_token.is_empty()
            || self.access_token.chars().any(char::is_whitespace)
            || self.expires_in == 0
            || !self.scope.split_ascii_whitespace().any(|s| s == SCOPE)
        {
            return Err(bad("ChatGPT did not grant direct token sharing"));
        }
        now()
            .checked_add(self.expires_in)
            .ok_or_else(|| bad("invalid ChatGPT token expiry"))
    }
    pub fn earliest(&self) -> Option<u64> {
        let at = self.earliest_refresh_at.as_ref()?;
        at.as_u64().or_else(|| {
            at.as_str()?
                .parse::<jiff::Timestamp>()
                .ok()?
                .as_second()
                .try_into()
                .ok()
        })
    }
}
pub(super) struct Issuer {
    client: Client,
    pub doc: Discovery,
}
fn transport() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| bad("could not initialize ChatGPT transport"))
}
async fn read<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    let mut res = request
        .send()
        .await
        .map_err(|_| bad("ChatGPT service unavailable"))?;
    if !res.status().is_success() {
        return Err(bad("ChatGPT declined the request"));
    }
    let mut body = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|_| bad("incomplete ChatGPT response"))?
    {
        if body.len() + chunk.len() > 256 * 1024 {
            return Err(bad("oversized ChatGPT response"));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| bad("invalid ChatGPT service response"))
}
impl Issuer {
    pub async fn discover() -> Result<Self> {
        let base = match std::env::var("KAR_CHATGPT_LOOPBACK_ISSUER") {
            Ok(s) => {
                let u = url::Url::parse(&s).map_err(|_| bad("invalid loopback test issuer"))?;
                if !matches!(u.host_str(), Some("127.0.0.1" | "[::1]"))
                    || u.scheme() != "http"
                    || u.port().is_none()
                    || u.origin().ascii_serialization() != s
                {
                    return Err(bad(
                        "fixture issuer must be an explicit HTTP loopback origin",
                    ));
                }
                s
            }
            Err(_) => ISSUER.into(),
        };
        let client = transport()?;
        let doc: Discovery =
            read(client.get(format!("{base}/.well-known/openid-configuration"))).await?;
        if doc.issuer != base
            || [
                &doc.authorization_endpoint,
                &doc.token_endpoint,
                &doc.jwks_uri,
            ]
            .into_iter()
            .any(|v| {
                url::Url::parse(v).map_or(true, |u| {
                    u.origin().ascii_serialization() != base
                        || !u.username().is_empty()
                        || u.password().is_some()
                        || u.fragment().is_some()
                })
            })
        {
            return Err(bad("untrusted ChatGPT discovery document"));
        }
        if let Some(v) = &doc.revocation_endpoint
            && url::Url::parse(v).map_or(true, |u| {
                u.origin().ascii_serialization() != base
                    || !u.username().is_empty()
                    || u.password().is_some()
                    || u.fragment().is_some()
            })
        {
            return Err(bad("untrusted ChatGPT revocation endpoint"));
        }
        Ok(Self { client, doc })
    }
    pub async fn revoke(&self, client: &str, refresh: &str) -> bool {
        let Some(endpoint) = &self.doc.revocation_endpoint else {
            return false;
        };
        self.client
            .post(endpoint)
            .form(&[
                ("token", refresh),
                ("client_id", client),
                ("token_type_hint", "refresh_token"),
            ])
            .send()
            .await
            .is_ok_and(|r| r.status() == reqwest::StatusCode::OK)
    }
    pub async fn exchange(&self, params: &[(&str, &str)]) -> Result<Tokens> {
        read(self.client.post(&self.doc.token_endpoint).form(params)).await
    }
    pub async fn identity(
        &self,
        token: &str,
        client: &str,
        nonce: Option<&str>,
        access: &str,
    ) -> Result<Identity> {
        let keys: Value = read(self.client.get(&self.doc.jwks_uri)).await?;
        verify(&keys, &self.doc.issuer, token, client, nonce, access)
    }
}
#[derive(Deserialize)]
pub(super) struct Identity {
    pub sub: String,
    pub email: Option<String>,
}
fn invalid() -> Error {
    bad("ChatGPT identity could not be verified")
}
fn verify(
    keys: &Value,
    issuer: &str,
    token: &str,
    client: &str,
    nonce: Option<&str>,
    access: &str,
) -> Result<Identity> {
    if token.len() > 32768 || !valid_client(client) {
        return Err(invalid());
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(invalid());
    }
    let decode = |s: &str| B64.decode(s).map_err(|_| invalid());
    let h: Value = serde_json::from_slice(&decode(parts[0])?).map_err(|_| invalid())?;
    let alg = h["alg"].as_str().ok_or_else(invalid)?;
    let kid = h["kid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)?;
    if h.get("crit").is_some() || h.get("b64").is_some() {
        return Err(invalid());
    }
    let mut matching = keys["keys"]
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .filter(|k| {
            k["kid"] == kid
                && k.get("alg").is_none_or(|v| v == alg)
                && k.get("use").is_none_or(|v| v == "sig")
                && k.get("key_ops").is_none_or(|ops| {
                    ops.as_array()
                        .is_some_and(|a| a.iter().any(|o| o == "verify"))
                })
        });
    let k = matching.next().ok_or_else(invalid)?;
    if matching.next().is_some() {
        return Err(invalid());
    }
    let signed = &token.as_bytes()[..parts[0].len() + parts[1].len() + 1];
    let sig = decode(parts[2])?;
    let field = |name: &str| decode(k[name].as_str().ok_or_else(invalid)?);
    match alg {
        "RS256" if k["kty"] == "RSA" => signature::RsaPublicKeyComponents {
            n: field("n")?,
            e: field("e")?,
        }
        .verify(&signature::RSA_PKCS1_2048_8192_SHA256, signed, &sig)
        .map_err(|_| invalid())?,
        "ES256" if k["kty"] == "EC" && k["crv"] == "P-256" => {
            let x = field("x")?;
            let y = field("y")?;
            if x.len() != 32 || y.len() != 32 {
                return Err(invalid());
            }
            let mut pk = vec![4];
            pk.extend(x);
            pk.extend(y);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, pk)
                .verify(signed, &sig)
                .map_err(|_| invalid())?;
        }
        _ => return Err(invalid()),
    }
    let c: Value = serde_json::from_slice(&decode(parts[1])?).map_err(|_| invalid())?;
    let aud = &c["aud"];
    let audience = aud == client
        || aud.as_array().is_some_and(|a| {
            !a.is_empty()
                && a.iter().all(Value::is_string)
                && a.iter().any(|v| v == client)
                && (a.len() == 1 || c["azp"] == client)
        });
    let now = now();
    if c["iss"] != issuer
        || !audience
        || c.get("azp").is_some_and(|v| v != client)
        || c["exp"].as_u64().is_none_or(|v| v.saturating_add(5) <= now)
        || ["nbf", "iat"].iter().any(|k| {
            c.get(k)
                .is_some_and(|v| v.as_u64().is_none_or(|n| n > now + 5))
        })
        || nonce.is_some_and(|n| c["nonce"] != n)
        || c.get("at_hash")
            .is_some_and(|v| v != &B64.encode(&Sha256::digest(access.as_bytes())[..16]))
    {
        return Err(invalid());
    }
    let identity: Identity = serde_json::from_value(c).map_err(|_| invalid())?;
    if identity.sub.is_empty() {
        return Err(invalid());
    }
    Ok(identity)
}
