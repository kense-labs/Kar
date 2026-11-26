use super::{
    Result, bad,
    catalog::{self, Binary},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use url::Url;

pub(super) fn relative(raw: &str) -> Result<PathBuf> {
    let raw = raw.replace('\\', "/");
    if raw.starts_with('/')
        || raw.as_bytes().get(1) == Some(&b':')
        || raw.contains('\0')
        || raw.split('/').any(|p| p == "..")
    {
        return Err(bad("unsafe path in ACP archive"));
    }
    let mut path = PathBuf::new();
    for part in Path::new(&raw).components() {
        match part {
            Component::CurDir => {}
            Component::Normal(n) => path.push(n),
            _ => return Err(bad("unsafe path in ACP archive")),
        }
    }
    if path.as_os_str().is_empty() {
        return Err(bad("empty path in ACP archive"));
    }
    Ok(path)
}
pub(super) async fn install(root: &Path, source: &Url, b: &Binary) -> Result<(PathBuf, String)> {
    install_with_progress(root, source, b, |_| {}).await
}
pub(super) async fn install_with_progress(
    root: &Path,
    source: &Url,
    b: &Binary,
    stage: impl Fn(&'static str) + Send + Sync,
) -> Result<(PathBuf, String)> {
    stage("downloading");
    let url = Url::parse(&b.archive).map_err(|_| bad("invalid archive URL"))?;
    let mut r = catalog::client(source)?
        .get(url.clone())
        .timeout(Duration::from_secs(1200))
        .send()
        .await?
        .error_for_status()?;
    let archive = tempfile::NamedTempFile::new_in(root)?;
    let mut file = tokio::fs::File::from_std(archive.reopen()?);
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    while let Some(chunk) = r.chunk().await? {
        bytes += chunk.len() as u64;
        if bytes > 1 << 30 {
            return Err(bad("ACP archive exceeds 1 GiB"));
        }
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    stage("verifying");
    let digest = format!("{:x}", digest.finalize());
    if b.sha256
        .as_ref()
        .is_some_and(|v| !v.eq_ignore_ascii_case(&digest))
    {
        return Err(bad("ACP archive SHA-256 mismatch"));
    }
    stage("installing");
    let path = url.path().to_ascii_lowercase();
    let command = relative(&b.cmd)?;
    let dest = root.join("payload");
    tokio::task::spawn_blocking(move || {
        fs::create_dir_all(&dest)?;
        let file = archive.reopen()?;
        if path.ends_with(".zip") {
            unzip(file, &dest)?
        } else if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
            untar(flate2::read::MultiGzDecoder::new(file), &dest)?
        } else if path.ends_with(".tar.bz2") || path.ends_with(".tbz2") {
            untar(bzip2::read::MultiBzDecoder::new(file), &dest)?
        } else {
            fs::create_dir_all(dest.join(&command).parent().unwrap())?;
            fs::copy(archive.path(), dest.join(&command))?;
        }
        let exe = dest.join(&command).canonicalize()?;
        if !exe.starts_with(dest.canonicalize()?) || !exe.is_file() {
            return Err(bad("archive command is not a managed file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = exe.metadata()?.permissions();
            p.set_mode(p.mode() | 0o700);
            fs::set_permissions(exe, p)?;
        }
        Ok((Path::new("payload").join(command), digest))
    })
    .await
    .map_err(|e| bad(&e.to_string()))?
}
struct Extract<'a> {
    root: &'a Path,
    count: usize,
    bytes: u64,
    links: Vec<(PathBuf, PathBuf, bool)>,
}
impl<'a> Extract<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            count: 0,
            bytes: 0,
            links: Vec::new(),
        }
    }
    fn entry(&mut self, name: &str) -> Result<Option<PathBuf>> {
        self.count += 1;
        if self.count > 65_536 {
            return Err(bad("too many entries in ACP archive"));
        }
        if name == "." || name == "./" {
            return Ok(None);
        }
        Ok(Some(self.root.join(relative(name)?)))
    }
    fn file(&mut self, path: &Path, data: impl Read, mode: u32) -> Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        let mut out = File::create_new(path)?;
        let limit = (4u64 << 30) - self.bytes;
        let copied = io::copy(&mut data.take(limit + 1), &mut out)?;
        if copied > limit {
            return Err(bad("expanded ACP archive exceeds 4 GiB"));
        }
        self.bytes += copied;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600 | (mode & 0o155)))?;
        }
        Ok(())
    }
    fn finish(self) -> Result<()> {
        // Extract files first so that no archive member can redirect a later write.
        let root = self.root.canonicalize()?;
        let mut links = self.links;
        while !links.is_empty() {
            let count = links.len();
            let mut waiting = Vec::new();
            for (path, target, hard) in links {
                if target.is_absolute()
                    || target.to_string_lossy().contains('\\')
                    || target.to_string_lossy().as_bytes().get(1) == Some(&b':')
                {
                    return Err(bad("absolute link in ACP archive"));
                }
                let base = if hard {
                    self.root
                } else {
                    path.parent().unwrap()
                };
                let source = match base.join(&target).canonicalize() {
                    Ok(source) => source,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        waiting.push((path, target, hard));
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                if !source.starts_with(&root) {
                    return Err(bad("link escapes ACP archive"));
                }
                fs::create_dir_all(path.parent().unwrap())?;
                if !path.parent().unwrap().canonicalize()?.starts_with(&root) {
                    return Err(bad("link parent escapes ACP archive"));
                }
                if hard {
                    fs::hard_link(source, &path)?;
                } else {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(target, &path)?;
                    #[cfg(windows)]
                    if source.is_dir() {
                        std::os::windows::fs::symlink_dir(target, &path)?
                    } else {
                        std::os::windows::fs::symlink_file(target, &path)?
                    }
                }
            }
            if waiting.len() == count {
                return Err(bad("unresolved links in ACP archive"));
            }
            links = waiting;
        }
        Ok(())
    }
}
fn untar(input: impl Read, root: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(input);
    let mut out = Extract::new(root);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(path) = out.entry(path.to_str().ok_or_else(|| bad("non-UTF8 archive entry"))?)?
        else {
            continue;
        };
        let typ = entry.header().entry_type();
        if typ.is_file() {
            let mode = entry.header().mode()?;
            out.file(&path, &mut entry, mode)?
        } else if typ.is_dir() {
            fs::create_dir_all(path)?
        } else if typ.is_symlink() || typ.is_hard_link() {
            out.links.push((
                path,
                entry
                    .link_name()?
                    .ok_or_else(|| bad("missing link target"))?
                    .into_owned(),
                typ.is_hard_link(),
            ));
        } else {
            return Err(bad("unsupported special file in ACP archive"));
        }
    }
    out.finish()
}
fn unzip(file: File, root: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(file).map_err(|e| bad(&e.to_string()))?;
    let mut out = Extract::new(root);
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| bad(&e.to_string()))?;
        let Some(path) = out.entry(entry.name())? else {
            continue;
        };
        if entry.is_dir() {
            fs::create_dir_all(path)?
        } else if entry.is_symlink() {
            let mut target = String::new();
            entry.take(4097).read_to_string(&mut target)?;
            if target.len() > 4096 {
                return Err(bad("oversized link in ACP archive"));
            }
            out.links.push((path, PathBuf::from(target), false));
        } else {
            let mode = entry.unix_mode().unwrap_or(0);
            out.file(&path, &mut entry, mode)?;
        }
    }
    out.finish()
}
