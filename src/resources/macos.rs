use super::{Process, Table};
use libproc::{
    proc_pid::{kproc_info_raw, pidinfo},
    processes::{ProcFilter, pids_by_type},
    task_info::TaskAllInfo,
};
use nix::{
    errno::Errno,
    unistd::{Pid, getpgid},
};

fn stamp(sec: u64, usec: u64) -> Option<u64> {
    sec.checked_mul(1_000_000)?.checked_add(usec)
}
pub(super) fn birth(pid: u32) -> Option<u64> {
    let info = pidinfo::<TaskAllInfo>(pid.try_into().ok()?, 0).ok()?;
    if info.pbsd.pbi_status == nix::libc::SZOMB {
        return None;
    }
    stamp(info.pbsd.pbi_start_tvsec, info.pbsd.pbi_start_tvusec)
}
#[allow(dead_code)]
pub(crate) fn pgid_of(pid: u32) -> Option<u32> {
    let raw: i32 = pid.try_into().ok()?;
    if let Ok(info) = pidinfo::<TaskAllInfo>(raw, 0) {
        return (info.pbsd.pbi_status != nix::libc::SZOMB).then_some(info.pbsd.pbi_pgid);
    }
    if let Ok(info) = kproc_info_raw(raw) {
        let zombie = u32::try_from(info.kp_proc.p_stat).ok()? == nix::libc::SZOMB;
        return (!zombie)
            .then(|| info.kp_eproc.e_pgid.try_into().ok())
            .flatten();
    }
    getpgid(Some(Pid::from_raw(raw)))
        .ok()
        .and_then(|p| u32::try_from(p.as_raw()).ok())
}
pub(super) fn table() -> Option<Table> {
    let mut rows = Table::new();
    for pid in pids_by_type(ProcFilter::All).ok()? {
        if pid == 0 {
            continue;
        }
        let raw: i32 = pid.try_into().ok()?;
        let row = if let Ok(info) = pidinfo::<TaskAllInfo>(raw, 0) {
            Process {
                parent: info.pbsd.pbi_ppid,
                group: info.pbsd.pbi_pgid,
                born: stamp(info.pbsd.pbi_start_tvsec, info.pbsd.pbi_start_tvusec)?,
                rss: Some(info.ptinfo.pti_resident_size),
                zombie: info.pbsd.pbi_status == nix::libc::SZOMB,
            }
        } else if let Ok(info) = kproc_info_raw(raw) {
            // Protected processes still expose their lineage. If one belongs to
            // the measured tree, the missing RSS makes that reading unavailable.
            let birth = info.kp_proc.p_un;
            Process {
                parent: info.kp_eproc.e_ppid.try_into().ok()?,
                group: info.kp_eproc.e_pgid.try_into().ok()?,
                born: stamp(
                    birth.tv_sec.try_into().ok()?,
                    birth.tv_usec.try_into().ok()?,
                )?,
                rss: None,
                zombie: u32::try_from(info.kp_proc.p_stat).ok()? == nix::libc::SZOMB,
            }
        } else if getpgid(Some(Pid::from_raw(raw))) == Err(Errno::ESRCH) {
            continue;
        } else {
            return None;
        };
        rows.insert(pid, row);
    }
    Some(rows)
}
