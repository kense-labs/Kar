//! Point-in-time process-tree memory observations, independent of the session journal.
pub(crate) mod turn;
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceScope {
    SharedProcess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResources {
    /// Resident bytes summed across the processes, including shared pages in each RSS.
    pub rss: u64,
    pub processes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ResourceScope>,
}

#[derive(Clone)]
pub(crate) struct Root {
    pid: u32,
    born: u64,
    scope: Option<ResourceScope>,
    ownership: Option<Shared<BoxFuture<'static, bool>>>,
}
#[derive(Clone, Copy)]
struct Process {
    parent: u32,
    group: u32,
    born: u64,
    rss: Option<u64>,
    zombie: bool,
}
type Table = HashMap<u32, Process>;
type Reading = Shared<BoxFuture<'static, Option<Arc<Table>>>>;
static IN_FLIGHT: Mutex<Option<Reading>> = Mutex::new(None);

#[cfg(any(target_os = "linux", all(test, unix)))]
#[path = "resources/linux.rs"]
mod procfs;
#[cfg(target_os = "linux")]
use procfs as os;
#[cfg(target_os = "macos")]
#[path = "resources/macos.rs"]
mod os;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    pub(super) fn birth(_: u32) -> Option<u64> {
        None
    }
    pub(super) fn table() -> Option<super::Table> {
        None
    }
}
impl Root {
    pub(crate) fn new(pid: Option<u32>, scope: Option<ResourceScope>) -> Option<Self> {
        let pid = pid?;
        Some(Self {
            pid,
            born: os::birth(pid)?,
            scope,
            ownership: None,
        })
    }
    pub(crate) fn confirm_with(
        mut self,
        proof: impl std::future::Future<Output = bool> + Send + 'static,
    ) -> Self {
        self.ownership = Some(proof.boxed().shared());
        self
    }
    pub(crate) fn shared(&self) -> bool {
        self.scope.is_some()
    }
    pub(crate) fn shares_process_with(&self, other: &Self) -> bool {
        self.shared() && other.shared() && self.pid == other.pid && self.born == other.born
    }
    /// A controller with a stalled pipe may be unable to process its close message. Only
    /// signal a group created for this session, whose original leader is still present.
    pub(crate) fn stop_dedicated_group(&self) -> crate::Result<bool> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use nix::{
                errno::Errno,
                sys::signal::{Signal, killpg},
                unistd::{Pid, getpgid},
            };
            if self.shared()
                || self.ownership.is_some()
                || self.pid <= 1
                || self.pid > i32::MAX as u32
            {
                return Ok(false);
            }
            let pid = Pid::from_raw(self.pid as i32);
            if getpgid(Some(pid)).ok() != Some(pid) || os::birth(self.pid) != Some(self.born) {
                return Ok(false);
            }
            match killpg(pid, Signal::SIGKILL) {
                Ok(()) => Ok(true),
                Err(Errno::ESRCH) => Ok(false),
                Err(e) => Err(std::io::Error::from(e).into()),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Ok(false)
    }
    pub(crate) async fn read(&self) -> Option<SessionResources> {
        if let Some(proof) = &self.ownership
            && !tokio::time::timeout(std::time::Duration::from_secs(1), proof.clone())
                .await
                .ok()?
        {
            return None;
        }
        let table = snapshot().await?;
        self.measure(&table)
    }
    fn measure(&self, table: &Table) -> Option<SessionResources> {
        let root = table.get(&self.pid)?;
        if root.born != self.born || root.zombie {
            return None;
        }
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut members = HashSet::new();
        for (&pid, p) in table {
            if p.group == self.pid {
                members.insert(pid);
            }
            children.entry(p.parent).or_default().push(pid);
        }
        let mut descendants = HashSet::from([self.pid]);
        let mut pending = vec![self.pid];
        while let Some(parent) = pending.pop() {
            for &pid in children.get(&parent).into_iter().flatten() {
                if descendants.insert(pid) {
                    pending.push(pid);
                }
            }
        }
        members.extend(descendants);
        let rss = members
            .iter()
            .try_fold(0u64, |sum, pid| sum.checked_add(table.get(pid)?.rss?))?;
        Some(SessionResources {
            rss,
            processes: members.len(),
            scope: self.scope,
        })
    }
}

async fn snapshot() -> Option<Arc<Table>> {
    let reading = {
        let mut current = IN_FLIGHT.lock().unwrap();
        current
            .get_or_insert_with(|| {
                let (done, wait) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    let table = os::table().map(Arc::new);
                    IN_FLIGHT.lock().unwrap().take();
                    let _ = done.send(table);
                });
                async move { wait.await.ok().flatten() }.boxed().shared()
            })
            .clone()
    };
    reading.await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_the_led_group_and_descendants_exactly_once() {
        let root = Root {
            pid: 10,
            born: 100,
            scope: Some(ResourceScope::SharedProcess),
            ownership: None,
        };
        let process = |parent, group| Process {
            parent,
            group,
            born: 100,
            rss: Some(11),
            zombie: false,
        };
        let mut table = Table::from([
            (10, process(1, 10)),
            (11, process(10, 10)),
            (12, process(11, 12)),
            (13, process(1, 10)),
            (14, process(1, 14)),
        ]);
        assert_eq!(
            root.measure(&table),
            Some(SessionResources {
                rss: 44,
                processes: 4,
                scope: root.scope
            })
        );
        table.get_mut(&12).unwrap().rss = None;
        assert_eq!(root.measure(&table), None);
        table.remove(&12);
        table.get_mut(&10).unwrap().born += 1;
        assert_eq!(root.measure(&table), None);
        table.remove(&10);
        assert_eq!(root.measure(&table), None);
    }
}
