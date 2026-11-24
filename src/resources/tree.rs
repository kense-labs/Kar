//! Birth-checked host process identities shared by terminal ownership and diagnostics.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub(crate) struct ProcessId {
    pub(crate) pid: u32,
    pub(crate) born: u64,
}
#[cfg(target_os = "macos")]
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
pub(crate) fn identity(pid: u32) -> Option<ProcessId> {
    Some(ProcessId {
        pid,
        born: super::os::birth(pid)?,
    })
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn identity(_pid: u32) -> Option<ProcessId> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn descendants(root: ProcessId) -> Vec<ProcessId> {
    use libproc::{
        proc_pid::pidinfo,
        processes::{ProcFilter, pids_by_type},
        task_info::TaskAllInfo,
    };
    if identity(root.pid) != Some(root) {
        return vec![];
    }
    let mut seen = BTreeSet::from([root.pid]);
    let mut list = vec![root];
    let mut pos = 0;
    while pos < list.len() {
        let parent = list[pos];
        pos += 1;
        for pid in
            pids_by_type(ProcFilter::ByParentProcess { ppid: parent.pid }).unwrap_or_default()
        {
            if seen.contains(&pid) {
                continue;
            }
            let Ok(info) = pidinfo::<TaskAllInfo>(pid as i32, 0) else {
                continue;
            };
            if info.pbsd.pbi_ppid != parent.pid {
                continue;
            }
            if let Some(child) = identity(pid)
                && child.born >= parent.born
                && identity(parent.pid) == Some(parent)
            {
                seen.insert(pid);
                list.push(child);
            }
        }
    }
    list
}
