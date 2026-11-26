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

pub(super) fn parse_pmset_batt(output: &str) -> (&'static str, Option<bool>, Option<u8>) {
    let on_ac = output.contains("AC Power");
    let on_batt = output.contains("Battery Power");
    let source = if on_ac {
        "ac"
    } else if on_batt {
        "battery"
    } else {
        "unknown"
    };
    let on_battery = if on_ac {
        Some(false)
    } else if on_batt {
        Some(true)
    } else {
        None
    };
    let percent = output.lines().find_map(|line| {
        let idx = line.find('%')?;
        let prefix = &line[..idx];
        let digits: String = prefix
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        digits.parse::<u8>().ok().filter(|p| *p <= 100)
    });
    (source, on_battery, percent)
}

pub(super) fn power(at: i64) -> Value {
    let batt_out = std::process::Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .env_clear()
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|r| r.status.success())
        .map(|r| String::from_utf8_lossy(&r.stdout).into_owned())
        .unwrap_or_default();
    let (source, on_battery, battery_percent) = parse_pmset_batt(&batt_out);

    let low_power = std::process::Command::new("/usr/bin/pmset")
        .arg("-g")
        .env_clear()
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| {
            let text = String::from_utf8_lossy(&r.stdout);
            text.lines().find_map(|l| {
                let t = l.trim();
                let val = t.strip_prefix("lowpowermode")?.trim();
                match val {
                    "1" => Some("enabled"),
                    "0" => Some("disabled"),
                    _ => None,
                }
            })
        })
        .unwrap_or("unknown");

    let idle_seconds = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4"])
        .env_clear()
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| {
            let text = String::from_utf8_lossy(&r.stdout);
            text.lines().find_map(|l| {
                let (_, rhs) = l.split_once("\"HIDIdleTime\"")?;
                let (_, num) = rhs.split_once('=')?;
                let ns: u64 = num.trim().parse().ok()?;
                Some(ns / 1_000_000_000)
            })
        });

    let thermal_state = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.thermal_level"])
        .env_clear()
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| {
            String::from_utf8_lossy(&r.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .map(|lvl| match lvl {
            0 => "nominal",
            1..=39 => "fair",
            40..=79 => "serious",
            _ => "critical",
        })
        .unwrap_or("nominal");

    let idle_state = match idle_seconds {
        Some(s) if s >= 300 => "idle",
        Some(_) => "active",
        None => "unknown",
    };

    json!({
        "source": source,
        "idle": idle_state,
        "idle_seconds": idle_seconds,
        "locked": "unknown",
        "suspended": false,
        "on_battery": match on_battery {
            Some(true) => json!(true),
            Some(false) => json!(false),
            None => json!("unknown"),
        },
        "battery_percent": battery_percent,
        "low_power_mode": low_power,
        "thermal_state": thermal_state,
        "stale": source == "unknown" && idle_seconds.is_none(),
        "updated_at": stamp(at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pmset_battery_and_ac_output() {
        let ac = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=123)\t98%; charged; 0:00 remaining present: true";
        assert_eq!(parse_pmset_batt(ac), ("ac", Some(false), Some(98)));
        let batt = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=123)\t42%; discharging; 3:15 remaining";
        assert_eq!(parse_pmset_batt(batt), ("battery", Some(true), Some(42)));
    }
}
