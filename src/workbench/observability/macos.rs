use super::*;
use crate::resources::tree::{descendants, identity};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub(super) fn host() -> Result<Value> {
    let mut s = System::new();
    s.refresh_cpu_usage();
    s.refresh_memory();
    // vm_stat's "free" value excludes speculative pages. Add those and inactive pages once.
    let available = std::process::Command::new("/usr/bin/vm_stat")
        .env_clear()
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| available_pages(&String::from_utf8_lossy(&r.stdout)))
        .unwrap_or_else(|| s.free_memory());
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    s.refresh_cpu_usage();
    let count = s.cpus().len();
    Ok(
        json!({"sampled_at":now(),"cpu_utilization":(count>0).then(||(s.global_cpu_usage()/100.).clamp(0.,1.)),
        "cpu_count":count,"available_memory_bytes":available,"total_memory_bytes":s.total_memory()}),
    )
}
fn available_pages(output: &str) -> Option<u64> {
    let page_size = output
        .lines()
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    let get = |key: &str| -> Option<u64> {
        output
            .lines()
            .find_map(|l| l.strip_prefix(key))?
            .trim()
            .trim_end_matches('.')
            .parse()
            .ok()
    };
    get("Pages free:")?
        .checked_add(get("Pages inactive:")?)?
        .checked_add(get("Pages speculative:")?)?
        .checked_mul(page_size)
}
pub(super) fn sample(
    s: &mut System,
    category: &BTreeMap<ProcessId, &'static str>,
    at: i64,
    prime: bool,
) -> Result<(usize, Vec<Row>)> {
    let root = identity(std::process::id())
        .ok_or_else(|| Error::Closed("host process identity unavailable".into()))?;
    let tree = descendants(root);
    let ids = tree
        .iter()
        .map(|id| Pid::from_u32(id.pid))
        .collect::<Vec<_>>();
    // A long-lived host should not retain old command arguments or CPU entries after a child exits.
    let selected = ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if s.processes().keys().any(|pid| !selected.contains(pid)) {
        *s = System::new();
    }
    let kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .with_cmd(UpdateKind::Always)
        .without_tasks();
    if prime {
        s.refresh_processes_specifics(ProcessesToUpdate::Some(&ids), true, kind);
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    }
    s.refresh_processes_specifics(ProcessesToUpdate::Some(&ids), true, kind);
    let mut rows = Vec::new();
    let mut depths = BTreeMap::new();
    let secret = std::env::var("KAR_WORKBENCH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .into_iter()
        .collect::<Vec<_>>();
    for id in &tree {
        let Some(p) = s.process(Pid::from_u32(id.pid)) else {
            continue;
        };
        if identity(id.pid) != Some(*id) {
            continue;
        }
        let ppid = p.parent().map_or(0, Pid::as_u32);
        let depth = if *id == root {
            0
        } else if let Some(n) = depths.get(&ppid) {
            n + 1
        } else {
            continue;
        };
        depths.insert(id.pid, depth);
        let name = p.name().to_string_lossy().into_owned();
        let args = p
            .cmd()
            .iter()
            .map(|w| w.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let command =
            crate::process::diagnostic::excerpt(&args, &secret).unwrap_or_else(|| name.clone());
        let io = p.disk_usage();
        let ts = stamp(at);
        rows.push(Row {
            key: Some(*id),
            pid: id.pid,
            start_time_ms: id.born / 1_000,
            ppid,
            depth,
            child_pids: vec![],
            name,
            command,
            status: p.status().to_string(),
            category: if *id == root {
                "server"
            } else {
                category.get(id).copied().unwrap_or("server-child")
            },
            cpu_percent: f64::from(p.cpu_usage()),
            cpu_time_ms: p.accumulated_cpu_time(),
            resident_bytes: p.memory(),
            peak_resident_bytes: p.memory(),
            virtual_bytes: p.virtual_memory(),
            io_read_bytes: io.total_read_bytes,
            io_write_bytes: io.total_written_bytes,
            io_semantics: "storage",
            io_read_bytes_per_second: 0.,
            io_write_bytes_per_second: 0.,
            run_time_ms: (at.max(0) as u64).saturating_sub(id.born / 1000),
            first_seen_at: ts.clone(),
            last_seen_at: ts,
        });
    }
    if !depths.contains_key(&root.pid) {
        return Err(Error::Closed("host process sample was incomplete".into()));
    }
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for r in &rows {
        children.entry(r.ppid).or_default().push(r.pid);
    }
    for r in &mut rows {
        r.child_pids = children.remove(&r.pid).unwrap_or_default();
    }
    rows.sort_by_key(|r| r.pid);
    Ok((ids.len(), rows))
}
