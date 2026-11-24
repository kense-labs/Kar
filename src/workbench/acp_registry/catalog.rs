use super::{Result, bad, packages};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};
use url::Url;

const OFFICIAL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

#[derive(Default)]
pub(in crate::workbench) struct Catalog(tokio::sync::Mutex<Option<(Instant, Url, Vec<Agent>)>>);

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Agent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub icon: Option<String>,
    pub distribution: Distribution,
}
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Distribution {
    #[serde(default)]
    binary: BTreeMap<String, Binary>,
    npx: Option<Package>,
    uvx: Option<Package>,
}
#[derive(Clone, Deserialize, Serialize, Default)]
pub(super) struct Launch {
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Binary {
    pub archive: String,
    pub cmd: String,
    pub sha256: Option<String>,
    #[serde(flatten)]
    pub launch: Launch,
}
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Package {
    pub package: String,
    #[serde(flatten)]
    pub launch: Launch,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "lowercase")]
pub(super) enum Selected {
    Binary(Binary),
    Npx(Package),
    Uvx(Package),
}
impl Selected {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Binary(_) => "binary",
            Self::Npx(_) => "npx",
            Self::Uvx(_) => "uvx",
        }
    }
    pub fn launch(&self) -> &Launch {
        match self {
            Self::Binary(b) => &b.launch,
            Self::Npx(p) | Self::Uvx(p) => &p.launch,
        }
    }
}
pub(super) fn id_ok(id: &str) -> bool {
    id.len() <= 128
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b))
}
pub(super) fn version_ok(v: &str) -> bool {
    v.len() <= 128
        && v.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+_".contains(&b))
}
pub(super) fn platform() -> String {
    format!(
        "{}-{}",
        match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        },
        std::env::consts::ARCH
    )
}
impl Agent {
    pub fn select(&self, kind: &str) -> Option<Selected> {
        let os = platform();
        let binary = self.distribution.binary.get(&os).or_else(|| {
            (os == "windows-aarch64")
                .then(|| self.distribution.binary.get("windows-x86_64"))
                .flatten()
        });
        if matches!(kind, "auto" | "binary")
            && let Some(b) = binary
        {
            return Some(Selected::Binary(b.clone()));
        }
        if matches!(kind, "auto" | "npx")
            && let Some(p) = &self.distribution.npx
        {
            return Some(Selected::Npx(p.clone()));
        }
        if matches!(kind, "auto" | "uvx")
            && let Some(p) = &self.distribution.uvx
        {
            return Some(Selected::Uvx(p.clone()));
        }
        None
    }
    pub fn rank(&self, query: &str) -> Option<u8> {
        let q = query.trim().to_lowercase();
        let name = self.name.to_lowercase();
        let id = &self.id;
        let terms = q.split_whitespace().collect::<Vec<_>>();
        let identity = format!("{id} {name}");
        let tokens: Vec<_> = identity
            .split(|c: char| !c.is_ascii_alphanumeric())
            .collect();
        let authors = self.authors.join(" ").to_lowercase();
        let all = format!("{identity} {authors} {}", self.description.to_lowercase());
        Some(if q.is_empty() {
            100
        } else if id == &q || name == q {
            0
        } else if id.starts_with(&q) || name.starts_with(&q) {
            10
        } else if terms
            .iter()
            .all(|q| tokens.iter().any(|t| t.starts_with(q)))
        {
            20
        } else if terms.iter().all(|q| identity.contains(q)) {
            30
        } else if terms.iter().all(|q| authors.contains(q)) {
            40
        } else if terms.iter().all(|q| all.contains(q)) {
            50
        } else {
            return None;
        })
    }
    fn valid(&self, source: &Url) -> bool {
        let d = &self.distribution;
        let launch_ok = |l: &Launch| {
            l.args.len() <= 64
                && l.args.iter().all(|x| x.len() <= 1024 && !x.contains('\0'))
                && l.env.iter().all(|(k, v)| {
                    !k.is_empty()
                        && k.len() <= 256
                        && !k.contains(['=', '\0'])
                        && v.len() <= 1024
                        && !v.contains('\0')
                })
        };
        id_ok(&self.id)
            && version_ok(&self.version)
            && !self.name.trim().is_empty()
            && self.name.len() <= 160
            && self.description.len() <= 1024
            && d.binary.values().all(|b| {
                b.sha256
                    .as_ref()
                    .is_none_or(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
                    && launch_ok(&b.launch)
                    && super::archive::relative(&b.cmd).is_ok()
                    && Url::parse(&b.archive).is_ok_and(|u| allowed(&u, Some(source)))
            })
            && [("npx", &d.npx), ("uvx", &d.uvx)].iter().all(|(kind, p)| {
                p.as_ref().is_none_or(|p| {
                    launch_ok(&p.launch) && packages::identity(kind, &p.package).is_ok()
                })
            })
    }
}
pub(super) fn allowed(url: &Url, source: Option<&Url>) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(url.host(),Some(url::Host::Ipv4(ip)) if ip.is_loopback())
            && source.is_none_or(|s| s.origin() == url.origin()))
        || (url.scheme() == "http"
            && matches!(url.host(),Some(url::Host::Ipv6(ip)) if ip.is_loopback())
            && source.is_none_or(|s| s.origin() == url.origin()))
}
pub(super) fn client(source: &Url) -> Result<reqwest::Client> {
    let source = source.clone();
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 8 || !allowed(attempt.url(), Some(&source)) {
                attempt.error("invalid ACP registry redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}
impl Catalog {
    pub(super) async fn agents(&self, refresh: bool) -> Result<(Url, Vec<Agent>)> {
        let url =
            Url::parse(&std::env::var("KAR_ACP_REGISTRY_URL").unwrap_or_else(|_| OFFICIAL.into()))
                .map_err(|_| bad("invalid ACP registry URL"))?;
        if !allowed(&url, None) {
            return Err(bad(
                "ACP registry requires HTTPS or an explicit loopback catalog",
            ));
        }
        let mut cache = self.0.lock().await;
        if !refresh
            && let Some((at, source, agents)) = &*cache
            && source == &url
            && at.elapsed() < Duration::from_secs(300)
        {
            return Ok((url, agents.clone()));
        }
        let mut response = client(&url)?
            .get(url.clone())
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(bad("ACP registry exceeds 1 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Index {
            version: String,
            agents: Vec<serde_json::Value>,
        }
        let index: Index = serde_json::from_slice(&bytes)?;
        if !version_ok(&index.version) || index.agents.len() > 512 {
            return Err(bad("invalid ACP registry index"));
        }
        let mut ids = HashSet::new();
        let agents: Vec<Agent> = index
            .agents
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Agent>(v).ok())
            .filter(|a| a.valid(&url) && ids.insert(a.id.clone()))
            .collect();
        *cache = Some((Instant::now(), url.clone(), agents.clone()));
        Ok((url, agents))
    }
}
