//! Keep the handoff helper's tokens, PKCE verifier and callback listener ephemeral.
use super::*;
use axum::{
    http::Uri,
    response::{IntoResponse, Response},
};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    pub environment: Uuid,
    pub instance: String,
    pub offer: Uuid,
    pub expires: u64,
    pub public_key: String,
    pub nonce: String,
    pub client_id: Option<String>,
    pub subject: Option<String>,
    pub email: Option<String>,
}
impl Target {
    pub fn aad(&self) -> Vec<u8> {
        serde_json::to_vec(&(
            self.environment,
            &self.instance,
            self.offer,
            self.expires,
            &self.public_key,
            &self.nonce,
            &self.client_id,
            &self.subject,
        ))
        .expect("tuple serialization")
    }
    pub fn receiver(&self) -> Result<Vec<u8>> {
        let key = B64
            .decode(&self.public_key)
            .map_err(|_| bad("invalid transfer ticket"))?;
        if B64.decode(&self.nonce).map_or(true, |s| s.len() != 32)
            || key.len() != 32
            || self.environment.is_nil()
            || self.offer.is_nil()
            || self.expires <= now()
            || self.expires > now() + 605
            || self.instance.is_empty()
            || self.instance.len() > 128
            || self.client_id.as_ref().is_some_and(|c| !valid_client(c))
            || self
                .subject
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 512)
            || self.email.as_ref().is_some_and(|s| s.len() > 320)
            || self.subject.is_some() != self.client_id.is_some()
        {
            return Err(bad("invalid or expired transfer ticket"));
        }
        Ok(key)
    }
}
pub(super) type Owner = (Uuid, String, String);
pub(super) struct Offer {
    pub owner: Owner,
    pub target: Target,
    pub result: Option<(String, Value)>,
    pub secret: transfer::Receiver,
}
pub(super) struct Job {
    pub owner: Owner,
    pub expires: u64,
    pub status: watch::Receiver<Value>,
    pub callback: Callback,
    pub task: tokio::task::JoinHandle<()>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Default)]
pub(in crate::workbench) struct Jobs {
    pub(super) offers: Mutex<BTreeMap<Uuid, Offer>>,
    pub(super) jobs: Mutex<BTreeMap<Uuid, Job>>,
}
#[derive(Clone)]
pub(super) struct Callback {
    state: String,
    redirect: String,
    client: Option<String>,
    tx: Arc<Mutex<Option<oneshot::Sender<Code>>>>,
}
struct Code {
    code: String,
    client: String,
}
impl Callback {
    pub fn submit(&self, raw: &str) -> Result<()> {
        if raw.len() > 16384 {
            return Err(bad("invalid callback"));
        }
        let url = url::Url::parse(raw).map_err(|_| bad("invalid callback URL"))?;
        let mut base = url.clone();
        base.set_query(None);
        if base.as_str() != self.redirect {
            return Err(bad("callback does not match the active redirect URI"));
        }
        let mut params = BTreeMap::new();
        for (k, v) in url.query_pairs() {
            if params.insert(k.into_owned(), v.into_owned()).is_some() {
                return Err(bad("duplicate callback parameter"));
            }
        }
        if params.get("state") != Some(&self.state) {
            return Err(bad("callback state mismatch"));
        }
        let mut tx = self.tx.lock().unwrap();
        if params.contains_key("error") {
            tx.take().ok_or_else(|| bad("callback already consumed"))?;
            return Err(bad("ChatGPT sign-in was declined"));
        }
        let client = params
            .remove("client_id")
            .or_else(|| self.client.clone())
            .filter(|s| valid_client(s))
            .ok_or_else(|| bad("missing registered ChatGPT client"))?;
        if self.client.as_ref().is_some_and(|s| s != &client) {
            return Err(bad("callback client mismatch"));
        }
        let code = params
            .remove("code")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| bad("missing authorization code"))?;
        tx.take()
            .ok_or_else(|| bad("callback already consumed"))?
            .send(Code { code, client })
            .map_err(|_| bad("sign-in expired"))
    }
}
async fn receive(State(callback): State<Callback>, uri: Uri) -> Response {
    let mut raw = callback.redirect.clone();
    if let Some(q) = uri.query() {
        raw.push('?');
        raw.push_str(q);
    }
    let (code, msg) = match callback.submit(&raw) {
        Ok(()) => (200, "Return to Kar to finish sign-in."),
        Err(_) => (400, "Kar could not accept this sign-in response."),
    };
    (
        axum::http::StatusCode::from_u16(code).unwrap(),
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
            ("content-security-policy", "default-src 'none'"),
        ],
        msg,
    )
        .into_response()
}
pub(super) async fn start(target: Target, owner: Owner) -> Result<(Uuid, Job)> {
    let (public, key) = transfer::sender(&target.receiver()?, &target.aad())?;
    let issuer = oidc::Issuer::discover().await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/auth/callback",
        listener.local_addr()?.port()
    );
    let state = B64.encode(random::<32>()?);
    let nonce = target.nonce.clone();
    let verifier = zeroize::Zeroizing::new(B64.encode(random::<64>()?));
    let (sender, rx) = oneshot::channel();
    let callback = Callback {
        state: state.clone(),
        redirect: redirect.clone(),
        client: target.client_id.clone(),
        tx: Arc::new(Mutex::new(Some(sender))),
    };
    let mut url = url::Url::parse(&issuer.doc.authorization_endpoint)
        .map_err(|_| bad("invalid authorization endpoint"))?;
    {
        let mut q = url.query_pairs_mut();
        q.clear()
            .append_pair("response_type", "code")
            .append_pair(
                "client_id",
                target
                    .client_id
                    .as_deref()
                    .unwrap_or("dynamic_agent_client"),
            )
            .append_pair(
                "ext_agent_host_id",
                &format!("urn:uuid:{}", target.environment),
            )
            .append_pair("redirect_uri", &redirect)
            .append_pair(
                "scope",
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
            )
            .append_pair("resource", oidc::RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair(
                "code_challenge",
                &B64.encode(Sha256::digest(verifier.as_bytes())),
            );
        if target.client_id.is_none() {
            q.append_pair("agent_name_hint", "Kar");
        } else if let Some(email) = &target.email {
            q.append_pair("login_hint", email);
        }
    }
    let flow = Uuid::new_v4();
    let (tx, status) = watch::channel(
        json!({"phase":"auth","flow":flow,"authorization_url":url.as_str(),"redirect_uri":redirect}),
    );
    let expires = target.expires;
    let handler = Router::new()
        .route("/auth/callback", get(receive))
        .with_state(callback.clone());
    let task = tokio::spawn(async move {
        let work = async {
            let server = async { axum::serve(listener, handler).await };
            tokio::pin!(server);
            let code = tokio::select! {r=rx=>r.map_err(|_|bad("ChatGPT sign-in was declined"))?, _=&mut server=>return Err(bad("ChatGPT callback listener closed"))};
            tx.send_replace(json!({"phase":"verifying"}));
            let tokens = issuer
                .exchange(&[
                    ("grant_type", "authorization_code"),
                    ("resource", oidc::RESOURCE),
                    ("code", &code.code),
                    ("client_id", &code.client),
                    ("redirect_uri", &redirect),
                    ("code_verifier", &verifier),
                ])
                .await?;
            let expires = tokens.expires()?;
            let earliest = tokens.earliest();
            let id_token = tokens
                .id_token
                .ok_or_else(|| bad("missing signed ChatGPT identity"))?;
            let ident = issuer
                .identity(&id_token, &code.client, Some(&nonce), &tokens.access_token)
                .await?;
            if target.subject.as_ref().is_some_and(|s| s != &ident.sub) {
                return Err(bad(
                    "ChatGPT account differs from the requested registration",
                ));
            }
            let p = store::Profile {
                client: code.client,
                subject: ident.sub,
                email: ident.email,
                issuer: issuer.doc.issuer,
                scopes: tokens
                    .scope
                    .split_ascii_whitespace()
                    .map(str::to_owned)
                    .collect(),
                access: tokens.access_token,
                refresh: tokens.refresh_token,
                id_token,
                expires,
                earliest,
            };
            let sealed = store::encrypt(
                &key,
                &target.aad(),
                &zeroize::Zeroizing::new(serde_json::to_vec(&p)?),
            )?;
            let profile = transfer::Envelope {
                public,
                body: sealed,
            };
            Ok::<_, Error>(json!({"phase":"finished","offer":target.offer,"profile":profile}))
        };
        let value =
            match tokio::time::timeout(Duration::from_secs(expires.saturating_sub(now())), work)
                .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => json!({"phase":"failed","reason":e.to_string()}),
                Err(_) => json!({"phase":"failed","reason":"ChatGPT handoff expired"}),
            };
        tx.send_replace(value);
    });
    Ok((
        flow,
        Job {
            owner,
            expires,
            status,
            callback,
            task,
        },
    ))
}
