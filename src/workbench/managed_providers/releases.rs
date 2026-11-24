use super::*;
use reqwest::Client;
use sha2::{Digest, Sha256, Sha512};
use std::io::{Read, Write};
use url::Url;
pub(super) const MAX_BYTES: u64 = 1 << 30;
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Asset {
    pub version: String,
    pub target: String,
    url: String,
    sha256: Option<String>,
    sha512: Option<String>,
    bytes: Option<u64>,
}
impl Asset {
    fn digest(&self) -> Result<&str> {
        let (s, len) = match (&self.sha256, &self.sha512) {
            (Some(s), None) => (s, 64),
            (None, Some(s)) => (s, 128),
            _ => return Err(bad("Invalid release digest")),
        };
        if s.len() != len || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("Invalid release digest"));
        }
        Ok(s)
    }
}
fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let old = attempt.previous().first();
            if attempt.previous().len() > 5
                || old.is_some_and(|u| {
                    u.scheme() == "https" && attempt.url().scheme() != "https"
                        || u.scheme() == "http" && u.origin() != attempt.url().origin()
                })
            {
                attempt.error("Rejected redirect")
            } else {
                attempt.follow()
            }
        }))
        .timeout(std::time::Duration::from_secs(2700))
        .build()?)
}
async fn blob(client: &Client, url: &str) -> Result<Vec<u8>> {
    let mut r = client
        .get(url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?;
    let mut b = Vec::new();
    while let Some(c) = r.chunk().await? {
        if b.len() + c.len() > 131072 {
            return Err(bad("Release record exceeds limit"));
        }
        b.extend(c)
    }
    Ok(b)
}
async fn asset(driver: &str, updates: bool) -> Result<Asset> {
    if std::env::consts::OS != "macos" {
        return Err(Error::Unsupported(
            "Managed provider installation is available on macOS".into(),
        ));
    }
    let arm = match std::env::consts::ARCH {
        "aarch64" => true,
        "x86_64" => false,
        _ => return Err(Error::Unsupported("Unsupported macOS architecture".into())),
    };
    let cpu = if arm { "arm64" } else { "x64" };
    if let Some(file) = std::env::var_os("KAR_MANAGED_PROVIDER_CATALOG") {
        let map: BTreeMap<String, Asset> = serde_json::from_reader(File::open(file)?.take(131072))?;
        let a = map
            .get(driver)
            .cloned()
            .ok_or_else(|| bad("No host-configured release for this driver"))?;
        let u = Url::parse(&a.url).map_err(|_| bad("Invalid host-configured release URL"))?;
        if u.scheme() != "https"
            && !(u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "[::1]")))
        {
            return Err(bad("Release archives require HTTPS or a loopback fixture"));
        }
        return Ok(a);
    }
    let c = client()?;
    if driver == "antigravity" {
        let mac = if arm { "arm64" } else { "amd64" };
        let base =
            "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests";
        let raw: Value =
            serde_json::from_slice(&blob(&c, &format!("{base}/darwin_{mac}.json")).await?)?;
        let get = |k: &str| {
            raw[k]
                .as_str()
                .ok_or_else(|| bad("Invalid Antigravity release manifest"))
        };
        let version = get("version")?;
        semver::Version::parse(version).map_err(|_| bad("Invalid release version"))?;
        let url = get("url")?;
        let prefix =
            format!("https://storage.googleapis.com/antigravity-public/antigravity-cli/{version}-");
        let folder = if arm { "darwin-arm" } else { "darwin-x64" };
        let target = format!("{folder}/cli_mac_{cpu}.tar.gz");
        if !url
            .strip_prefix(&prefix)
            .and_then(|s| s.split_once('/'))
            .is_some_and(|(build, f)| {
                !build.is_empty() && build.bytes().all(|b| b.is_ascii_digit()) && f == target
            })
        {
            return Err(bad("Antigravity asset does not match this platform"));
        }
        return Ok(Asset {
            version: version.into(),
            url: url.into(),
            target: format!("darwin_{mac}"),
            sha512: Some(get("sha512")?.into()),
            sha256: None,
            bytes: None,
        });
    }
    let pinned: BTreeMap<String, Asset> = serde_json::from_str(include_str!("codex-macos.json"))?;
    let pinned = pinned
        .get(&format!("darwin-{cpu}"))
        .cloned()
        .ok_or_else(|| bad("No pinned Codex asset"))?;
    if !updates {
        return Ok(pinned);
    }
    let latest = async {
        let r: Value = serde_json::from_slice(
            &blob(&c, "https://registry.npmjs.org/@openai/codex/latest").await?,
        )?;
        let version = r["version"]
            .as_str()
            .ok_or_else(|| bad("Missing Codex release version"))?;
        let v =
            semver::Version::parse(version).map_err(|_| bad("Malformed Codex release version"))?;
        if v <= semver::Version::parse(&pinned.version).expect("bundled version") {
            return Ok::<_, Error>(pinned.clone());
        }
        let path = format!("https://github.com/openai/codex/releases/download/rust-v{version}");
        let name = format!("codex-package-{}.tar.gz", pinned.target);
        let sums = blob(&c, &format!("{path}/codex-package_SHA256SUMS")).await?;
        let sha = std::str::from_utf8(&sums)
            .ok()
            .and_then(|s| {
                s.lines().find_map(|line| {
                    let mut p = line.split_whitespace();
                    let hash = p.next()?;
                    let f = p.next()?.trim_start_matches('*');
                    (f == name).then_some(hash)
                })
            })
            .ok_or_else(|| bad("Missing published Codex checksum"))?;
        let a = Asset {
            version: version.into(),
            target: pinned.target.clone(),
            url: format!("{path}/{name}"),
            sha256: Some(sha.to_owned()),
            sha512: None,
            bytes: None,
        };
        a.digest()?;
        Ok(a)
    }
    .await;
    Ok(latest.unwrap_or(pinned))
}
pub(super) async fn prepare(
    driver: &str,
    updates: bool,
    stage: &Path,
    progress: &watch::Sender<View>,
    cancel: &CancellationToken,
) -> Result<Record> {
    let candidate = tokio::select! {r=asset(driver,updates)=>r,_=cancel.cancelled()=>return Err(bad("Cancelled"))};
    let root = stage.parent().expect("installation staging parent");
    if let Some(active) = read::<Record>(&root.join("active.json"))? {
        let version = semver::Version::parse(&active.version)
            .map_err(|_| bad("Invalid active release version"))?;
        let current = candidate
            .as_ref()
            .ok()
            .and_then(|r| semver::Version::parse(&r.version).ok());
        if current.as_ref().is_none_or(|r| version >= *r) {
            active.path(root)?;
            progress.send_modify(|s| {
                s.version = Some(active.version.clone());
                s.phase = "verifying".into()
            });
            let published = root.join("releases").join(&active.digest);
            let check = verify(&published, stage, &active);
            tokio::select! {r=tokio::time::timeout(std::time::Duration::from_secs(90),check)=>r.map_err(|_|bad("Provider verification timed out"))??,_=cancel.cancelled()=>return Err(bad("Cancelled"))}
            return Ok(active);
        }
    }
    let a = candidate?;
    let hash = a.digest()?.to_ascii_lowercase();
    semver::Version::parse(&a.version).map_err(|_| bad("Invalid selected provider version"))?;
    let client = client()?;
    let mut response = tokio::select! {r=client.get(&a.url).send()=>r?.error_for_status()?,_=cancel.cancelled()=>return Err(bad("Cancelled"))};
    let size = a.bytes.or(response.content_length());
    if size.is_some_and(|v| v == 0 || v > MAX_BYTES) {
        return Err(bad("Invalid provider archive size"));
    }
    progress.send_modify(|s| {
        s.version = Some(a.version.clone());
        s.total_bytes = size;
    });
    let mut file = tokio::fs::File::create(stage.join("download")).await?;
    let mut n = 0;
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    loop {
        let part = tokio::select! {v=response.chunk()=>v?,_=cancel.cancelled()=>return Err(bad("Cancelled"))};
        let Some(bytes) = part else { break };
        n += bytes.len() as u64;
        if n > size.unwrap_or(MAX_BYTES) {
            return Err(bad("Excess provider archive bytes"));
        }
        if a.sha256.is_some() {
            sha256.update(&bytes)
        } else {
            sha512.update(&bytes)
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
        progress.send_modify(|s| s.downloaded_bytes = n);
    }
    file.sync_all().await?;
    drop(file);
    let observed = if a.sha256.is_some() {
        format!("{:x}", sha256.finalize())
    } else {
        format!("{:x}", sha512.finalize())
    };
    if observed != hash || size.is_some_and(|s| s != n) {
        return Err(bad(
            "Provider archive size or checksum differs from the release",
        ));
    }
    progress.send_modify(|s| s.phase = "extracting".into());
    let root = stage.to_owned();
    let token = cancel.clone();
    let only = driver == "antigravity";
    tokio::task::spawn_blocking(move || extract(&root, only, &token))
        .await
        .map_err(|e| bad(&e.to_string()))??;
    progress.send_modify(|s| s.phase = "verifying".into());
    let record = Record {
        driver: driver.into(),
        version: a.version,
        target: a.target,
        digest: hash,
    };
    let check = verify(stage, stage, &record);
    tokio::select! {r=tokio::time::timeout(std::time::Duration::from_secs(90),check)=>r.map_err(|_|bad("Provider verification timed out"))??,_=cancel.cancelled()=>return Err(bad("Cancelled"))}
    Ok(record)
}
fn extract(stage: &Path, single: bool, cancel: &CancellationToken) -> Result<()> {
    let root = stage.join("payload");
    fs::create_dir(&root)?;
    let input = File::open(stage.join("download"))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(input));
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in archive.entries()? {
        if cancel.is_cancelled() {
            return Err(bad("Cancelled"));
        }
        count += 1;
        let mut entry = entry?;
        let path = entry.path()?;
        let kind = entry.header().entry_type();
        if count > 10000
            || path.as_os_str().is_empty()
            || path.to_string_lossy().contains(['\\', ':'])
            || path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || (!kind.is_file() && !kind.is_dir())
            || single && (path.as_ref() != Path::new("antigravity") || !kind.is_file())
        {
            return Err(bad("Unexpected provider archive entry"));
        }
        let dest = root.join(path);
        if kind.is_dir() {
            fs::create_dir_all(dest)?;
            continue;
        }
        total = total
            .checked_add(entry.size())
            .filter(|n| *n <= (4u64 << 30))
            .ok_or_else(|| bad("Expanded provider archive exceeds limit"))?;
        fs::create_dir_all(dest.parent().unwrap())?;
        let mut f = File::create_new(&dest)?;
        let mode = entry.header().mode()?;
        let mut buf = [0; 65536];
        loop {
            if cancel.is_cancelled() {
                return Err(bad("Cancelled"));
            }
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n])?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                dest,
                fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }),
            )?;
        }
    }
    if count == 0 {
        return Err(bad("Empty provider archive"));
    }
    Ok(())
}
async fn verify(package: &Path, stage: &Path, r: &Record) -> Result<()> {
    let root = package.join("payload");
    let command = root.join(r.executable());
    let mut opts = crate::SessionOptions::new(stage);
    opts.remove_env("KAR_WORKBENCH_TOKEN")?;
    if r.driver == "codex" {
        let manifest: Value =
            serde_json::from_reader(File::open(root.join("codex-package.json"))?.take(65536))?;
        if manifest["layoutVersion"] != 1
            || manifest["target"] != r.target
            || manifest["version"] != r.version
            || manifest["entrypoint"] != "bin/codex"
        {
            return Err(bad("Codex package manifest does not match the release"));
        }
        for path in ["bin/codex", "bin/codex-code-mode-host", "codex-path/rg"] {
            let md = fs::symlink_metadata(root.join(path))?;
            if !md.is_file() || md.len() == 0 {
                return Err(bad("Incomplete Codex package"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if md.permissions().mode() & 0o111 == 0 {
                    return Err(bad("Codex helper is not executable"));
                }
            }
        }
        let out = opts
            .command(&command)?
            .arg("--version")
            .kill_on_drop(true)
            .output()
            .await?;
        if !out.status.success()
            || !String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .any(|s| s == r.version)
        {
            return Err(bad("Codex executable reported an unexpected version"));
        }
    } else {
        let mut cmd = opts.command(&command)?;
        cmd.args(["acp", "--process-per-session"]);
        cmd.env("HOME", stage)
            .env("XDG_CONFIG_HOME", stage.join("config"))
            .env("XDG_DATA_HOME", stage.join("data"));
        let mut wire = crate::account::AccountWire::open(cmd)?;
        let h = wire
            .initialize(crate::account::AccountWire::capabilities())
            .await?;
        if h["agentInfo"]["name"] != "Antigravity ACP Server"
            || h["agentInfo"]["version"] != r.version
            || !matches!(h["protocolVersion"].as_u64(), Some(1 | 2))
            || h["agentCapabilities"]["loadSession"] != true
            || h["agentCapabilities"]["sessionCapabilities"]["resume"].is_null()
            || h["agentCapabilities"]["auth"]["logout"].is_null()
            || !h["authMethods"]
                .as_array()
                .is_some_and(|a| a.iter().any(|m| m["id"] == "oauth-personal"))
        {
            return Err(bad(
                "Antigravity ACP identity or capabilities do not match the release",
            ));
        }
    }
    Ok(())
}
