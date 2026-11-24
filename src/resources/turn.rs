//! Identify the processes of one Codex turn from three independent OS readings.
use super::{Process, Table, os};
use crate::ProcessCleanupResult;
use futures_util::{FutureExt, future::BoxFuture, future::Shared};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

type Reading = Result<Arc<Table>, String>;
#[derive(Clone)]
pub(crate) struct Baseline(Shared<BoxFuture<'static, Reading>>);
pub(crate) struct Evidence {
    root: u32,
    baseline: Arc<Table>,
    snapshot: Arc<Table>,
}

pub(crate) fn failed(reason: impl Into<String>) -> ProcessCleanupResult {
    ProcessCleanupResult::Failed {
        reason: reason.into(),
        signalled: 0,
        foreign: 0,
    }
}

async fn read(label: &'static str, seconds: u64) -> Reading {
    let job = tokio::task::spawn_blocking(os::table);
    tokio::time::timeout(Duration::from_secs(seconds), job)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .map(Arc::new)
        .ok_or_else(|| format!("{label} process table unavailable within {seconds}s"))
}

pub(crate) fn baseline() -> Option<Baseline> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return None;
    }
    let task = tokio::spawn(read("baseline", 5));
    Some(Baseline(
        async move { task.await.map_err(|e| e.to_string())? }
            .boxed()
            .shared(),
    ))
}

impl Evidence {
    pub(crate) async fn observe(
        root: Option<u32>,
        baseline: Option<Baseline>,
    ) -> Result<Self, String> {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Err("turn process groups cannot be identified on this platform".into());
        }
        let root = root.ok_or("app-server PID is unavailable")?;
        let baseline = baseline.ok_or("no baseline for this turn")?;
        let snapshot = read("pre-interrupt", 1).await?;
        let baseline = baseline
            .0
            .now_or_never()
            .ok_or("baseline read had not finished")??;
        let original = baseline
            .get(&root)
            .ok_or("app-server missing from baseline")?;
        if !snapshot.get(&root).is_some_and(|p| {
            p.born == original.born && !p.zombie && p.group > 1 && p.group == original.group
        }) {
            return Err("app-server identity or process group could not be confirmed".into());
        }
        Ok(Self {
            root,
            baseline,
            snapshot,
        })
    }

    pub(crate) async fn end(self) -> ProcessCleanupResult {
        let targets = self.targets();
        if targets.is_empty() {
            return ProcessCleanupResult::Completed {
                signalled: 0,
                foreign: 0,
            };
        }
        let fresh = match read("cleanup", 5).await {
            Ok(table) => table,
            Err(reason) => return failed(reason),
        };
        tokio::task::spawn_blocking(move || self.end_in(&targets, &fresh))
            .await
            .unwrap_or_else(|e| failed(e.to_string()))
    }

    fn targets(&self) -> Vec<(u32, u64)> {
        let group = self.snapshot[&self.root].group;
        let mut targets = Vec::new();
        walk(&self.snapshot, &[self.root], |pid, p| {
            let earlier = same(&self.baseline, pid, p.born);
            if earlier && p.group != group {
                return false; // Spare the subtree of an older command or MCP server.
            }
            if pid != self.root && p.group != group && !earlier && !p.zombie {
                targets.push((pid, p.born));
            }
            true
        });
        targets
    }

    fn end_in(&self, targets: &[(u32, u64)], fresh: &Table) -> ProcessCleanupResult {
        let host = std::process::id();
        let Some(host_group) = fresh.get(&host).map(|p| p.group) else {
            return failed("host missing from cleanup process table");
        };
        let root = &self.snapshot[&self.root];
        if !fresh
            .get(&self.root)
            .is_some_and(|row| row.born == root.born && row.group == root.group && !row.zombie)
        {
            return failed("app-server identity or process group changed before cleanup");
        }
        let starts: Vec<_> = targets
            .iter()
            .filter(|&&(pid, born)| same(fresh, pid, born))
            .map(|&(pid, _)| pid)
            .collect();
        let mut members = HashMap::new();
        walk(fresh, &starts, |pid, p| {
            if pid <= 1
                || pid == host
                || pid == self.root
                || p.group == root.group
                || same(&self.baseline, pid, p.born)
            {
                return false;
            }
            if !p.zombie {
                members.insert(pid, *p);
            }
            true
        });
        signal(&members, fresh, host_group)
    }
}

fn same(table: &Table, pid: u32, born: u64) -> bool {
    table.get(&pid).is_some_and(|p| p.born == born)
}

fn walk(table: &Table, starts: &[u32], mut descend: impl FnMut(u32, &Process) -> bool) {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, p) in table {
        children.entry(p.parent).or_default().push(pid);
    }
    let mut stack = starts.to_vec();
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if seen.insert(pid)
            && let Some(p) = table.get(&pid)
            && descend(pid, p)
            && let Some(kids) = children.get(&pid)
        {
            stack.extend(kids);
        }
    }
}

#[cfg(unix)]
fn signal(members: &HashMap<u32, Process>, fresh: &Table, host_group: u32) -> ProcessCleanupResult {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    let mut groups: HashSet<_> = members.values().map(|p| p.group).collect();
    for (&pid, p) in fresh {
        if !members.contains_key(&pid) {
            groups.remove(&p.group);
        }
    }
    groups.retain(|group| {
        *group > 1
            && *group != host_group
            && members.get(group).is_some_and(|leader| {
                leader.group == *group && os::birth(*group) == Some(leader.born)
            })
            && killpg(Pid::from_raw(*group as i32), Signal::SIGKILL).is_ok()
    });
    let mut signalled = 0;
    let mut foreign = 0;
    let mut errors = Vec::new();
    for (&pid, p) in members {
        match signal_identity(pid, p.born) {
            Ok(()) => signalled += 1,
            Err(Errno::ESRCH) if groups.contains(&p.group) => signalled += 1,
            Err(Errno::ESRCH) => {}
            Err(Errno::EPERM) => foreign += 1,
            Err(e) => errors.push(format!("PID {pid}: {e}")),
        }
    }
    if errors.is_empty() {
        ProcessCleanupResult::Completed { signalled, foreign }
    } else {
        ProcessCleanupResult::Failed {
            reason: errors.join(", "),
            signalled,
            foreign,
        }
    }
}

#[cfg(unix)]
fn signal_identity(pid: u32, born: u64) -> Result<(), nix::errno::Errno> {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    let native = Pid::from_raw(pid as i32);
    #[cfg(target_os = "linux")]
    let handle = {
        use rustix::process::{PidfdFlags, pidfd_open};
        let native = rustix::process::Pid::from_raw(native.as_raw()).ok_or(Errno::EINVAL)?;
        match pidfd_open(native, PidfdFlags::empty()) {
            Ok(fd) => Some(fd),
            Err(rustix::io::Errno::NOSYS) => None,
            Err(e) => return Err(Errno::from_raw(e.raw_os_error())),
        }
    };
    match os::birth(pid) {
        Some(found) if found == born => {}
        Some(_) => return Err(Errno::ESRCH),
        None => {
            kill(native, None)?;
            return Err(Errno::EAGAIN);
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(handle) = handle {
        return rustix::process::pidfd_send_signal(handle, rustix::process::Signal::KILL)
            .map_err(|e| Errno::from_raw(e.raw_os_error()));
    }
    kill(native, Signal::SIGKILL)
}

#[cfg(not(unix))]
fn signal(_: &HashMap<u32, Process>, _: &Table, _: u32) -> ProcessCleanupResult {
    failed("turn process groups cannot be identified on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_excludes_older_subtrees_and_identifies_reused_pids() {
        let p = |parent, group, born| Process {
            parent,
            group,
            born,
            rss: None,
            zombie: false,
        };
        let baseline = Table::from([
            (10, p(1, 10, 1)),
            (15, p(10, 10, 1)),
            (21, p(15, 21, 1)),
            (30, p(15, 30, 1)),
            (42, p(1, 42, 1)),
        ]);
        let mut snapshot = baseline.clone();
        snapshot.extend([
            (22, p(21, 22, 2)),
            (31, p(30, 10, 2)),
            (32, p(31, 32, 2)),
            (41, p(15, 10, 2)),
            (42, p(15, 42, 3)),
            (43, p(42, 43, 3)),
            (44, p(1, 44, 3)),
        ]);
        let e = Evidence {
            root: 10,
            baseline: Arc::new(baseline),
            snapshot: Arc::new(snapshot),
        };
        let mut result = e.targets();
        result.sort();
        assert_eq!(result, vec![(42, 3), (43, 3)]);
        assert!(matches!(
            e.end_in(&result, &Table::new()),
            ProcessCleanupResult::Failed { signalled: 0, .. }
        ));
    }
}
