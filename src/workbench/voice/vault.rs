//! Encrypted owner-local values and explicit host-configured secret-provider namespaces.
use super::*;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use std::io::{self, Read};
use zeroize::Zeroizing;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(super) enum Credential {
    Reference { provider: String, reference: String },
    Sealed { nonce: String, sealed: String },
}
pub(super) fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 4096
        && k.bytes().all(|c| (33..=126).contains(&c))
        && !k.split_once("://").is_some_and(|(scheme, _)| {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"+.-".contains(&c))
        })
}
fn driver(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn invalid_ref() -> Error {
    Error::Refused("invalid voice secret reference".into())
}
fn master(dir: &Path, create: bool) -> Result<Zeroizing<Vec<u8>>> {
    let path = dir.join("keyring.key");
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = match options.open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
            let mut key = Zeroizing::new([0u8; 32]);
            getrandom::fill(key.as_mut())
                .map_err(|_| io::Error::other("voice key generator unavailable"))?;
            let mut t = tempfile::NamedTempFile::new_in(dir)?;
            t.write_all(key.as_ref())?;
            t.as_file().sync_all()?;
            match t.persist_noclobber(&path) {
                Ok(f) => drop(f),
                Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
            options.open(&path)?
        }
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != 32 {
        return Err(invalid_ref());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(invalid_ref());
        }
    }
    let mut key = Zeroizing::new(vec![]);
    file.take(33).read_to_end(&mut key)?;
    if key.len() != 32 {
        return Err(invalid_ref());
    }
    Ok(key)
}
impl Service {
    pub(super) fn credential(&self, provider: &str) -> Result<Option<Credential>> {
        match fs::read(self.dir.join(format!("credential-{provider}.json"))) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub(super) fn put_key(&self, provider: &str, v: Value) -> Result<()> {
        let value = match v {
            Value::Null => None,
            Value::String(key) if valid_key(key.trim()) => {
                let key = Zeroizing::new(key);
                let cipher = Aes256Gcm::new_from_slice(&master(&self.dir, true)?)
                    .map_err(|_| invalid_ref())?;
                let mut nonce = [0u8; 12];
                getrandom::fill(&mut nonce).map_err(|_| invalid_ref())?;
                let sealed = cipher
                    .encrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: key.trim().as_bytes(),
                            aad: provider.as_bytes(),
                        },
                    )
                    .map_err(|_| invalid_ref())?;
                Some(Credential::Sealed {
                    nonce: STANDARD.encode(nonce),
                    sealed: STANDARD.encode(sealed),
                })
            }
            Value::Object(ref obj) => {
                let p = obj
                    .get("provider")
                    .and_then(Value::as_str)
                    .filter(|s| driver(s))
                    .ok_or_else(invalid_ref)?;
                let r = obj
                    .get("reference")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && s.len() <= 1024 && !s.contains(['\r', '\n', '\0']))
                    .ok_or_else(invalid_ref)?;
                Some(Credential::Reference {
                    provider: p.into(),
                    reference: r.into(),
                })
            }
            _ => return Err(Error::Refused("invalid voice credential".into())),
        };
        atomic(
            &self.dir.join(format!("credential-{provider}.json")),
            &serde_json::to_vec(&value)?,
        )
    }
    pub(super) async fn resolve(
        &self,
        provider: &str,
        cred: &Credential,
    ) -> Result<Zeroizing<String>> {
        let key = match cred {
            Credential::Sealed { nonce, sealed } => {
                let nonce = STANDARD
                    .decode(nonce)
                    .ok()
                    .filter(|n| n.len() == 12)
                    .ok_or_else(invalid_ref)?;
                let sealed = STANDARD.decode(sealed).map_err(|_| invalid_ref())?;
                let cipher = Aes256Gcm::new_from_slice(&master(&self.dir, false)?)
                    .map_err(|_| invalid_ref())?;
                let bytes = cipher
                    .decrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: &sealed,
                            aad: provider.as_bytes(),
                        },
                    )
                    .map_err(|_| invalid_ref())?;
                String::from_utf8(bytes).map_err(|_| invalid_ref())?
            }
            Credential::Reference {
                provider,
                reference,
            } => reference_key(provider, reference).await?,
        };
        let key = Zeroizing::new(key);
        if valid_key(key.trim()) {
            Ok(Zeroizing::new(key.trim().into()))
        } else {
            Err(invalid_ref())
        }
    }
}
async fn reference_key(provider: &str, reference: &str) -> Result<String> {
    let path = std::env::var_os("KAR_VOICE_SECRET_PROVIDERS").ok_or_else(invalid_ref)?;
    let file = fs::File::open(path)?;
    let mut bytes = vec![];
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(invalid_ref());
    }
    #[derive(Deserialize)]
    #[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
    enum Driver {
        Directory {
            root: PathBuf,
        },
        #[cfg(target_os = "macos")]
        Keychain {
            service: String,
            keychain: Option<PathBuf>,
        },
    }
    let mut map: BTreeMap<String, Driver> = serde_json::from_slice(&bytes)?;
    match map.remove(provider).ok_or_else(invalid_ref)? {
        Driver::Directory { root } => {
            if !root.is_absolute() {
                return Err(invalid_ref());
            }
            let mut dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
            let mut parts = Path::new(reference).components().peekable();
            while let Some(part) = parts.next() {
                let std::path::Component::Normal(name) = part else {
                    return Err(invalid_ref());
                };
                if parts.peek().is_some() {
                    dir = dir.open_dir_nofollow(name)?;
                } else {
                    let mut opts = cap_std::fs::OpenOptions::new();
                    opts.read(true).follow(FollowSymlinks::No);
                    #[cfg(unix)]
                    {
                        use cap_std::fs::OpenOptionsExt;
                        opts.custom_flags(nix::libc::O_NONBLOCK);
                    }
                    let f = dir.open_with(name, &opts)?.into_std();
                    let m = f.metadata()?;
                    if !m.is_file() || m.len() > 4098 {
                        return Err(invalid_ref());
                    }
                    let mut key = String::new();
                    f.take(4099).read_to_string(&mut key)?;
                    return Ok(key);
                }
            }
            Err(invalid_ref())
        }
        #[cfg(target_os = "macos")]
        Driver::Keychain { service, keychain } => {
            let mut cmd = tokio::process::Command::new("/usr/bin/security");
            cmd.kill_on_drop(true)
                .args([
                    "find-generic-password",
                    "-s",
                    &service,
                    "-a",
                    reference,
                    "-w",
                ])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .env_remove("KAR_WORKBENCH_TOKEN");
            if let Some(file) = keychain {
                if !file.is_absolute() {
                    return Err(invalid_ref());
                }
                cmd.arg(file);
            }
            let out = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output())
                .await
                .map_err(|_| invalid_ref())??;
            if !out.status.success() || out.stdout.len() > 4098 {
                return Err(invalid_ref());
            }
            String::from_utf8(out.stdout).map_err(|_| invalid_ref())
        }
    }
}
