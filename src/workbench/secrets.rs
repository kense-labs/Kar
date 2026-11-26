//! Unified encrypted host secret vault (`secrets.enc` + `0600` `secrets.key`),
//! macOS Keychain (`keychain://service/account`) backend, and 1Password CLI (`op://`)
//! single + Touch-ID-batched (`op inject` over stdin/stdout) secret resolver.

use super::{
    Workbench,
    http::{ApiError, blocking},
};
use crate::{Error, Result};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use axum::{
    Json, Router,
    extract::{Path as Param, State},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const SECRET_REDACTED: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

const VAULT_AAD: &[u8] = b"kar-secrets-vault-v1";
const KEY_FILE: &str = "secrets.key";
const VAULT_FILE: &str = "secrets.enc";
const OP_READ_TIMEOUT: Duration = Duration::from_secs(45);
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SECRET_BYTES: usize = 65_536;

fn refused(msg: impl Into<String>) -> Error {
    Error::Refused(msg.into())
}

fn missing_secret() -> Error {
    io::Error::new(io::ErrorKind::NotFound, "secret not found").into()
}

pub fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn safe_cli_token(token: &str, max_len: usize) -> bool {
    !token.is_empty()
        && token.len() <= max_len
        && !token.starts_with('-')
        && !token.contains(['\0', '\r', '\n'])
}

/// Validate a `1Password` `op://<vault>/<item>/<field>` (or `<vault>/<item>/<section>/<field>`) reference.
pub fn parse_op_reference(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("op://") else {
        return Err(refused("expected an op://<vault>/<item>/<field> reference"));
    };
    if trimmed.len() > 1024
        || trimmed.contains(['\0', '\r', '\n'])
        || trimmed.contains("{{")
        || trimmed.contains("}}")
    {
        return Err(refused("invalid characters in 1Password secret reference"));
    }
    let path_part = rest.split('?').next().unwrap_or(rest);
    let segments: Vec<&str> = path_part.split('/').collect();
    if segments.len() < 3 || segments.iter().any(|seg| seg.trim().is_empty()) {
        return Err(refused(
            "1Password reference must include vault, item, and field (op://vault/item/field)",
        ));
    }
    Ok(trimmed.to_owned())
}

/// Parse `keychain://<service>/<account>` into `(service, account)`.
pub fn parse_keychain_uri(raw: &str) -> Result<(String, String)> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("keychain://") else {
        return Err(refused(
            "expected a keychain://<service>/<account> reference",
        ));
    };
    let Some((service, account)) = rest.split_once('/') else {
        return Err(refused(
            "keychain reference must have format keychain://<service>/<account>",
        ));
    };
    if !safe_cli_token(service, 256) || !safe_cli_token(account, 256) {
        return Err(refused("invalid keychain service or account name"));
    }
    Ok((service.to_owned(), account.to_owned()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretBackendKind {
    Encrypted,
    Onepassword,
    Keychain,
}

/// Public metadata returned by `GET /server/secrets` — never carries secret plaintext or ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMetadata {
    pub name: String,
    pub backend: SecretBackendKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub updated_at: String,
}

/// Config-embeddable secret value supporting encrypted blobs, `op://`, `keychain://`,
/// and transparent upgrade of legacy plaintext JSON strings on next save.
#[derive(Clone, PartialEq, Eq)]
pub enum SecretValue {
    Encrypted {
        nonce: String,
        ciphertext: String,
    },
    OnePassword {
        reference: String,
        account: Option<String>,
    },
    Keychain {
        service: String,
        account: String,
        keychain: Option<PathBuf>,
    },
    LegacyPlaintext(Zeroizing<String>),
}

impl SecretValue {
    pub fn parse_input(raw: impl Into<String>) -> Result<Self> {
        let s = raw.into();
        if s.trim_start().starts_with("op://") {
            let reference = parse_op_reference(&s)?;
            Ok(Self::OnePassword {
                reference,
                account: None,
            })
        } else if s.trim_start().starts_with("keychain://") {
            let (service, account) = parse_keychain_uri(&s)?;
            Ok(Self::Keychain {
                service,
                account,
                keychain: None,
            })
        } else {
            Ok(Self::LegacyPlaintext(Zeroizing::new(s)))
        }
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encrypted { .. } => f.write_str("SecretValue::Encrypted([REDACTED])"),
            Self::OnePassword { reference, account } => f
                .debug_struct("SecretValue::OnePassword")
                .field("reference", reference)
                .field("account", account)
                .finish(),
            Self::Keychain {
                service, account, ..
            } => f
                .debug_struct("SecretValue::Keychain")
                .field("service", service)
                .field("account", account)
                .finish(),
            Self::LegacyPlaintext(_) => f.write_str("SecretValue::LegacyPlaintext([REDACTED])"),
        }
    }
}

impl Serialize for SecretValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Encrypted { nonce, ciphertext } => json!({
                "backend": "encrypted",
                "nonce": nonce,
                "ciphertext": ciphertext,
            })
            .serialize(serializer),
            Self::OnePassword { reference, account } => {
                let mut obj = json!({
                    "backend": "onepassword",
                    "reference": reference,
                });
                if let Some(acc) = account {
                    obj["account"] = json!(acc);
                }
                obj.serialize(serializer)
            }
            Self::Keychain {
                service,
                account,
                keychain,
            } => {
                let mut obj = json!({
                    "backend": "keychain",
                    "service": service,
                    "account": account,
                });
                if let Some(kc) = keychain {
                    obj["keychain"] = json!(kc);
                }
                obj.serialize(serializer)
            }
            Self::LegacyPlaintext(raw) => raw.as_str().serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let v = Value::deserialize(deserializer)?;
        match v {
            Value::String(s) => {
                if s.trim_start().starts_with("op://") {
                    let reference = parse_op_reference(&s).map_err(serde::de::Error::custom)?;
                    Ok(Self::OnePassword {
                        reference,
                        account: None,
                    })
                } else if s.trim_start().starts_with("keychain://") {
                    let (service, account) =
                        parse_keychain_uri(&s).map_err(serde::de::Error::custom)?;
                    Ok(Self::Keychain {
                        service,
                        account,
                        keychain: None,
                    })
                } else {
                    Ok(Self::LegacyPlaintext(Zeroizing::new(s)))
                }
            }
            Value::Object(map) => {
                let backend = map
                    .get("backend")
                    .or_else(|| map.get("provider"))
                    .and_then(Value::as_str);
                match backend {
                    Some("encrypted") | None
                        if map.contains_key("nonce")
                            && (map.contains_key("ciphertext") || map.contains_key("sealed")) =>
                    {
                        let nonce = map
                            .get("nonce")
                            .and_then(Value::as_str)
                            .ok_or_else(|| serde::de::Error::custom("missing nonce"))?
                            .to_owned();
                        let ciphertext = map
                            .get("ciphertext")
                            .or_else(|| map.get("sealed"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| serde::de::Error::custom("missing ciphertext"))?
                            .to_owned();
                        Ok(Self::Encrypted { nonce, ciphertext })
                    }
                    Some("onepassword" | "1password" | "op") => {
                        let raw_ref = map
                            .get("reference")
                            .and_then(Value::as_str)
                            .ok_or_else(|| serde::de::Error::custom("missing op:// reference"))?;
                        let reference =
                            parse_op_reference(raw_ref).map_err(serde::de::Error::custom)?;
                        let account = map
                            .get("account")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned);
                        if let Some(acc) = &account
                            && !safe_cli_token(acc, 256)
                        {
                            return Err(serde::de::Error::custom("invalid 1Password account"));
                        }
                        Ok(Self::OnePassword { reference, account })
                    }
                    Some("keychain") => {
                        let (service, account) =
                            if let Some(uri) = map.get("reference").and_then(Value::as_str) {
                                if uri.starts_with("keychain://") {
                                    parse_keychain_uri(uri).map_err(serde::de::Error::custom)?
                                } else if let Some((svc, acc)) = uri.split_once('/') {
                                    if !safe_cli_token(svc, 256) || !safe_cli_token(acc, 256) {
                                        return Err(serde::de::Error::custom(
                                            "invalid keychain service/account",
                                        ));
                                    }
                                    (svc.to_owned(), acc.to_owned())
                                } else {
                                    return Err(serde::de::Error::custom(
                                        "expected keychain service/account",
                                    ));
                                }
                            } else {
                                let svc = map.get("service").and_then(Value::as_str).ok_or_else(
                                    || serde::de::Error::custom("missing keychain service"),
                                )?;
                                let acc = map.get("account").and_then(Value::as_str).ok_or_else(
                                    || serde::de::Error::custom("missing keychain account"),
                                )?;
                                if !safe_cli_token(svc, 256) || !safe_cli_token(acc, 256) {
                                    return Err(serde::de::Error::custom(
                                        "invalid keychain service or account",
                                    ));
                                }
                                (svc.to_owned(), acc.to_owned())
                            };
                        let keychain = map
                            .get("keychain")
                            .and_then(Value::as_str)
                            .map(PathBuf::from);
                        Ok(Self::Keychain {
                            service,
                            account,
                            keychain,
                        })
                    }
                    _ => Err(serde::de::Error::custom("unsupported secret value object")),
                }
            }
            _ => Err(serde::de::Error::custom(
                "expected secret string or structured secret object",
            )),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredEntry {
    Encrypted {
        nonce: String,
        ciphertext: String,
    },
    OnePassword {
        reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    Keychain {
        service: String,
        account: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keychain: Option<PathBuf>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredSecret {
    name: String,
    updated_at: String,
    entry: StoredEntry,
}

impl StoredSecret {
    fn metadata(&self) -> SecretMetadata {
        let (backend, reference, account) = match &self.entry {
            StoredEntry::Encrypted { .. } => (SecretBackendKind::Encrypted, None, None),
            StoredEntry::OnePassword { reference, account } => (
                SecretBackendKind::Onepassword,
                Some(reference.clone()),
                account.clone(),
            ),
            StoredEntry::Keychain {
                service, account, ..
            } => (
                SecretBackendKind::Keychain,
                Some(format!("keychain://{service}/{account}")),
                None,
            ),
        };
        SecretMetadata {
            name: self.name.clone(),
            backend,
            reference,
            account,
            updated_at: self.updated_at.clone(),
        }
    }

    fn as_secret_value(&self) -> SecretValue {
        match &self.entry {
            StoredEntry::Encrypted { nonce, ciphertext } => SecretValue::Encrypted {
                nonce: nonce.clone(),
                ciphertext: ciphertext.clone(),
            },
            StoredEntry::OnePassword { reference, account } => SecretValue::OnePassword {
                reference: reference.clone(),
                account: account.clone(),
            },
            StoredEntry::Keychain {
                service,
                account,
                keychain,
            } => SecretValue::Keychain {
                service: service.clone(),
                account: account.clone(),
                keychain: keychain.clone(),
            },
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct VaultData {
    #[serde(default)]
    entries: BTreeMap<String, StoredSecret>,
}

#[derive(Serialize, Deserialize)]
struct VaultEnvelope {
    version: u8,
    nonce: String,
    sealed: String,
}

fn master_key(dir: &Path, create: bool) -> Result<Zeroizing<Vec<u8>>> {
    fs::create_dir_all(dir)?;
    let path = dir.join(KEY_FILE);
    let mut options = OpenOptions::new();
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
                .map_err(|_| io::Error::other("secret master key generator unavailable"))?;
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tmp.as_file()
                    .set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            tmp.write_all(key.as_ref())?;
            tmp.as_file().sync_all()?;
            match tmp.persist_noclobber(&path) {
                Ok(f) => drop(f),
                Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
            #[cfg(unix)]
            File::open(dir)?.sync_all()?;
            options.open(&path)?
        }
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != 32 {
        return Err(refused("invalid host secret key file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(refused(
                "host secrets.key must not be accessible to group or others (expected 0600)",
            ));
        }
    }
    let mut key = Zeroizing::new(Vec::with_capacity(33));
    file.take(33).read_to_end(&mut key)?;
    if key.len() != 32 {
        return Err(refused("invalid host secret key length"));
    }
    Ok(key)
}

fn seal_with_aad(key: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<(String, String)> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| refused("invalid AES-256-GCM key"))?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| io::Error::other("nonce RNG unavailable"))?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| refused("failed to encrypt secret payload"))?;
    Ok((STANDARD.encode(nonce), STANDARD.encode(ciphertext)))
}

fn open_with_aad(
    key: &[u8],
    aad: &[u8],
    nonce_b64: &str,
    ciphertext_b64: &str,
) -> Result<Zeroizing<Vec<u8>>> {
    let nonce = STANDARD
        .decode(nonce_b64)
        .ok()
        .filter(|n| n.len() == 12)
        .ok_or_else(|| refused("invalid AES-256-GCM nonce"))?;
    let ciphertext = STANDARD
        .decode(ciphertext_b64)
        .map_err(|_| refused("invalid secret ciphertext encoding"))?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| refused("invalid AES-256-GCM key"))?;
    let bytes = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| {
            refused("secret authentication failed (tampered ciphertext or AAD mismatch)")
        })?;
    Ok(Zeroizing::new(bytes))
}

fn load_vault(dir: &Path) -> Result<VaultData> {
    let path = dir.join(VAULT_FILE);
    let raw = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(VaultData::default()),
        Err(e) => return Err(e.into()),
    };
    let env: VaultEnvelope = serde_json::from_slice(&raw)?;
    if env.version != 1 {
        return Err(refused("unsupported secrets.enc envelope version"));
    }
    let key = master_key(dir, false)?;
    let plain = open_with_aad(&key, VAULT_AAD, &env.nonce, &env.sealed)?;
    Ok(serde_json::from_slice(&plain)?)
}

fn save_vault(dir: &Path, data: &VaultData) -> Result<()> {
    fs::create_dir_all(dir)?;
    let key = master_key(dir, true)?;
    let raw = Zeroizing::new(serde_json::to_vec(data)?);
    let (nonce, sealed) = seal_with_aad(&key, VAULT_AAD, &raw)?;
    let env = VaultEnvelope {
        version: 1,
        nonce,
        sealed,
    };
    let encoded = serde_json::to_vec(&env)?;
    let path = dir.join(VAULT_FILE);
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(&encoded)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[derive(Clone)]
pub struct SecretService {
    inner: Arc<SecretInner>,
}

struct SecretInner {
    dir: PathBuf,
    vault: Mutex<VaultData>,
    op_bin: Option<PathBuf>,
    security_bin: Option<PathBuf>,
}

static WORKBENCH_SERVICES: LazyLock<Mutex<HashMap<PathBuf, SecretService>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl SecretService {
    pub fn open(dir: &Path) -> Result<Self> {
        let vault = load_vault(dir)?;
        Ok(Self {
            inner: Arc::new(SecretInner {
                dir: dir.to_path_buf(),
                vault: Mutex::new(vault),
                op_bin: None,
                security_bin: None,
            }),
        })
    }

    pub fn with_binaries(
        dir: &Path,
        op_bin: Option<PathBuf>,
        security_bin: Option<PathBuf>,
    ) -> Result<Self> {
        let vault = load_vault(dir)?;
        Ok(Self {
            inner: Arc::new(SecretInner {
                dir: dir.to_path_buf(),
                vault: Mutex::new(vault),
                op_bin,
                security_bin,
            }),
        })
    }

    pub(crate) fn for_workbench(app: &Workbench) -> Result<Self> {
        let dir = app.0.state.clone();
        let mut cache = WORKBENCH_SERVICES.lock().unwrap();
        if let Some(svc) = cache.get(&dir) {
            return Ok(svc.clone());
        }
        let svc = Self::open(&dir)?;
        cache.insert(dir, svc.clone());
        Ok(svc)
    }

    fn op_program(&self) -> PathBuf {
        self.inner
            .op_bin
            .clone()
            .or_else(|| std::env::var_os("KAR_OP_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("op"))
    }

    fn security_program(&self) -> PathBuf {
        self.inner
            .security_bin
            .clone()
            .or_else(|| std::env::var_os("KAR_SECURITY_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/usr/bin/security"))
    }

    pub fn list(&self) -> Vec<SecretMetadata> {
        self.inner
            .vault
            .lock()
            .unwrap()
            .entries
            .values()
            .map(StoredSecret::metadata)
            .collect()
    }

    pub fn metadata(&self, name: &str) -> Result<SecretMetadata> {
        if !valid_secret_name(name) {
            return Err(refused("invalid secret name"));
        }
        self.inner
            .vault
            .lock()
            .unwrap()
            .entries
            .get(name)
            .map(StoredSecret::metadata)
            .ok_or_else(missing_secret)
    }

    /// Store an encrypted secret in `secrets.enc` with AAD bound to `name`.
    pub fn put_encrypted(&self, name: &str, plaintext: &str) -> Result<SecretMetadata> {
        if !valid_secret_name(name) {
            return Err(refused("invalid secret name"));
        }
        if plaintext.is_empty() || plaintext.len() > MAX_SECRET_BYTES || plaintext.contains('\0') {
            return Err(refused("secret value must be 1..=65536 non-NUL bytes"));
        }
        if plaintext == SECRET_REDACTED {
            return self.metadata(name);
        }
        let key = master_key(&self.inner.dir, true)?;
        let (nonce, ciphertext) = seal_with_aad(&key, name.as_bytes(), plaintext.as_bytes())?;
        self.upsert_entry(name, StoredEntry::Encrypted { nonce, ciphertext })
    }

    /// Register a 1Password `op://` secret reference (zero secret bytes stored on disk).
    pub fn put_onepassword(
        &self,
        name: &str,
        reference: &str,
        account: Option<String>,
    ) -> Result<SecretMetadata> {
        if !valid_secret_name(name) {
            return Err(refused("invalid secret name"));
        }
        let reference = parse_op_reference(reference)?;
        let account = account
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty());
        if let Some(acc) = &account
            && !safe_cli_token(acc, 256)
        {
            return Err(refused("invalid 1Password account identifier"));
        }
        self.upsert_entry(name, StoredEntry::OnePassword { reference, account })
    }

    /// Register a macOS Keychain `keychain://service/account` reference, optionally storing `plaintext` in Keychain.
    pub async fn put_keychain(
        &self,
        name: &str,
        service: &str,
        account: &str,
        keychain: Option<PathBuf>,
        plaintext: Option<Zeroizing<String>>,
    ) -> Result<SecretMetadata> {
        if !valid_secret_name(name) {
            return Err(refused("invalid secret name"));
        }
        if !safe_cli_token(service, 256) || !safe_cli_token(account, 256) {
            return Err(refused("invalid keychain service or account"));
        }
        if let Some(kc) = &keychain
            && !kc.is_absolute()
        {
            return Err(refused("keychain file path must be absolute"));
        }
        if let Some(secret) = plaintext {
            if secret.is_empty() || secret.len() > MAX_SECRET_BYTES || secret.contains('\0') {
                return Err(refused("keychain secret must be 1..=65536 non-NUL bytes"));
            }
            if secret.as_str() != SECRET_REDACTED {
                self.write_macos_keychain(service, account, keychain.as_deref(), &secret)
                    .await?;
            }
        }
        self.upsert_entry(
            name,
            StoredEntry::Keychain {
                service: service.to_owned(),
                account: account.to_owned(),
                keychain,
            },
        )
    }

    pub fn delete(&self, name: &str) -> Result<bool> {
        if !valid_secret_name(name) {
            return Err(refused("invalid secret name"));
        }
        let mut vault = self.inner.vault.lock().unwrap();
        let mut next = vault.clone();
        let existed = next.entries.remove(name).is_some();
        if existed {
            save_vault(&self.inner.dir, &next)?;
            *vault = next;
        }
        Ok(existed)
    }

    fn upsert_entry(&self, name: &str, entry: StoredEntry) -> Result<SecretMetadata> {
        let mut vault = self.inner.vault.lock().unwrap();
        let mut next = vault.clone();
        let stored = StoredSecret {
            name: name.to_owned(),
            updated_at: jiff::Timestamp::now().to_string(),
            entry,
        };
        let meta = stored.metadata();
        next.entries.insert(name.to_owned(), stored);
        save_vault(&self.inner.dir, &next)?;
        *vault = next;
        Ok(meta)
    }

    /// Upgrade a legacy plaintext `SecretValue` in a JSON config into AES-256-GCM `SecretValue::Encrypted`
    /// authenticated with AAD = `secret_id`. Leaves `Encrypted`, `OnePassword`, and `Keychain` untouched.
    pub fn upgrade_for_save(
        &self,
        secret_id: &str,
        value: SecretValue,
    ) -> Result<(SecretValue, bool)> {
        match value {
            SecretValue::LegacyPlaintext(raw) => {
                if raw.is_empty() || raw.as_str() == SECRET_REDACTED {
                    return Ok((SecretValue::LegacyPlaintext(raw), false));
                }
                let key = master_key(&self.inner.dir, true)?;
                let (nonce, ciphertext) =
                    seal_with_aad(&key, secret_id.as_bytes(), raw.as_bytes())?;
                Ok((SecretValue::Encrypted { nonce, ciphertext }, true))
            }
            other => Ok((other, false)),
        }
    }

    /// Resolve a single stored secret by `name`.
    pub async fn resolve(&self, name: &str) -> Result<Zeroizing<String>> {
        if !valid_secret_name(name) {
            return Err(refused("invalid secret name"));
        }
        let val = self
            .inner
            .vault
            .lock()
            .unwrap()
            .entries
            .get(name)
            .map(StoredSecret::as_secret_value)
            .ok_or_else(missing_secret)?;
        self.resolve_secret_value(name, &val).await
    }

    /// Resolve multiple stored secrets by name, batching all `op://` references per account into
    /// a single `op inject --cache=false` invocation so Touch ID only prompts once!
    pub async fn resolve_many(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, Zeroizing<String>>> {
        let mut pairs = Vec::with_capacity(names.len());
        {
            let vault = self.inner.vault.lock().unwrap();
            for name in names {
                if !valid_secret_name(name) {
                    return Err(refused(format!("invalid secret name: {name}")));
                }
                let stored = vault.entries.get(name).ok_or_else(missing_secret)?;
                pairs.push((name.clone(), stored.as_secret_value()));
            }
        }
        let refs: Vec<(&str, &SecretValue)> = pairs.iter().map(|(n, v)| (n.as_str(), v)).collect();
        let outcomes = self.resolve_values_many(&refs).await;
        let mut resolved = BTreeMap::new();
        for ((name, _), outcome) in pairs.into_iter().zip(outcomes) {
            resolved.insert(name, outcome?);
        }
        Ok(resolved)
    }

    /// Parse a user-supplied secret string (`op://`, `keychain://`, or plaintext) and seal plaintext
    /// into AES-256-GCM authenticated with `secret_id`.
    pub fn seal_input(&self, secret_id: &str, raw: impl Into<String>) -> Result<SecretValue> {
        let parsed = SecretValue::parse_input(raw)?;
        Ok(self.upgrade_for_save(secret_id, parsed)?.0)
    }

    /// Synchronously resolve a `SecretValue` (instantaneous for `Encrypted` and `LegacyPlaintext`;
    /// falls back to synchronous CLI read for `Keychain` / `OnePassword`).
    pub fn resolve_local_sync(
        &self,
        secret_id: &str,
        value: &SecretValue,
    ) -> Result<Zeroizing<String>> {
        match value {
            SecretValue::Encrypted { nonce, ciphertext } => {
                let key = master_key(&self.inner.dir, false)?;
                let bytes = open_with_aad(&key, secret_id.as_bytes(), nonce, ciphertext)?;
                let s = std::str::from_utf8(&bytes)
                    .map_err(|_| refused("decrypted secret is not UTF-8"))?;
                Ok(Zeroizing::new(s.to_owned()))
            }
            SecretValue::LegacyPlaintext(raw) => Ok(raw.clone()),
            SecretValue::Keychain {
                service,
                account,
                keychain,
            } => {
                let mut cmd = std::process::Command::new(self.security_program());
                cmd.args(["find-generic-password", "-s", service, "-a", account, "-w"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .env_remove("KAR_WORKBENCH_TOKEN");
                if let Some(kc) = keychain {
                    cmd.arg(kc);
                }
                let out = cmd.output()?;
                if !out.status.success() {
                    return Err(refused("macOS Keychain item not found or locked"));
                }
                let s = String::from_utf8(out.stdout)
                    .map_err(|_| refused("macOS Keychain secret is not UTF-8"))?;
                Ok(Zeroizing::new(s.trim_end_matches(['\r', '\n']).to_owned()))
            }
            SecretValue::OnePassword { reference, account } => {
                let clean = parse_op_reference(reference)?;
                let mut cmd = std::process::Command::new(self.op_program());
                cmd.arg("--cache=false").arg("read");
                if let Some(acc) = account {
                    cmd.arg(format!("--account={acc}"));
                }
                cmd.arg("--no-newline")
                    .arg(clean)
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .env_remove("KAR_WORKBENCH_TOKEN");
                let out = cmd.output()?;
                if !out.status.success() {
                    return Err(refused("1Password could not read secret reference"));
                }
                let s = String::from_utf8(out.stdout)
                    .map_err(|_| refused("1Password secret is not UTF-8"))?;
                Ok(Zeroizing::new(s))
            }
        }
    }

    /// Resolve a single `SecretValue` (whether `Encrypted`, `OnePassword`, `Keychain`, or `LegacyPlaintext`).
    pub async fn resolve_secret_value(
        &self,
        secret_id: &str,
        value: &SecretValue,
    ) -> Result<Zeroizing<String>> {
        let mut results = self.resolve_values_many(&[(secret_id, value)]).await;
        results.pop().expect("single result")
    }

    /// Unified batched resolver across `Encrypted`, `Keychain`, `OnePassword`, and `LegacyPlaintext`.
    /// Groups all `OnePassword` references by `account` and executes one `op inject` pipe per account.
    pub async fn resolve_values_many(
        &self,
        items: &[(&str, &SecretValue)],
    ) -> Vec<Result<Zeroizing<String>>> {
        let mut out: Vec<Option<Result<Zeroizing<String>>>> =
            (0..items.len()).map(|_| None).collect();
        let mut op_groups: BTreeMap<Option<String>, Vec<(usize, String)>> = BTreeMap::new();

        for (idx, (secret_id, val)) in items.iter().enumerate() {
            match val {
                SecretValue::Encrypted { nonce, ciphertext } => {
                    let res = master_key(&self.inner.dir, false)
                        .and_then(|key| {
                            open_with_aad(&key, secret_id.as_bytes(), nonce, ciphertext)
                        })
                        .and_then(|bytes| {
                            let s = std::str::from_utf8(&bytes)
                                .map_err(|_| refused("decrypted secret is not UTF-8"))?;
                            Ok(Zeroizing::new(s.to_owned()))
                        });
                    out[idx] = Some(res);
                }
                SecretValue::LegacyPlaintext(raw) => {
                    out[idx] = Some(Ok(raw.clone()));
                }
                SecretValue::Keychain {
                    service,
                    account,
                    keychain,
                } => {
                    out[idx] = Some(
                        self.read_macos_keychain(service, account, keychain.as_deref())
                            .await,
                    );
                }
                SecretValue::OnePassword { reference, account } => {
                    match parse_op_reference(reference) {
                        Ok(clean_ref) => {
                            op_groups
                                .entry(account.clone())
                                .or_default()
                                .push((idx, clean_ref));
                        }
                        Err(e) => out[idx] = Some(Err(e)),
                    }
                }
            }
        }

        for (account, group) in op_groups {
            let refs: Vec<String> = group.iter().map(|(_, r)| r.clone()).collect();
            let group_results = self.resolve_op_group(account.as_deref(), &refs).await;
            for ((idx, _), res) in group.into_iter().zip(group_results) {
                out[idx] = Some(res);
            }
        }

        out.into_iter()
            .map(|r| r.unwrap_or_else(|| Err(refused("secret resolution did not complete"))))
            .collect()
    }

    async fn resolve_op_group(
        &self,
        account: Option<&str>,
        references: &[String],
    ) -> Vec<Result<Zeroizing<String>>> {
        if let Some(acc) = account
            && !safe_cli_token(acc, 256)
        {
            return references
                .iter()
                .map(|_| Err(refused("invalid 1Password account identifier")))
                .collect();
        }
        let deadline = tokio::time::Instant::now() + OP_READ_TIMEOUT;
        if references.len() > 1
            && let Ok(Some(batch)) =
                tokio::time::timeout_at(deadline, self.read_op_inject_batch(account, references))
                    .await
        {
            return batch;
        }
        let mut results = Vec::with_capacity(references.len());
        for reference in references {
            let res = match tokio::time::timeout_at(deadline, self.read_op_one(account, reference))
                .await
            {
                Ok(r) => r,
                Err(_) => Err(Error::Timeout(
                    "1Password CLI did not answer in time".into(),
                )),
            };
            results.push(res);
        }
        results
    }

    async fn read_op_one(
        &self,
        account: Option<&str>,
        reference: &str,
    ) -> Result<Zeroizing<String>> {
        let mut cmd = Command::new(self.op_program());
        cmd.kill_on_drop(true).arg("--cache=false").arg("read");
        if let Some(acc) = account {
            cmd.arg(format!("--account={acc}"));
        }
        cmd.arg("--no-newline")
            .arg(reference)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("KAR_WORKBENCH_TOKEN");
        let out = cmd.output().await?;
        if !out.status.success() {
            return Err(refused(format!(
                "1Password could not read reference (exit {:?})",
                out.status.code()
            )));
        }
        let bytes = Zeroizing::new(out.stdout);
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(refused("1Password secret exceeds maximum length"));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| refused("1Password returned non-UTF-8 secret"))?;
        if text.trim().is_empty() {
            return Err(refused("1Password returned an empty secret"));
        }
        Ok(Zeroizing::new(text.to_owned()))
    }

    /// Batched `op --cache=false inject` via stdin/stdout pipe (zero temp files on disk!).
    async fn read_op_inject_batch(
        &self,
        account: Option<&str>,
        references: &[String],
    ) -> Option<Vec<Result<Zeroizing<String>>>> {
        let separator = format!("__kar-secret-{}__", Uuid::new_v4());
        let mut template = Zeroizing::new(String::new());
        for r in references {
            template.push_str("{{ ");
            template.push_str(r);
            template.push_str(" }}");
            template.push_str(&separator);
        }

        let mut cmd = Command::new(self.op_program());
        cmd.kill_on_drop(true).arg("--cache=false").arg("inject");
        if let Some(acc) = account {
            cmd.arg(format!("--account={acc}"));
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("KAR_WORKBENCH_TOKEN");

        let mut child = cmd.spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(template.as_bytes()).await.ok()?;
            stdin.flush().await.ok()?;
            drop(stdin);
        }
        let out = child.wait_with_output().await.ok()?;
        if !out.status.success() {
            return None;
        }
        let raw_bytes = Zeroizing::new(out.stdout);
        let stdout = std::str::from_utf8(&raw_bytes).ok()?;
        let mut parts: Vec<&str> = stdout.split(&separator).collect();
        let trailing = parts.pop()?;
        if parts.len() != references.len() || !trailing.trim().is_empty() {
            return None;
        }
        let mut resolved = Vec::with_capacity(parts.len());
        for value in parts {
            if value.trim().is_empty() || value.len() > MAX_SECRET_BYTES {
                return None;
            }
            resolved.push(Ok(Zeroizing::new(value.to_owned())));
        }
        Some(resolved)
    }

    async fn read_macos_keychain(
        &self,
        service: &str,
        account: &str,
        keychain: Option<&Path>,
    ) -> Result<Zeroizing<String>> {
        if !safe_cli_token(service, 256) || !safe_cli_token(account, 256) {
            return Err(refused("invalid keychain service or account"));
        }
        let mut cmd = Command::new(self.security_program());
        cmd.kill_on_drop(true)
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("KAR_WORKBENCH_TOKEN");
        if let Some(file) = keychain {
            if !file.is_absolute() {
                return Err(refused("keychain path must be absolute"));
            }
            cmd.arg(file);
        }
        let out = tokio::time::timeout(KEYCHAIN_TIMEOUT, cmd.output())
            .await
            .map_err(|_| Error::Timeout("macOS Keychain lookup timed out".into()))??;
        if !out.status.success() {
            return Err(refused("could not read secret from macOS Keychain"));
        }
        let raw = Zeroizing::new(out.stdout);
        if raw.len() > MAX_SECRET_BYTES + 2 {
            return Err(refused("Keychain item exceeds maximum secret size"));
        }
        let s =
            std::str::from_utf8(&raw).map_err(|_| refused("Keychain item is not UTF-8 text"))?;
        let trimmed = s
            .strip_suffix("\r\n")
            .or_else(|| s.strip_suffix('\n'))
            .unwrap_or(s);
        if trimmed.is_empty() {
            return Err(refused("Keychain item is empty"));
        }
        Ok(Zeroizing::new(trimmed.to_owned()))
    }

    async fn write_macos_keychain(
        &self,
        service: &str,
        account: &str,
        keychain: Option<&Path>,
        secret: &str,
    ) -> Result<()> {
        if !safe_cli_token(service, 256) || !safe_cli_token(account, 256) {
            return Err(refused("invalid keychain service or account"));
        }
        let mut cmd = Command::new(self.security_program());
        cmd.kill_on_drop(true)
            .args([
                "add-generic-password",
                "-U",
                "-s",
                service,
                "-a",
                account,
                "-w",
                secret,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("KAR_WORKBENCH_TOKEN");
        if let Some(file) = keychain {
            if !file.is_absolute() {
                return Err(refused("keychain path must be absolute"));
            }
            cmd.arg(file);
        }
        let status = tokio::time::timeout(KEYCHAIN_TIMEOUT, cmd.status())
            .await
            .map_err(|_| Error::Timeout("macOS Keychain write timed out".into()))??;
        if !status.success() {
            return Err(refused("could not store secret in macOS Keychain"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutSecretRequest {
    #[serde(default)]
    backend: Option<SecretBackendKind>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    keychain: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ResolveTestRequest {
    names: Vec<String>,
    references: Vec<String>,
    account: Option<String>,
}

type Reply<T> = std::result::Result<Json<T>, ApiError>;

async fn list_secrets(State(app): State<Workbench>) -> Reply<Value> {
    blocking(move || {
        let svc = SecretService::for_workbench(&app)?;
        Ok(json!({ "secrets": svc.list() }))
    })
    .await
}

async fn get_secret(
    State(app): State<Workbench>,
    Param(name): Param<String>,
) -> Reply<SecretMetadata> {
    blocking(move || SecretService::for_workbench(&app)?.metadata(&name)).await
}

async fn put_secret(
    State(app): State<Workbench>,
    Param(name): Param<String>,
    Json(req): Json<PutSecretRequest>,
) -> Reply<SecretMetadata> {
    let svc = SecretService::for_workbench(&app)?;
    let raw_value = req.value.map(Zeroizing::new);
    let inferred = match req.backend {
        Some(b) => b,
        None => {
            let probe = req
                .reference
                .as_deref()
                .or_else(|| raw_value.as_ref().map(|v| v.as_str()))
                .unwrap_or("");
            if probe.trim_start().starts_with("op://") {
                SecretBackendKind::Onepassword
            } else if probe.trim_start().starts_with("keychain://") || req.service.is_some() {
                SecretBackendKind::Keychain
            } else {
                SecretBackendKind::Encrypted
            }
        }
    };

    let meta = match inferred {
        SecretBackendKind::Encrypted => {
            let val = raw_value
                .as_ref()
                .ok_or_else(|| refused("encrypted secret requires a value"))?;
            svc.put_encrypted(&name, val)?
        }
        SecretBackendKind::Onepassword => {
            let reference = req
                .reference
                .as_deref()
                .or_else(|| raw_value.as_ref().map(|v| v.as_str()))
                .ok_or_else(|| refused("1Password secret requires an op:// reference"))?;
            svc.put_onepassword(&name, reference, req.account)?
        }
        SecretBackendKind::Keychain => {
            let (service, account) = if let Some(uri) = req.reference.as_deref().or_else(|| {
                raw_value
                    .as_ref()
                    .map(|v| v.as_str())
                    .filter(|s| s.trim_start().starts_with("keychain://"))
            }) {
                parse_keychain_uri(uri)?
            } else {
                let svc_name = req.service.as_deref().ok_or_else(|| {
                    refused("keychain secret requires service and account or keychain:// URI")
                })?;
                let acc_name = req
                    .account
                    .as_deref()
                    .ok_or_else(|| refused("keychain secret requires account"))?;
                (svc_name.to_owned(), acc_name.to_owned())
            };
            let to_store = raw_value.filter(|v| !v.trim_start().starts_with("keychain://"));
            svc.put_keychain(&name, &service, &account, req.keychain, to_store)
                .await?
        }
    };
    Ok(Json(meta))
}

async fn delete_secret(State(app): State<Workbench>, Param(name): Param<String>) -> Reply<Value> {
    blocking(move || {
        let deleted = SecretService::for_workbench(&app)?.delete(&name)?;
        if !deleted {
            return Err(missing_secret());
        }
        Ok(json!({ "deleted": true, "name": name }))
    })
    .await
}

async fn resolve_test(
    State(app): State<Workbench>,
    Json(req): Json<ResolveTestRequest>,
) -> Reply<Value> {
    if req.names.len() + req.references.len() == 0 || req.names.len() + req.references.len() > 64 {
        return Err(refused("resolve-test requires 1 to 64 secret names or references").into());
    }
    let svc = SecretService::for_workbench(&app)?;
    let mut owned: Vec<(String, String, SecretBackendKind, Result<SecretValue>)> = Vec::new();
    {
        let vault = svc.inner.vault.lock().unwrap();
        for name in &req.names {
            if !valid_secret_name(name) {
                owned.push((
                    name.clone(),
                    name.clone(),
                    SecretBackendKind::Encrypted,
                    Err(refused("invalid secret name")),
                ));
                continue;
            }
            match vault.entries.get(name) {
                Some(stored) => {
                    let meta = stored.metadata();
                    owned.push((
                        name.clone(),
                        name.clone(),
                        meta.backend,
                        Ok(stored.as_secret_value()),
                    ));
                }
                None => owned.push((
                    name.clone(),
                    name.clone(),
                    SecretBackendKind::Encrypted,
                    Err(missing_secret()),
                )),
            }
        }
    }
    for raw_ref in &req.references {
        if raw_ref.trim_start().starts_with("op://") {
            let parsed = parse_op_reference(raw_ref).map(|reference| SecretValue::OnePassword {
                reference,
                account: req.account.clone(),
            });
            owned.push((
                raw_ref.clone(),
                raw_ref.clone(),
                SecretBackendKind::Onepassword,
                parsed,
            ));
        } else if raw_ref.trim_start().starts_with("keychain://") {
            let parsed =
                parse_keychain_uri(raw_ref).map(|(service, account)| SecretValue::Keychain {
                    service,
                    account,
                    keychain: None,
                });
            owned.push((
                raw_ref.clone(),
                raw_ref.clone(),
                SecretBackendKind::Keychain,
                parsed,
            ));
        } else {
            owned.push((
                raw_ref.clone(),
                raw_ref.clone(),
                SecretBackendKind::Onepassword,
                Err(refused("reference must begin with op:// or keychain://")),
            ));
        }
    }

    let mut valid_indices = Vec::new();
    let mut valid_refs = Vec::new();
    for (idx, (_, secret_id, _, entry)) in owned.iter().enumerate() {
        if let Ok(val) = entry {
            valid_indices.push(idx);
            valid_refs.push((secret_id.as_str(), val));
        }
    }
    let resolved = svc.resolve_values_many(&valid_refs).await;
    let mut by_idx: HashMap<usize, Result<Zeroizing<String>>> =
        valid_indices.into_iter().zip(resolved).collect();

    let mut rows = Vec::with_capacity(owned.len());
    for (idx, (target, _, backend, entry)) in owned.into_iter().enumerate() {
        let outcome = match entry {
            Ok(_) => by_idx.remove(&idx).unwrap(),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(secret) => rows.push(json!({
                "target": target,
                "ok": true,
                "backend": backend,
                "byte_length": secret.len(),
                "redacted": SECRET_REDACTED,
            })),
            Err(e) => rows.push(json!({
                "target": target,
                "ok": false,
                "backend": backend,
                "error": e.to_string(),
            })),
        }
    }
    Ok(Json(json!({ "results": rows })))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/server/secrets", get(list_secrets))
        .route(
            "/server/secrets/{name}",
            get(get_secret).put(put_secret).delete(delete_secret),
        )
        .route("/server/secrets/resolve-test", post(resolve_test))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encrypted_vault_enforces_aad_and_never_exposes_raw_values_in_list() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let svc = SecretService::open(dir.path())?;

        let meta = svc.put_encrypted("openai_api_key", "sk-super-secret-value-123")?;
        assert_eq!(meta.backend, SecretBackendKind::Encrypted);
        assert_eq!(meta.reference, None);

        let listed = serde_json::to_string(&svc.list())?;
        assert!(!listed.contains("sk-super-secret-value-123"));
        assert!(listed.contains("openai_api_key"));

        // Verify secrets.key has strict 0600 permissions and secrets.enc contains no plaintext.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(KEY_FILE))?
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let disk_enc = fs::read_to_string(dir.path().join(VAULT_FILE))?;
        assert!(!disk_enc.contains("sk-super-secret-value-123"));

        // Reopen from disk and resolve.
        let reopened = SecretService::open(dir.path())?;
        let resolved = reopened.resolve("openai_api_key").await?;
        assert_eq!(resolved.as_str(), "sk-super-secret-value-123");

        // Legacy plaintext SecretValue upgrades cleanly and rejects AAD swapping across secret_ids.
        let legacy: SecretValue = serde_json::from_value(json!("legacy-token-xyz"))?;
        assert!(matches!(legacy, SecretValue::LegacyPlaintext(_)));
        let (upgraded, changed) = reopened.upgrade_for_save("slot-a", legacy)?;
        assert!(changed);
        assert!(matches!(upgraded, SecretValue::Encrypted { .. }));
        let serialized = serde_json::to_string(&upgraded)?;
        assert!(!serialized.contains("legacy-token-xyz"));

        assert_eq!(
            reopened
                .resolve_secret_value("slot-a", &upgraded)
                .await?
                .as_str(),
            "legacy-token-xyz"
        );
        // Swapping ciphertext into slot-b must fail AES-256-GCM AAD verification!
        assert!(
            reopened
                .resolve_secret_value("slot-b", &upgraded)
                .await
                .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn onepassword_batches_ten_secrets_into_single_inject_invocation_and_keychain_resolves()
    -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir)?;
        let counter_file = dir.path().join("op-inject-count");
        let mock_op = bin_dir.join("mock-op");
        let mock_sec = bin_dir.join("mock-security");

        // Mock `op` CLI that counts `inject` invocations and replaces `{{ op://vault/item/fieldN }}` over stdin/stdout!
        let op_script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" != \"--cache=false\" ]; then exit 2; fi\n\
             shift\n\
             cmd=\"$1\"; shift\n\
             case \"$cmd\" in\n\
               inject)\n\
                 echo 1 >> \"{}\"\n\
                 sed -E 's|\\{{\\{{ op://([^/]+)/([^/]+)/([^ ]+) \\}}\\}}|resolved:\\1:\\2:\\3|g'\n\
                 ;;\n\
               read)\n\
                 for last; do :; done\n\
                 ref=\"${{last#op://}}\"\n\
                 printf 'single:%s' \"$ref\"\n\
                 ;;\n\
               *) exit 3 ;;\n\
             esac\n",
            counter_file.display()
        );
        fs::write(&mock_op, op_script)?;
        fs::set_permissions(&mock_op, fs::Permissions::from_mode(0o700))?;

        let sec_script = "#!/bin/sh\n\
            if [ \"$1\" = \"find-generic-password\" ]; then\n\
              printf 'kc-secret-%s-%s\\n' \"$3\" \"$5\"\n\
              exit 0\n\
            fi\n\
            exit 0\n";
        fs::write(&mock_sec, sec_script)?;
        fs::set_permissions(&mock_sec, fs::Permissions::from_mode(0o700))?;

        let vault_dir = dir.path().join("vault");
        let svc = SecretService::with_binaries(&vault_dir, Some(mock_op), Some(mock_sec))?;

        let mut names = Vec::new();
        for i in 0..10 {
            let name = format!("op_secret_{i}");
            svc.put_onepassword(
                &name,
                &format!("op://Engineering/APIKeys/key_{i}"),
                Some("team.1password.com".into()),
            )?;
            names.push(name);
        }
        svc.put_keychain("gh_pat", "kar.github", "leon", None, None)
            .await?;
        names.push("gh_pat".into());

        let resolved = svc.resolve_many(&names).await?;
        assert_eq!(resolved.len(), 11);
        for i in 0..10 {
            assert_eq!(
                resolved[&format!("op_secret_{i}")].as_str(),
                format!("resolved:Engineering:APIKeys:key_{i}")
            );
        }
        assert_eq!(resolved["gh_pat"].as_str(), "kc-secret-kar.github-leon");

        // All 10 op:// secrets for the same account were resolved in ONE `op inject` call!
        let calls = fs::read_to_string(&counter_file)?;
        assert_eq!(calls.lines().count(), 1);
        Ok(())
    }
}
