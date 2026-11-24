use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Stage {
    pub id: String,
    status: String,
    started_at: Option<String>,
    ended_at: Option<String>,
    percent: Option<u8>,
    detail: Option<String>,
    tail: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub thread_id: Uuid,
    pub phase: String,
    started_at: String,
    ended_at: Option<String>,
    pub(super) branch: String,
    base_ref: String,
    worktree_path: PathBuf,
    setup_script: Option<serde_json::Value>,
    stages: Vec<Stage>,
    error: Option<String>,
    pub sequence: u64,
}
fn now() -> String {
    jiff::Timestamp::now().to_string()
}
impl Snapshot {
    pub fn new(id: Uuid, branch: &str, base: &str, path: &Path) -> Self {
        let t = now();
        let stages = ["fetch", "checkout", "submodules", "setup-script", "agent"]
            .into_iter()
            .map(|id| Stage {
                id: id.into(),
                status: if id == "checkout" {
                    "running"
                } else {
                    "skipped"
                }
                .into(),
                started_at: (id == "checkout").then(|| t.clone()),
                ended_at: (id != "checkout").then(|| t.clone()),
                percent: None,
                detail: None,
                tail: vec![],
            })
            .collect();
        Self {
            thread_id: id,
            phase: "running".into(),
            started_at: t,
            ended_at: None,
            branch: branch.into(),
            base_ref: base.into(),
            worktree_path: path.into(),
            setup_script: None,
            stages,
            error: None,
            sequence: 1,
        }
    }
    pub fn advance(&mut self, detail: String, percent: u8) {
        self.sequence += 1;
        let s = &mut self.stages[1];
        s.percent = Some(percent);
        s.detail = Some(detail.chars().take(200).collect());
        s.tail.push(detail.chars().take(400).collect());
        if s.tail.len() > 24 {
            s.tail.remove(0);
        }
    }
    pub fn finish(&mut self, phase: &str, error: Option<&str>) {
        self.phase = phase.into();
        self.error = error.map(|s| s.chars().take(1000).collect());
        self.sequence += 1;
        let t = now();
        self.ended_at = Some(t.clone());
        let s = &mut self.stages[1];
        s.status = if phase == "done" {
            "done"
        } else if phase == "cancelled" {
            "skipped"
        } else {
            "failed"
        }
        .into();
        s.ended_at = Some(t);
        if phase == "done" {
            s.percent = Some(100);
        }
    }
}
pub(super) fn write<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, v)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub(super) struct Lifecycle {
    pub running: Mutex<Option<Arc<AtomicBool>>>,
    pub updates: watch::Sender<Option<Snapshot>>,
}
impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            running: Mutex::new(None),
            updates: watch::channel(None).0,
        }
    }
}
impl Lifecycle {
    pub fn begin(&self) -> Result<Arc<AtomicBool>> {
        let mut active = self.running.lock().unwrap();
        if active.is_some() {
            return Err(Error::Busy);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *active = Some(cancel.clone());
        Ok(cancel)
    }
}
