use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio_util::sync::CancellationToken;
pub(super) struct Download {
    pub id: String,
    cancel: CancellationToken,
    done: watch::Sender<Option<std::result::Result<bool, String>>>,
    bytes: AtomicU64,
    verifying: AtomicBool,
}
impl Download {
    pub fn progress(&self) -> (u64, bool) {
        (
            self.bytes.load(Ordering::Relaxed),
            self.verifying.load(Ordering::Relaxed),
        )
    }
    async fn wait(&self) -> Result<bool> {
        let mut rx = self.done.subscribe();
        let res = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::Closed("voice download stopped".into()))?
            .clone()
            .unwrap();
        res.map_err(Error::Refused)
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime: std::time::SystemTime,
    device: u64,
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}
fn stamp(m: &fs::Metadata) -> Option<Stamp> {
    if !m.is_file() {
        return None;
    }
    #[cfg(unix)]
    let (device, inode, changed) = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino(), (m.ctime(), m.ctime_nsec()))
    };
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    Some(Stamp {
        size: m.len(),
        mtime: m.modified().ok()?,
        device,
        inode,
        #[cfg(unix)]
        changed,
    })
}
pub(super) struct Verification {
    stamp: Stamp,
    digest: String,
}
impl Service {
    pub(super) fn path(&self, m: &Model, s: &Settings) -> PathBuf {
        s.links
            .get(&m.id)
            .cloned()
            .unwrap_or_else(|| self.dir.join("models").join(&m.filename))
    }
    pub(super) fn ready(&self, m: &Model, path: &Path) -> bool {
        self.verify(m, path).unwrap_or(false)
    }
    pub(super) fn verify(&self, m: &Model, path: &Path) -> Result<bool> {
        let mut open = fs::OpenOptions::new();
        open.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.custom_flags(nix::libc::O_NONBLOCK);
        }
        let mut f = match open.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let Some(st) = stamp(&f.metadata()?).filter(|s| s.size == m.size) else {
            return Ok(false);
        };
        if let Some(v) = self
            .verified
            .lock()
            .unwrap()
            .get(path)
            .filter(|v| v.stamp == st)
        {
            return Ok(v.digest == m.sha256);
        }
        let mut hash = Sha256::new();
        io::copy(&mut (&mut f).take(m.size + 1), &mut hash)?;
        if fs::metadata(path).ok().and_then(|m| stamp(&m)).as_ref() != Some(&st) {
            return Ok(false);
        }
        let digest = format!("{:x}", hash.finalize());
        let valid = digest == m.sha256;
        self.verified
            .lock()
            .unwrap()
            .insert(path.to_owned(), Verification { stamp: st, digest });
        Ok(valid)
    }
    pub(super) fn link(&self, source: &Path) -> Result<()> {
        let _gate = self.exclusive()?;
        let mut pending = vec![(source.canonicalize()?, 0)];
        let mut visited = HashSet::new();
        let mut linked = BTreeMap::new();
        let mut first = None;
        for _ in 0..5000 {
            let Some((p, depth)) = pending.pop() else {
                break;
            };
            let Ok(meta) = fs::metadata(&p) else {
                continue;
            };
            if meta.is_dir() && depth < 5 {
                let Ok(canonical) = p.canonicalize() else {
                    continue;
                };
                if !visited.insert(canonical) {
                    continue;
                }
                if let Ok(entries) = fs::read_dir(&p) {
                    pending.extend(
                        entries
                            .take(5000 - pending.len().min(5000))
                            .filter_map(|e| e.ok())
                            .map(|e| (e.path(), depth + 1)),
                    );
                }
            } else if meta.is_file() {
                for m in CATALOG.models.iter().filter(|m| m.size == meta.len()) {
                    if linked.contains_key(&m.id) || !self.verify(m, &p)? {
                        continue;
                    }
                    let p = p.canonicalize()?;
                    first.get_or_insert(m.id.clone());
                    if p != self.dir.join("models").join(&m.filename) {
                        linked.insert(m.id.clone(), p);
                    }
                }
            }
        }
        let first =
            first.ok_or_else(|| Error::Refused("no files match a pinned speech model".into()))?;
        self.update(|s| {
            s.links.extend(linked);
            if let Ok(m) = model(&s.model)
                && !self.ready(m, &self.path(m, s))
            {
                s.model = first;
            }
        })
    }
    pub(super) fn remove(&self, m: &Model) -> Result<()> {
        let _gate = self.exclusive()?;
        if self
            .active
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|j| j.id == m.id)
        {
            return Err(Error::Busy);
        }
        let managed = self.dir.join("models").join(&m.filename);
        match fs::remove_file(managed) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.update(|s| {
            s.links.remove(&m.id);
            if s.model == m.id
                && let Some(next) = CATALOG
                    .models
                    .iter()
                    .find(|other| other.id != m.id && self.ready(other, &self.path(other, s)))
            {
                s.model = next.id.clone();
            }
        })
    }
    pub(super) async fn cancel(&self, id: &str) -> Result<()> {
        let active = self.active.lock().unwrap().clone();
        if let Some(j) = active.filter(|j| j.id == id) {
            j.cancel.cancel();
            j.wait().await?;
        }
        Ok(())
    }
}
pub(super) async fn download(app: Workbench, m: &'static Model) -> Result<bool> {
    let active = {
        let service = &app.0.voice;
        let mut active = service.active.lock().unwrap();
        if let Some(j) = active.as_ref() {
            if j.id != m.id {
                return Err(Error::Busy);
            }
            j.clone()
        } else {
            let job = Arc::new(Download {
                id: m.id.clone(),
                cancel: CancellationToken::new(),
                done: watch::channel(None).0,
                bytes: AtomicU64::new(0),
                verifying: AtomicBool::new(false),
            });
            *active = Some(job.clone());
            let run = job.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let result = acquire(&app, m, &run).await;
                let svc = &app.0.voice;
                let mut current = svc.active.lock().unwrap();
                current.take();
                run.done
                    .send_replace(Some(result.map_err(|e| e.to_string())));
                drop(current);
                svc.changed();
            });
            job
        }
    };
    active.wait().await
}
async fn acquire(app: &Workbench, m: &'static Model, job: &Arc<Download>) -> Result<bool> {
    let service = &app.0.voice;
    let _gate = service.exclusive()?;
    let source = service.path(m, &service.config());
    if verify(app, m, source.clone()).await? {
        return Ok(false);
    }
    if let Some(home) = std::env::home_dir() {
        let handy = home
            .join("Library/Application Support/com.pais.handy/models")
            .join(&m.filename);
        let hf = home
            .join(".cache/huggingface/hub")
            .join(format!("models--{}", m.id.replace('/', "--")))
            .join("snapshots")
            .join(&m.revision)
            .join(&m.filename);
        for candidate in [hf, handy] {
            if job.cancel.is_cancelled() {
                return Ok(true);
            }
            if verify(app, m, candidate.clone()).await? {
                service.update(|s| {
                    s.links.insert(m.id.clone(), candidate);
                })?;
                return Ok(false);
            }
        }
    }
    let mirror =
        std::env::var("KAR_VOICE_MODEL_MIRROR").unwrap_or_else(|_| "https://huggingface.co".into());
    let url = format!(
        "{}/{}/resolve/{}/{}",
        mirror.trim_end_matches('/'),
        m.id,
        m.revision,
        m.filename
    );
    let dest = service.dir.join("models").join(&m.filename);
    let tmp = tempfile::NamedTempFile::new_in(dest.parent().unwrap())?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Closed(e.to_string()))?;
    for attempt in 0..3 {
        let res = tokio::select! {biased; _=job.cancel.cancelled()=>return Ok(true),res=fetch(&client,&url,tmp.as_file(),m,job,service)=>res};
        match res {
            Ok(()) => break,
            Err(e) if attempt == 2 => return Err(e),
            Err(_) => continue,
        }
    }
    job.verifying.store(true, Ordering::Relaxed);
    service.changed();
    if !verify(app, m, tmp.path().to_owned()).await? {
        return Err(Error::Refused(
            "speech model failed SHA-256 verification".into(),
        ));
    }
    if job.cancel.is_cancelled() {
        return Ok(true);
    }
    tmp.as_file().sync_all()?;
    tmp.persist(&dest).map_err(|e| e.error)?;
    service.update(|s| {
        s.links.remove(&m.id);
    })?;
    Ok(false)
}
async fn verify(app: &Workbench, m: &'static Model, path: PathBuf) -> Result<bool> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || app.0.voice.verify(m, &path))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    file: &File,
    m: &Model,
    j: &Download,
    svc: &Service,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut at = file.metadata()?.len();
    let mut req = client.get(url);
    if at > 0 {
        req = req.header("Range", format!("bytes={at}-"));
    }
    let err = |e: reqwest::Error| Error::Closed(e.to_string());
    let mut response = req.send().await.map_err(err)?;
    match response.status().as_u16() {
        206 if at > 0
            && response
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                == Some(format!("bytes {at}-{}/{}", m.size - 1, m.size).as_str()) => {}
        200 => {
            file.set_len(0)?;
            at = 0;
        }
        _ => {
            return Err(Error::Refused(
                "speech model server returned an invalid download or range".into(),
            ));
        }
    }
    if response
        .content_length()
        .is_some_and(|n| Some(n) != m.size.checked_sub(at))
    {
        return Err(Error::Refused("speech model size mismatch".into()));
    }
    let mut dest = tokio::fs::File::from_std(file.try_clone()?);
    dest.seek(std::io::SeekFrom::Start(at)).await?;
    while let Some(data) = response.chunk().await.map_err(err)? {
        at = at
            .checked_add(data.len() as u64)
            .filter(|n| *n <= m.size)
            .ok_or_else(|| Error::Refused("speech model download exceeds expected size".into()))?;
        dest.write_all(&data).await?;
        j.bytes.store(at, Ordering::Relaxed);
        svc.changed();
    }
    dest.flush().await?;
    if at != m.size {
        return Err(Error::Refused("incomplete speech model transfer".into()));
    }
    Ok(())
}
