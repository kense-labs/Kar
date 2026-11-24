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
        detail: Some(raw.chars().take(256).collect()),
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
    let checkout = staging.path().join("checkout");
    if *stop.borrow() {
        return Err(cancellation());
    }
    let mut command = crate::workbench_git::command(staging.path());
    command
        .args(["clone", "--progress", "--"])
        .arg(&job.input.source)
        .arg(&checkout)
        .env_remove("KAR_WORKBENCH_TOKEN")
        .env("LC_ALL", "C")
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
    loop {
        tokio::select! {
            changed=stop.changed()=>{if changed.is_err()||*stop.borrow(){child.signal_terminal(true).map_err(Error::Io)?;let _=child.wait().await;return Err(cancellation())}}
            read=stderr.read(&mut buf),if !eof=>{
                let n=read.map_err(Error::Io)?;
                if n==0 {eof=true;}
                for b in &buf[..n]{
                    if matches!(*b,b'\r'|b'\n'){
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
                if dirty&&saved.elapsed()>=Duration::from_millis(200){save(app,job,&pulse)?;saved=Instant::now();dirty=false;}
                if child.exited().map_err(Error::Io)?{break}
            }
        }
    }
    let status = child.wait().await.map_err(Error::Io)?;
    if !status.success() {
        return Err(ForgeError::remote(
            502,
            format!("git clone exited with {status}"),
        ));
    }
    if *stop.borrow() {
        return Err(cancellation());
    }
    if !checkout.join(".git").is_dir() {
        return Err(ForgeError::remote(502, "clone did not produce a checkout"));
    }
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
