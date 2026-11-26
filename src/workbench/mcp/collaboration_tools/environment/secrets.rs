use super::*;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use zeroize::Zeroizing;
const DAY: Duration = Duration::from_secs(86400);
fn dir(state: &Path, p: Uuid, c: Uuid) -> PathBuf {
    state
        .join("private-input")
        .join(p.to_string())
        .join(c.to_string())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Field {
    name: String,
    label: String,
    #[serde(default)]
    placeholder: Option<String>,
    #[serde(default)]
    secret: Option<bool>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Secret {
    request_id: Uuid,
    label: String,
    reason: String,
    url: Option<String>,
    placeholder: Option<String>,
    delivery: Option<String>,
    fields: Option<Vec<Field>>,
    timeout_ms: Option<u64>,
}
pub(super) fn spec(input: Value) -> Result<(Uuid, Value, u64)> {
    let m: Secret = serde_json::from_value(input)?;
    let bound = |s: &str, n| !s.trim().is_empty() && s.len() <= n;
    let timeout = m.timeout_ms.unwrap_or(600000);
    if !bound(&m.label, 256)
        || !bound(&m.reason, 2000)
        || m.placeholder.as_deref().is_some_and(|s| s.len() > 512)
        || !(1000..=3600000).contains(&timeout)
        || !matches!(m.delivery.as_deref(), None | Some("secret_ref" | "file"))
    {
        return Err(bad("invalid private input request"));
    }
    if let Some(link) = &m.url {
        let url = url::Url::parse(link).map_err(|_| bad("invalid private input URL"))?;
        if !matches!(url.scheme(), "https" | "http")
            || !url.has_host()
            || !url.username().is_empty()
            || url.password().is_some()
            || link.len() > 4096
        {
            return Err(bad("invalid private input URL"));
        }
    }
    if let Some(fields) = &m.fields {
        let mut names = std::collections::BTreeSet::new();
        if fields.is_empty()
            || fields.len() > 8
            || m.delivery.as_deref() != Some("file")
            || !fields.iter().all(|f| {
                let k = &f.name;
                k.len() <= 32
                    && k.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && k.bytes().all(|v| v == b'_' || v.is_ascii_alphanumeric())
                    && names.insert(k)
                    && bound(&f.label, 160)
                    && f.placeholder.as_ref().is_none_or(|s| s.len() <= 512)
            })
        {
            return Err(bad("invalid private input fields"));
        }
    }
    Ok((m.request_id, json!(m), timeout))
}

fn create(root: &Path, bytes: &str, file: bool) -> Result<(Uuid, PathBuf)> {
    fs::create_dir_all(root)?;
    let id = Uuid::new_v4();
    let path = root.join(format!("{id}.{}", if file { "file" } else { "ref" }));
    let mut tmp = tempfile::NamedTempFile::new_in(root)?;
    tmp.write_all(bytes.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(&path).map_err(|e| e.error)?;
    Ok((id, path))
}
pub(super) fn accept(app: &Workbench, card: &cards::Card, mut input: Value) -> Result<Value> {
    let defs: Secret = serde_json::from_value(card.details.clone())?;
    let map = input
        .as_object_mut()
        .ok_or_else(|| bad("expected private input fields"))?;
    let names = defs
        .fields
        .as_ref()
        .map(|f| f.iter().map(|v| v.name.as_str()).collect::<Vec<_>>())
        .unwrap_or_else(|| vec!["value"]);
    if map.len() != names.len()
        || names.iter().any(|n| {
            map.get(*n)
                .and_then(|v| v.as_str())
                .is_none_or(|s| s.is_empty() || s.len() > 65536 || s.contains(['\r', '\n', '\0']))
        })
    {
        return Err(bad(
            "provide exactly one single-line value for each requested field",
        ));
    }
    let mut files = serde_json::Map::new();
    let root = dir(&app.0.state, card.project, card.chat);
    for name in names {
        let secret = Zeroizing::new(map.remove(name).unwrap().as_str().unwrap().to_string());
        let file = defs.delivery.as_deref() == Some("file");
        let (id, path) = create(&root, &secret, file)?;
        if !file {
            return Ok(json!({"status":"saved","secret_ref":id}));
        }
        files.insert(name.into(), json!(path));
    }
    Ok(if defs.fields.is_none() {
        json!({"status":"saved","file":files["value"]})
    } else {
        json!({"status":"saved","files":files})
    })
}
pub(in crate::workbench) fn clean(state: &Path) -> Result<()> {
    let root = state.join("private-input");
    if !root.try_exists()? {
        return Ok(());
    }
    for p in root.read_dir()? {
        let p = p?;
        if !p.file_type()?.is_dir() {
            continue;
        }
        for c in p.path().read_dir()? {
            let c = c?;
            if !c.file_type()?.is_dir() {
                continue;
            }
            for f in c.path().read_dir()? {
                let f = f?;
                if f.file_type()?.is_file()
                    && f.metadata()?
                        .modified()?
                        .elapsed()
                        .is_ok_and(|age| age >= DAY)
                {
                    match fs::remove_file(f.path()) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}
impl Workbench {
    pub(in crate::workbench) fn with_model_secret<T>(
        &self,
        project: Uuid,
        chat: Uuid,
        reference: Uuid,
        commit: impl FnOnce(&str) -> Result<T>,
    ) -> Result<T> {
        self.binding(project, chat)?;
        let parent = dir(&self.0.state, project, chat);
        let path = parent.join(format!("{reference}.ref"));
        let mut f = fs::File::open(&path)
            .map_err(|_| bad("private input reference is expired, used, or outside this chat"))?;
        f.try_lock().map_err(|err| match err {
            fs::TryLockError::WouldBlock => crate::Error::Busy,
            fs::TryLockError::Error(err) => err.into(),
        })?;
        // An earlier holder may have committed and unlinked after we opened the file.
        if !path.try_exists()?
            || f.metadata()?
                .modified()?
                .elapsed()
                .map_or(true, |a| a >= DAY)
        {
            return Err(bad("private input expired or already consumed"));
        }
        let mut value = Zeroizing::new(String::new());
        Read::by_ref(&mut f)
            .take(65537)
            .read_to_string(&mut value)?;
        if value.len() > 65536 {
            return Err(bad("private input exceeded limit"));
        }
        let result = commit(&value)?;
        fs::remove_file(path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(result)
    }
}
