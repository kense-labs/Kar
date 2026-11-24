use super::*;
use crate::process::OwnedProcess;
use std::{io::Write, path::Path, process::Stdio, time::Instant};
use tokio::io::AsyncReadExt;

const RECEIPT: &str = ".git/kar-clone-attempt";
pub(super) fn has_receipt(job: &Job) -> bool {
    std::fs::read_to_string(job.input.dest.join(RECEIPT))
        .is_ok_and(|marker| marker == job.attempt.to_string())
}
fn stage_name(job: &Job) -> String {
    format!(".kar-clone-{}", job.attempt)
}
pub(super) fn reclaim_stage(app: &Workbench, job: &Job) {
    let Some(parent) = job.input.dest.parent() else {
        return;
    };
    if app.allowed(parent).is_err() || !parent.canonicalize().is_ok_and(|p| p == parent) {
        return;
    }
    let stage = parent.join(stage_name(job));
    if std::fs::symlink_metadata(&stage).is_ok_and(|m| m.is_dir())
        && std::fs::read_to_string(stage.join("kar-staging-owner"))
            .is_ok_and(|v| v == job.attempt.to_string())
    {
        let _ = std::fs::remove_dir_all(stage);
    }
}
fn cancellation() -> ForgeError {
    ForgeError::new("cancelled", 409, "clone cancelled")
}

#[derive(Default, PartialEq)]
struct Pulse {
    stage: String,
    percent: Option<u8>,
    detail: Option<String>,
    stalled: bool,
}
fn parse(bytes: &[u8]) -> Option<Pulse> {
    let raw = std::str::from_utf8(bytes).ok()?.trim();
    let (stage, _) = [
        ("counting", "remote: Enumerating objects"),
        ("counting", "remote: Counting objects"),
        ("counting", "remote: Compressing objects"),
        ("receiving", "Receiving objects"),
        ("resolving", "Resolving deltas"),
        ("checkout", "Updating files"),
        ("checkout", "Checking out files"),
        ("checkout", "Filtering content"),
    ]
    .into_iter()
    .find(|(_, p)| raw.starts_with(p))?;
    let percent = raw
        .split_once(": ")
        .and_then(|(_, v)| v.trim_start().split_once('%'))
        .and_then(|(n, _)| n.parse::<u8>().ok())
        .filter(|n| *n <= 100);
    Some(Pulse {
        stage: stage.into(),
        percent,
        detail: Some(percent.map_or_else(|| stage.to_string(), |n| format!("{stage} {n}%"))),
        stalled: false,
    })
}
fn save(app: &Workbench, job: &Job, pulse: &Pulse) -> Result<(), ForgeError> {
    app.update(|db| {
        let seq = next(&mut db.forge)?;
        let j = db
            .forge
            .clones
            .get_mut(&job.project)
            .filter(|v| v.attempt == job.attempt && v.phase == "running")
            .ok_or(Error::Busy)?;
        j.sequence = seq;
        j.stage = pulse.stage.clone();
        j.percent = pulse.percent;
        j.detail = pulse.detail.clone();
        j.stalled = pulse.stalled;
        Ok(())
    })?;
    notify(app);
    Ok(())
}
fn lfs_bytes(checkout: &Path) -> u64 {
    std::fs::read_dir(checkout.join(".git/lfs/incomplete"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|v| v.metadata().ok().filter(|m| m.is_file()).map(|m| m.len()))
        .fold(0, u64::saturating_add)
}
pub(super) async fn run(
    app: &Workbench,
    job: &Job,
    mut stop: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let dest = app.destination(&job.input.dest)?;
    let parent = dest
        .parent()
        .ok_or_else(|| ForgeError::bad("missing clone destination parent"))?;
    let staging = tempfile::Builder::new()
        .prefix(&stage_name(job))
        .rand_bytes(0)
        .tempdir_in(parent)
        .map_err(Error::Io)?;
    std::fs::write(
        staging.path().join("kar-staging-owner"),
        job.attempt.to_string(),
    )
    .map_err(Error::Io)?;
    let (checkout, selected) =
        download(Some((app, job)), &job.input, staging.path(), &mut stop).await?;
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(checkout.join(RECEIPT))
        .map_err(Error::Io)?;
    marker
        .write_all(job.attempt.to_string().as_bytes())
        .map_err(Error::Io)?;
    marker.sync_all().map_err(Error::Io)?;
    app.update(|db| {
        let seq = next(&mut db.forge)?;
        let j = db
            .forge
            .clones
            .get_mut(&job.project)
            .filter(|j| j.attempt == job.attempt && j.phase == "running")
            .ok_or(Error::Busy)?;
        if *stop.borrow() {
            return Err(Error::Busy);
        }
        let path = app.destination(&dest)?;
        if path != dest
            || !std::fs::symlink_metadata(&dest)?.is_dir()
            || std::fs::read_dir(&dest)?.next().is_some()
        {
            return Err(Error::Refused("clone destination was modified".into()));
        }
        std::fs::rename(&checkout, &dest)?;
        j.actual_url = Some(selected);
        j.input.credential_url = None;
        j.phase = "done".into();
        j.stage = "checkout".into();
        j.stalled = false;
        j.percent = Some(100);
        j.ended = Some(now());
        j.sequence = seq;
        Ok(())
    })
    .map_err(|e| {
        if *stop.borrow() {
            cancellation()
        } else {
            e.into()
        }
    })?;
    Ok(())
}

fn ssh_rejected(line: &[u8]) -> bool {
    let s = String::from_utf8_lossy(line).to_ascii_lowercase();
    s.contains("permission denied (publickey")
        || s.contains("host key verification failed")
        || s.contains("authentication failed")
}

pub(in crate::workbench::forge) async fn download(
    owner: Option<(&Workbench, &Job)>,
    input: &Prepared,
    staging: &Path,
    stop: &mut watch::Receiver<bool>,
) -> Result<(std::path::PathBuf, String), ForgeError> {
    let mut selected = &input.source;
    let mut checkout = staging.join("checkout");
    if *stop.borrow() {
        return Err(cancellation());
    }
    if let Err(e) = transfer(
        owner,
        staging,
        &checkout,
        selected,
        input.credential_url.as_deref(),
        stop,
    )
    .await
    {
        let Some(https) = input.fallback.as_ref().filter(|_| e.kind == "ssh_rejected") else {
            return Err(e);
        };
        if *stop.borrow() {
            return Err(cancellation());
        }
        selected = https;
        checkout = staging.join("checkout-https");
        if let Some((app, job)) = owner {
            app.update(|db| {
                let j = db
                    .forge
                    .clones
                    .get_mut(&job.project)
                    .filter(|j| j.attempt == job.attempt && j.phase == "running")
                    .ok_or(Error::Busy)?;
                j.actual_url = Some(https.clone());
                Ok(())
            })?;
            save(
                app,
                job,
                &Pulse {
                    stage: "connecting".into(),
                    detail: Some("SSH refused; trying HTTPS".into()),
                    ..Default::default()
                },
            )?;
        }
        transfer(owner, staging, &checkout, https, None, stop).await?;
    }
    if *stop.borrow() {
        return Err(cancellation());
    }
    if !checkout.join(".git").is_dir() {
        return Err(ForgeError::remote(502, "clone did not produce a checkout"));
    }
    Ok((checkout, selected.clone()))
}
async fn transfer(
    owner: Option<(&Workbench, &Job)>,
    staging: &Path,
    checkout: &Path,
    source: &str,
    credentials: Option<&str>,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let mut command = crate::workbench_git::command(staging);
    if let Some(raw) = credentials {
        // Separate keys and values also preserve "=" in a query or encoded password. Git records
        // only the redacted source as origin. Keep host-supplied Git config entries in order.
        let count = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .map(|n| n.parse::<u32>())
            .transpose()
            .map_err(|_| ForgeError::bad("invalid Git environment configuration"))?
            .unwrap_or_default();
        let next = count
            .checked_add(1)
            .ok_or_else(|| ForgeError::bad("invalid Git environment configuration"))?;
        command
            .env("GIT_CONFIG_COUNT", next.to_string())
            .env(
                format!("GIT_CONFIG_KEY_{count}"),
                format!("url.{raw}.insteadOf"),
            )
            .env(format!("GIT_CONFIG_VALUE_{count}"), source);
    }
    command
        .args(["clone", "--progress", "--"])
        .arg(source)
        .arg(checkout)
        .env_remove("KAR_WORKBENCH_TOKEN")
        .env("LANGUAGE", "C")
        .env("GIT_PROGRESS_DELAY", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::null());
    let mut child =
        OwnedProcess::spawn(tokio::process::Command::from(command)).map_err(Error::Io)?;
    let mut stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| ForgeError::remote(500, "git progress channel unavailable"))?;
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut last_active = Instant::now();
    let mut saved = last_active;
    let mut pulse = Pulse {
        stage: "connecting".into(),
        ..Default::default()
    };
    let mut dirty = false;
    let mut bytes_downloaded = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut eof = false;
    let mut rejected = false;
    loop {
        tokio::select! {
            changed=stop.changed()=>{if changed.is_err()||*stop.borrow(){child.signal_terminal(true).map_err(Error::Io)?;let _=child.wait().await;return Err(cancellation())}}
            read=stderr.read(&mut buf),if !eof=>{
                let n=read.map_err(Error::Io)?;
                if n==0 {eof=true;}
                for b in &buf[..n]{
                    if matches!(*b,b'\r'|b'\n'){
                        rejected|=ssh_rejected(&line);
                        if let Some(p)=parse(&line){last_active=Instant::now();dirty|=p!=pulse;pulse=p;}
                        line.clear();
                    }else if line.len()<2048{line.push(*b);}
                }
            }
            _=tick.tick()=>{
                let bytes=lfs_bytes(&checkout);
                if bytes!=bytes_downloaded&&bytes>0 {
                    last_active=Instant::now();pulse=Pulse{stage:"checkout".into(),percent:None,detail:Some(format!("Git LFS received {bytes} bytes")),stalled:false};dirty=true;
                }
                bytes_downloaded=bytes;
                if last_active.elapsed()>=Duration::from_secs(20)&&!pulse.stalled{pulse.stalled=true;dirty=true;}
                if dirty&&saved.elapsed()>=Duration::from_millis(200){if let Some((app,job))=owner{save(app,job,&pulse)?;}saved=Instant::now();dirty=false;}
                if eof && child.exited().map_err(Error::Io)?{break}
            }
        }
    }
    let status = child.wait().await.map_err(Error::Io)?;
    if status.success() {
        return Ok(());
    }
    if rejected || ssh_rejected(&line) {
        return Err(ForgeError::new(
            "ssh_rejected",
            403,
            "Git rejected SSH authentication or host verification",
        ));
    }
    Err(ForgeError::remote(
        502,
        format!("git clone exited with {status}"),
    ))
}
