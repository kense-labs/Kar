//! Each configured instance has an independent encrypted profile collection and active registration.
use super::*;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use std::{fs, io::Write};
use zeroize::Zeroizing;
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Profile {
    pub client: String,
    pub subject: String,
    pub email: Option<String>,
    pub issuer: String,
    pub scopes: Vec<String>,
    pub access: String,
    pub refresh: Option<String>,
    pub id_token: String,
    pub expires: u64,
    pub earliest: Option<u64>,
}
impl Profile {
    pub fn revision(&self) -> String {
        let mut h = Sha256::new();
        for field in [&self.client, &self.subject, &self.id_token] {
            h.update(field.as_bytes());
            h.update([0]);
        }
        format!("{:x}", h.finalize())
    }
    pub fn view(&self) -> Value {
        json!({"client_id":self.client,"account_email":self.email,"sharing_enabled":self.scopes.iter().any(|v|v==oidc::SCOPE)})
    }
}
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Profiles {
    pub active: Option<String>,
    #[serde(default)]
    pub last: Option<String>,
    pub saved: BTreeMap<String, Profile>,
}
impl Profiles {
    pub fn selected(&self) -> Option<&Profile> {
        self.saved.get(self.active.as_ref()?)
    }
}
#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Sealed {
    nonce: String,
    ciphertext: String,
}
fn crypto() -> Error {
    bad("invalid encrypted ChatGPT profile")
}
pub(super) fn encrypt(key: &[u8], binding: &[u8], payload: &[u8]) -> Result<Sealed> {
    let nonce = random::<12>()?;
    let ciphertext = Aes256Gcm::new_from_slice(key)
        .map_err(|_| crypto())?
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                aad: binding,
                msg: payload,
            },
        )
        .map_err(|_| crypto())?;
    Ok(Sealed {
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ciphertext),
    })
}
pub(super) fn decrypt(key: &[u8], binding: &[u8], s: &Sealed) -> Result<Zeroizing<Vec<u8>>> {
    let nonce = B64.decode(&s.nonce).map_err(|_| crypto())?;
    if nonce.len() != 12 || s.ciphertext.len() > 1024 * 1024 {
        return Err(crypto());
    }
    let bytes = B64.decode(&s.ciphertext).map_err(|_| crypto())?;
    Aes256Gcm::new_from_slice(key)
        .map_err(|_| crypto())?
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                aad: binding,
                msg: &bytes,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| crypto())
}
pub(super) fn dir(chats: &ChatStore, id: &str) -> PathBuf {
    chats
        .directory()
        .join("workbench/chatgpt")
        .join(format!("{:x}", Sha256::digest(id.as_bytes())))
}
pub(super) fn ensure(dir: &Path) -> Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)?;
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() {
        return Err(crypto());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.mode() & 0o077 != 0 || m.uid() != nix::unistd::geteuid().as_raw() {
            return Err(crypto());
        }
    }
    Ok(())
}
pub(super) fn lock(dir: &Path) -> Result<fs::File> {
    ensure(dir)?;
    let f = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("profiles.lock"))?;
    f.try_lock().map_err(|e| match e {
        fs::TryLockError::WouldBlock => Error::Busy,
        fs::TryLockError::Error(e) => e.into(),
    })?;
    Ok(f)
}
fn key(dir: &Path, create: bool) -> Result<Zeroizing<Vec<u8>>> {
    let path = dir.join("profiles.key");
    if create && !path.try_exists()? {
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        f.write_all(&*Zeroizing::new(random::<32>()?))?;
        f.as_file().sync_all()?;
        match f.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.error.into()),
        }
    }
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || meta.len() != 32 {
        return Err(crypto());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 || meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(crypto());
        }
    }
    Ok(Zeroizing::new(fs::read(path)?))
}
fn binding(dir: &Path) -> &[u8] {
    dir.file_name()
        .expect("profile directory")
        .as_encoded_bytes()
}
pub(super) fn read(dir: &Path) -> Result<Profiles> {
    let Some(sealed) =
        crate::workbench::mcp_apps::record::read::<Sealed>(&dir.join("profiles.sealed"))?
    else {
        return Ok(Default::default());
    };
    serde_json::from_slice(&decrypt(&key(dir, false)?, binding(dir), &sealed)?)
        .map_err(|_| crypto())
}
pub(super) fn save(dir: &Path, p: &Profiles) -> Result<()> {
    ensure(dir)?;
    let sealed = encrypt(
        &key(dir, true)?,
        binding(dir),
        &Zeroizing::new(serde_json::to_vec(p)?),
    )?;
    if sealed.ciphertext.len() > 1024 * 1024 {
        return Err(bad("ChatGPT profile collection exceeds storage limit"));
    }
    let mut f = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut f, &sealed)?;
    f.as_file().sync_all()?;
    f.persist(dir.join("profiles.sealed"))
        .map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
pub(super) async fn access(dir: &Path, revision: &str) -> Result<Zeroizing<String>> {
    let _lock = loop {
        match lock(dir) {
            Ok(lock) => break lock,
            Err(Error::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) => return Err(e),
        }
    };
    let mut db = read(dir)?;
    let p = db
        .active
        .as_ref()
        .and_then(|c| db.saved.get_mut(c))
        .ok_or_else(|| bad("no active ChatGPT profile"))?;
    if p.revision() != revision || !p.scopes.iter().any(|s| s == oidc::SCOPE) {
        return Err(bad("ChatGPT profile changed; reopen the chat"));
    }
    if p.expires > now() + 60 || (p.earliest.is_some_and(|t| t > now()) && p.expires > now()) {
        return Ok(Zeroizing::new(p.access.clone()));
    }
    if p.earliest.is_some_and(|t| t > now()) {
        return Err(bad("ChatGPT renewal is not available yet"));
    }
    let auth = oidc::Issuer::discover().await?;
    if auth.doc.issuer != p.issuer {
        return Err(bad("ChatGPT issuer changed; reconnect"));
    }
    let secret = p
        .refresh
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("ChatGPT profile requires reconnection"))?;
    let t = auth
        .exchange(&[
            ("grant_type", "refresh_token"),
            ("resource", oidc::RESOURCE),
            ("client_id", &p.client),
            ("refresh_token", secret),
        ])
        .await?;
    // A rotated refresh token must survive together with revoked sharing scope.
    if !t.token_type.eq_ignore_ascii_case("bearer")
        || t.refresh_token.as_deref().is_none_or(str::is_empty)
        || t.access_token.is_empty()
        || t.access_token.chars().any(char::is_whitespace)
        || t.expires_in == 0
    {
        return Err(bad("invalid ChatGPT renewal"));
    }
    p.expires = now()
        .checked_add(t.expires_in)
        .ok_or_else(|| bad("invalid renewal expiry"))?;
    p.earliest = t.earliest();
    p.access = t.access_token;
    p.refresh = t.refresh_token;
    p.scopes = t
        .scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    let token = p
        .scopes
        .iter()
        .any(|s| s == oidc::SCOPE)
        .then(|| Zeroizing::new(p.access.clone()));
    save(dir, &db)?;
    token.ok_or_else(|| bad("ChatGPT token sharing was revoked"))
}
