use super::*;
use serde::Deserialize;
#[derive(Deserialize)]
pub(super) struct Window {
    pub window_ms: u64,
    pub bucket_ms: u64,
}
#[derive(Default)]
struct Bucket {
    count: u64,
    cpu_sum: f64,
    cpu_max: f64,
    rss: u64,
    processes: usize,
    read: u64,
    written: u64,
}
struct Summary<'a> {
    first: &'a Row,
    latest: &'a Row,
    samples: u64,
    cpu_sum: f64,
    cpu_max: f64,
    rss_peak: u64,
    cpu_time: u64,
    read: u64,
    written: u64,
}
pub(super) fn build(
    frames: &VecDeque<Arc<Frame>>,
    at: i64,
    q: Window,
    interval: u64,
    legacy: bool,
) -> Value {
    let window = q.window_ms.clamp(1_000, 3_600_000) as i64;
    let width = q.bucket_ms.clamp(1_000, window as u64) as i64;
    let start = at - window;
    let count = ((window + width - 1) / width) as usize;
    let mut buckets = (0..count).map(|_| Bucket::default()).collect::<Vec<_>>();
    let mut summaries = BTreeMap::new();
    let mut previous: BTreeMap<(u32, u64), &Row> = BTreeMap::new();
    let mut last_at = None;
    let mut retained = 0;
    for f in frames.iter().filter(|f| f.at <= at && f.error.is_none()) {
        let current = f
            .rows
            .iter()
            .map(|r| ((r.pid, r.start_time_ms), r))
            .collect::<BTreeMap<_, _>>();
        if f.at >= start {
            retained += 1;
            let index = (((f.at - start) / width) as usize).min(count - 1);
            let b = &mut buckets[index];
            let cpu = f.rows.iter().map(|r| r.cpu_percent).sum::<f64>();
            b.count += 1;
            b.cpu_sum += cpu;
            b.cpu_max = b.cpu_max.max(cpu);
            b.rss = b.rss.max(f.rows.iter().map(|r| r.resident_bytes).sum());
            b.processes = b.processes.max(current.len());
            let fraction = last_at
                .filter(|&prev| prev < start && f.at > prev)
                .map_or(1., |prev| (f.at - start) as f64 / (f.at - prev) as f64);
            for (&key, &r) in &current {
                let s = summaries.entry(key).or_insert(Summary {
                    first: r,
                    latest: r,
                    samples: 0,
                    cpu_sum: 0.,
                    cpu_max: 0.,
                    rss_peak: 0,
                    cpu_time: 0,
                    read: 0,
                    written: 0,
                });
                s.latest = r;
                s.samples += 1;
                s.cpu_sum += r.cpu_percent;
                s.cpu_max = s.cpu_max.max(r.cpu_percent);
                s.rss_peak = s.rss_peak.max(r.resident_bytes);
                if let Some(old) = previous.get(&key)
                    && last_at.is_some_and(|at| (1..=30_000).contains(&(f.at - at)))
                {
                    let delta = |new: u64, old: u64| {
                        ((new.saturating_sub(old) as f64) * fraction).round() as u64
                    };
                    s.cpu_time += delta(r.cpu_time_ms, old.cpu_time_ms);
                    let read = delta(r.io_read_bytes, old.io_read_bytes);
                    let written = delta(r.io_write_bytes, old.io_write_bytes);
                    s.read += read;
                    s.written += written;
                    b.read += read;
                    b.written += written;
                }
            }
        }
        previous = current;
        last_at = Some(f.at);
    }
    let buckets=buckets.iter().enumerate().map(|(i,b)|{
        let from=start+i as i64*width;
        let mut value=json!({"started_at":stamp(from),"ended_at":stamp((from+width).min(at)),"avg_cpu_percent":if b.count>0{b.cpu_sum/b.count as f64}else{0.},
            "max_cpu_percent":b.cpu_max,"max_rss_bytes":b.rss,"max_process_count":b.processes});
        if !legacy{value["io_read_bytes"]=json!(b.read);value["io_write_bytes"]=json!(b.written);}value
    }).collect::<Vec<_>>();
    let mut summaries = summaries.into_values().collect::<Vec<_>>();
    summaries.sort_by_key(|s| std::cmp::Reverse((s.cpu_time, s.rss_peak)));
    let cpu_seconds = summaries.iter().map(|s| s.cpu_time).sum::<u64>() as f64 / 1000.;
    let top=summaries.iter().map(|s|{
        let r=s.latest;let mut v=json!({"pid":r.pid,"start_time_ms":r.start_time_ms,"ppid":r.ppid,"depth":r.depth,"command":r.command,"first_seen_at":s.first.first_seen_at,
            "last_seen_at":r.last_seen_at,"current_cpu_percent":r.cpu_percent,"avg_cpu_percent":s.cpu_sum/s.samples as f64,"max_cpu_percent":s.cpu_max,
            "current_rss_bytes":r.resident_bytes,"sample_count":s.samples});
        if legacy{
            v["process_key"]=json!(format!("{}:{}",r.pid,r.start_time_ms));v["is_server_root"]=json!(r.category=="server");
            v["cpu_seconds_approx"]=json!(s.cpu_time as f64/1000.);v["max_rss_bytes"]=json!(s.rss_peak);
        }else{
            v["name"]=json!(r.name);v["category"]=json!(r.category);v["peak_rss_bytes"]=json!(s.rss_peak);
            v["cpu_time_ms"]=json!(s.cpu_time);v["io_read_bytes"]=json!(s.read);v["io_write_bytes"]=json!(s.written);v["io_semantics"]=json!(r.io_semantics);
        }v
    }).collect::<Vec<_>>();
    let mut v = json!({"read_at":stamp(at),"window_ms":window,"bucket_ms":width,"sample_interval_ms":interval,
        "retained_sample_count":retained,"buckets":buckets,"top_processes":top});
    if legacy {
        v["total_cpu_seconds_approx"] = json!(cpu_seconds);
        v["error"] = frames.back().and_then(|f| f.error.as_ref()).map_or(
            Value::Null,
            |e| json!({"failure_tag":"process_query_failed","message":e}),
        );
    } else if let Some(f) = frames.back() {
        v["health"] = f.health();
    }
    v
}
