//! Discover listeners only in the designated PTY shell's process tree.
use super::{Port, ProcessId};
use crate::{Error, Result};
use std::{collections::BTreeSet, net::IpAddr};

#[cfg(target_os = "macos")]
use crate::resources::tree::descendants;
pub(super) use crate::resources::tree::identity;

#[cfg(target_os = "macos")]
pub(super) fn stop_tree(root: ProcessId) {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    for child in descendants(root).into_iter().rev() {
        if identity(child.pid) == Some(child) {
            let _ = kill(Pid::from_raw(child.pid as i32), Signal::SIGKILL);
        }
    }
}
#[cfg(not(target_os = "macos"))]
pub(super) fn stop_tree(_root: ProcessId) {}

#[cfg(target_os = "macos")]
pub(super) fn ports(root: ProcessId) -> Result<Vec<Port>> {
    let processes = descendants(root);
    if processes.is_empty() {
        return Ok(vec![]);
    }
    let pids = processes
        .iter()
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // lsof exposes the local address of the listening descriptor. This is an
    // OS socket table query on the authorized PIDs, not a network probe.
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &pids, "-iTCP", "-sTCP:LISTEN", "-Fptn"])
        .env_remove("KAR_WORKBENCH_TOKEN")
        .output()?;
    if !out.status.success() && (out.status.code() != Some(1) || !out.stderr.is_empty()) {
        return Err(Error::Protocol(
            "unable to inspect terminal listeners".into(),
        ));
    }
    let mut listeners = BTreeSet::new();
    let mut owner = None;
    let mut ipv6 = false;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(id) = line.strip_prefix('p') {
            owner = id
                .parse::<u32>()
                .ok()
                .and_then(|id| processes.iter().find(|p| p.pid == id))
                .copied();
        } else if let Some(family) = line.strip_prefix('t') {
            ipv6 = family == "IPv6";
        } else if let Some(value) = line.strip_prefix('n')
            && let Some(owner) = owner
        {
            let Some((address, port)) = value.rsplit_once(':') else {
                continue;
            };
            let Ok(port) = port.parse::<u16>() else {
                continue;
            };
            let ip = address.trim_matches(['[', ']']);
            let address = match ip.parse::<IpAddr>() {
                Ok(addr) if addr.is_loopback() => addr,
                Ok(addr) if addr.is_unspecified() => {
                    if addr.is_ipv6() {
                        IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                    } else {
                        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                    }
                }
                _ if ip == "*" => {
                    if ipv6 {
                        IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                    } else {
                        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                    }
                }
                _ => continue,
            };
            if identity(owner.pid) == Some(owner) {
                listeners.insert(Port {
                    owner,
                    address,
                    port,
                });
            }
        }
    }
    Ok(listeners.into_iter().collect())
}
#[cfg(not(target_os = "macos"))]
pub(super) fn ports(_root: ProcessId) -> Result<Vec<Port>> {
    Err(Error::Unsupported(
        "terminal listener discovery on this platform".into(),
    ))
}

/// The accepted socket must also belong to this terminal tree. Rechecking
/// after connect closes the gap if another process has rebound the port.
#[cfg(target_os = "macos")]
pub(super) fn owns_peer(
    root: ProcessId,
    target: Port,
    client: std::net::SocketAddr,
) -> Result<bool> {
    let processes = descendants(root);
    if !processes.contains(&target.owner) {
        return Ok(false);
    }
    let pids = processes
        .iter()
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args([
            "-nP",
            "-a",
            "-p",
            &pids,
            "-iTCP",
            "-sTCP:ESTABLISHED",
            "-Fpn",
        ])
        .env_remove("KAR_WORKBENCH_TOKEN")
        .output()?;
    let mut owner = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(pid) = line.strip_prefix('p') {
            owner = pid
                .parse::<u32>()
                .ok()
                .and_then(|id| processes.iter().find(|p| p.pid == id))
                .copied();
        }
        if let Some(pair) = line.strip_prefix('n')
            && let Some((a, b)) = pair.split_once("->")
            && let (Ok(a), Ok(b)) = (
                a.parse::<std::net::SocketAddr>(),
                b.parse::<std::net::SocketAddr>(),
            )
            && a.port() == target.port
            && a.ip().to_canonical().is_loopback()
            && b.port() == client.port()
            && b.ip().to_canonical() == client.ip().to_canonical()
            && owner.is_some_and(|p| identity(p.pid) == Some(p))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(not(target_os = "macos"))]
pub(super) fn owns_peer(
    _root: ProcessId,
    _target: Port,
    _client: std::net::SocketAddr,
) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
pub(super) fn stop_listener(root: ProcessId, pid: u32, port: u16, born: Option<u64>) -> Result<()> {
    use libproc::processes::{ProcFilter, pids_by_type};
    use nix::{
        errno::Errno,
        sys::signal::{Signal, killpg},
        unistd::{Pid, getpgid},
    };
    use std::time::{Duration, Instant};
    let not_found = || {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "preview listener not found",
        ))
    };
    let shared = || {
        Error::Refused(
            "shared-group: the listener does not have an independent terminal job".into(),
        )
    };
    let failed =
        || Error::Refused("signal-failed: could not inspect or signal the preview job".into());
    let owner = ports(root)?
        .into_iter()
        .find(|target| {
            target.owner.pid == pid
                && target.port == port
                && born.is_none_or(|b| target.owner.born == b)
        })
        .map(|t| t.owner)
        .ok_or_else(not_found)?;
    let pid = Pid::from_raw(owner.pid as i32);
    let group = match getpgid(Some(pid)) {
        Ok(group) => group,
        Err(Errno::ESRCH) => return Ok(()),
        Err(_) => return Err(failed()),
    };
    if group.as_raw() <= 1 || getpgid(Some(Pid::from_raw(root.pid as i32))) == Ok(group) {
        return Err(shared());
    }
    let members = || -> Result<Vec<ProcessId>> {
        let list = pids_by_type(ProcFilter::ByProgramGroup {
            pgrpid: group.as_raw() as u32,
        })
        .map_err(|_| failed())?;
        // Require an identity for every extant group member. A vanished process cannot receive a signal.
        let mut found = vec![];
        for id in list {
            if let Some(key) = identity(id) {
                found.push(key);
            } else if getpgid(Some(Pid::from_raw(id as i32))) == Ok(group) {
                return Err(failed());
            }
        }
        Ok(found)
    };
    let before = members()?;
    let allowed = descendants(root);
    if before.iter().any(|p| !allowed.contains(p) || *p == root) {
        return Err(shared());
    }
    if !before.contains(&owner)
        || identity(owner.pid) != Some(owner)
        || getpgid(Some(pid)) != Ok(group)
    {
        return Err(not_found());
    }
    let signal = |sig| match killpg(group, sig) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => Ok(()),
        Err(_) => Err(failed()),
    };
    signal(Signal::SIGTERM)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let now = members()?;
        if !now.iter().any(|p| before.contains(p)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let allowed = descendants(root);
            if now
                .iter()
                .any(|p| !before.contains(p) && !allowed.contains(p))
            {
                return Err(shared());
            }
            return signal(Signal::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(125));
    }
}
#[cfg(not(target_os = "macos"))]
pub(super) fn stop_listener(_: ProcessId, _: u32, _: u16, _: Option<u64>) -> Result<()> {
    Err(Error::Unsupported(
        "stopping terminal listeners on this platform".into(),
    ))
}
