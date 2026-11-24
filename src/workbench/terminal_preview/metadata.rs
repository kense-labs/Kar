//! Project-scoped directory of visible terminals. Output bytes never enter this feed.
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) struct Metadata {
    pub epoch: Uuid,
    known: Mutex<BTreeMap<Uuid, Meta>>,
    pub changes: events::Events,
    #[cfg(target_os = "macos")]
    running: Mutex<Vec<std::sync::Weak<Terminal>>>,
}
impl Default for Metadata {
    fn default() -> Self {
        Self {
            epoch: Uuid::new_v4(),
            known: Mutex::default(),
            changes: Default::default(),
            #[cfg(target_os = "macos")]
            running: Mutex::default(),
        }
    }
}
impl Metadata {
    pub fn update(&self, m: &Meta) {
        let mut known = self.known.lock().unwrap();
        let data = if m.closed {
            if known.remove(&m.id).is_none() {
                return;
            }
            json!({"type":"remove","project":m.project,"id":m.id})
        } else {
            known.insert(m.id, m.clone());
            json!({"type":"upsert","terminal":m})
        };
        self.changes.push(m.project, None, data);
    }
    pub fn snapshot(&self, project: Uuid) -> (u64, Value) {
        let known = self.known.lock().unwrap();
        let seq = *self.changes.changed.borrow();
        (
            seq,
            json!({"type":"snapshot","manager_id":self.epoch,"terminals":known.values().filter(|m|m.project==project).collect::<Vec<_>>()}),
        )
    }
}

#[cfg(target_os = "macos")]
impl Metadata {
    pub fn track(&self, t: &Arc<Terminal>) {
        self.running.lock().unwrap().push(Arc::downgrade(t));
    }
    pub fn monitor(self: &Arc<Self>) {
        let source = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(directory) = source.upgrade() else {
                    break;
                };
                let live = {
                    let mut weak = directory.running.lock().unwrap();
                    let mut live = Vec::new();
                    weak.retain(|link| {
                        if let Some(t) = link.upgrade() {
                            let m = t.meta.lock().unwrap().clone();
                            if let Some(pid) = m.process
                                && !m.closed
                                && matches!(m.status, Status::Running)
                            {
                                live.push((t, m, pid));
                                return true;
                            }
                        }
                        false
                    });
                    live
                };
                for (term, meta, pid) in live {
                    let children = crate::resources::tree::descendants(pid);
                    let Some((_, children)) = children.split_first() else {
                        continue;
                    };
                    let busy = !children.is_empty();
                    let label = children
                        .iter()
                        .rev()
                        .find_map(|child| {
                            let name = libproc::proc_pid::name(child.pid as i32).ok()?;
                            (process::identity(child.pid) == Some(*child) && !name.is_empty())
                                .then(|| name.chars().take(128).collect::<String>())
                        })
                        .unwrap_or_else(|| {
                            meta.shell
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("Terminal")
                                .into()
                        });
                    if (meta.has_running_subprocess, meta.label.as_str()) != (busy, label.as_str())
                    {
                        let _ = term.update(|m| {
                            if m.process == Some(pid)
                                && !m.closed
                                && matches!(m.status, Status::Running)
                            {
                                m.has_running_subprocess = busy;
                                m.label = label;
                            }
                        });
                    }
                }
            }
        });
    }
}
