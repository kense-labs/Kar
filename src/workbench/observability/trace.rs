use super::*;
use axum::{
    Json,
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::Response,
};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const BACKUPS: usize = 3;
const VOLUME: u64 = 8 << 20;
const MAX_RECORD: usize = 256 << 10;
pub(in crate::workbench) struct Trace {
    dir: PathBuf,
    write: Mutex<()>,
}
impl Trace {
    pub fn new(state: &Path) -> Self {
        Self {
            dir: state.join("traces"),
            write: Mutex::new(()),
        }
    }
    pub(super) fn path(&self, backup: usize) -> PathBuf {
        self.dir.join(if backup == 0 {
            "workbench.jsonl".into()
        } else {
            format!("workbench.jsonl.{backup}")
        })
    }
    pub(super) fn append(&self, record: &Value) -> Result<()> {
        let _guard = self.write.lock().unwrap();
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(0);
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if path
            .metadata()
            .is_ok_and(|m| m.len() + line.len() as u64 > VOLUME)
        {
            for i in (0..BACKUPS).rev() {
                match std::fs::rename(self.path(i), self.path(i + 1)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let mut open = OpenOptions::new();
        open.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        open.open(path)?.write_all(&line)?;
        Ok(())
    }
    pub(in crate::workbench) fn rotated(
        &self,
        before: std::time::SystemTime,
        apply: bool,
    ) -> Result<(u64, u64)> {
        let _guard = self.write.lock().unwrap();
        let mut files = 0;
        let mut bytes = 0u64;
        for backup in 1..=BACKUPS {
            let path = self.path(backup);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && meta.modified()? < before => {
                    files += 1;
                    bytes = bytes.saturating_add(meta.len());
                    if apply {
                        std::fs::remove_file(path)?;
                    }
                }
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok((files, bytes))
    }
    fn files(&self) -> Vec<(PathBuf, std::io::Result<File>)> {
        let _g = self.write.lock().unwrap();
        (0..=BACKUPS)
            .rev()
            .map(|i| {
                let p = self.path(i);
                let f = File::open(&p);
                (p, f)
            })
            .collect()
    }
}
static SERVER_START_NANO: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static TOTAL_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TOTAL_ERRORS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn parse_traceparent(header: &str) -> Option<(String, String)> {
    let mut parts = header.trim().split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_span_id = parts.next()?;
    let flags = parts.next()?;
    if parts.next().is_some()
        || version.len() != 2
        || trace_id.len() != 32
        || parent_span_id.len() != 16
        || flags.len() != 2
        || !trace_id.bytes().all(|b| b.is_ascii_hexdigit())
        || trace_id.bytes().all(|b| b == b'0')
        || !parent_span_id.bytes().all(|b| b.is_ascii_hexdigit())
        || parent_span_id.bytes().all(|b| b == b'0')
        || !flags.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some((
        trace_id.to_ascii_lowercase(),
        parent_span_id.to_ascii_lowercase(),
    ))
}

struct Span {
    trace: Arc<Trace>,
    name: String,
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    started: jiff::Timestamp,
    clock: Instant,
    finished: bool,
}
impl Span {
    fn record(&self, tag: &str, code: Option<u16>) -> Value {
        let ended = jiff::Timestamp::now();
        let cause = code.map(|c| format!("HTTP {c}"));
        let events = if tag == "Failure" {
            vec![
                json!({"name":cause,"timeUnixNano":ended.as_nanosecond().to_string(),
            "attributes":{"log.severity":if code.is_some_and(|c|c>=500){"Error"}else{"Warning"},"effect.logLevel":if code.is_some_and(|c|c>=500){"Error"}else{"Warning"}}}),
            ]
        } else {
            vec![]
        };
        let mut rec = json!({"name":self.name,"traceId":self.trace_id,"spanId":self.span_id,"durationMs":self.clock.elapsed().as_secs_f64()*1000.,
            "startTimeUnixNano":self.started.as_nanosecond().to_string(),"endTimeUnixNano":ended.as_nanosecond().to_string(),
            "exit":{"_tag":tag,"cause":cause},"events":events});
        if let Some(parent) = &self.parent_span_id {
            rec["parentSpanId"] = json!(parent);
        }
        rec
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let trace = self.trace.clone();
            let record = self.record("Interrupted", None);
            runtime.spawn_blocking(move || {
                let _ = trace.append(&record);
            });
        }
    }
}
pub(in crate::workbench) async fn observe(
    State(app): State<Workbench>,
    req: Request,
    next: Next,
) -> Response {
    let Some(route) = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
    else {
        return next.run(req).await;
    };
    if matches!(
        route,
        "/diagnostics/otlp" | "/diagnostics/traces" | "/telemetry/v1/traces"
    ) {
        return next.run(req).await;
    }
    let boot_nano = SERVER_START_NANO
        .get_or_init(|| jiff::Timestamp::now().as_nanosecond().to_string())
        .clone();
    let incoming_tp = req
        .headers()
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_traceparent);
    let (trace_id, parent_span_id) = match incoming_tp {
        Some((tid, pid)) => (tid, Some(pid)),
        None => (Uuid::new_v4().simple().to_string(), None),
    };
    let span_id = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let outbound_tp = format!("00-{trace_id}-{span_id}-01");
    let mut span = Span {
        trace: app.0.observability.traces.clone(),
        name: format!("{} {route}", req.method()),
        trace_id,
        span_id,
        parent_span_id,
        started: jiff::Timestamp::now(),
        clock: Instant::now(),
        finished: false,
    };
    let mut response = next.run(req).await;
    if let Ok(hv) = outbound_tp.parse() {
        response.headers_mut().insert("traceparent", hv);
    }
    let status = response.status();
    let is_fail = status.is_client_error() || status.is_server_error();
    let total_reqs = TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let total_errs = if is_fail {
        TOTAL_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    } else {
        TOTAL_ERRORS.load(std::sync::atomic::Ordering::Relaxed)
    };
    let record = span.record(
        if is_fail { "Failure" } else { "Success" },
        Some(status.as_u16()),
    );
    span.finished = true;
    let end_nano = record["endTimeUnixNano"]
        .as_str()
        .unwrap_or(&boot_nano)
        .to_owned();
    super::otlp::export_server_telemetry(
        record.clone(),
        total_reqs,
        total_errs,
        boot_nano,
        end_nano,
    );
    let trace = span.trace.clone();
    let _ = tokio::task::spawn_blocking(move || trace.append(&record)).await;
    response
}
#[derive(Default)]
struct Stats {
    count: u64,
    fails: u64,
    total: f64,
    max: f64,
}
#[derive(Default)]
struct Read {
    records: u64,
    invalid: u64,
    failed: u64,
    interrupted: u64,
    slow: u64,
    first: Option<i64>,
    last: Option<i64>,
    names: BTreeMap<String, Stats>,
    levels: BTreeMap<String, u64>,
    failures: BTreeMap<(String, String), (u64, i64, Value)>,
    latest: Vec<(i64, Value)>,
    slowest: Vec<(f64, Value)>,
    logs: Vec<(i64, Value)>,
}
fn ns(v: &Value) -> Option<i64> {
    let n = v.as_str()?.parse::<i128>().ok()?;
    let ms = i64::try_from(n / 1_000_000).ok()?;
    jiff::Timestamp::from_millisecond(ms).ok()?;
    Some(ms)
}
fn push<T: PartialOrd>(list: &mut Vec<(T, Value)>, key: T, v: Value, n: usize) {
    let i = list.partition_point(|(k, _)| *k >= key);
    if i < n {
        list.insert(i, (key, v));
        list.truncate(n);
    }
}
fn values<T>(list: Vec<(T, Value)>) -> Vec<Value> {
    list.into_iter().map(|(_, v)| v).collect()
}
impl Read {
    fn add(&mut self, line: &[u8]) -> Option<()> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Some(());
        }
        let v = serde_json::from_slice::<Value>(line).ok()?;
        let name = v["name"].as_str().filter(|s| !s.trim().is_empty())?;
        let trace = v["traceId"].as_str().filter(|s| !s.trim().is_empty())?;
        let span = v["spanId"].as_str().filter(|s| !s.trim().is_empty())?;
        let elapsed = v["durationMs"].as_f64()?;
        let ended = ns(&v["endTimeUnixNano"])?;
        let fail = v["exit"]["_tag"] == "Failure" || v["status"]["code"] == "2";
        let interrupted = v["exit"]["_tag"] == "Interrupted"
            || v["attributes"]["effect.fiber.interrupted"] == true
            || v["status"]["message"] == "Interrupted";
        self.records += 1;
        self.failed += u64::from(fail);
        self.slow += u64::from(elapsed >= 1000.);
        self.interrupted += u64::from(interrupted);
        if let Some(t) = ns(&v["startTimeUnixNano"]) {
            self.first = Some(self.first.map_or(t, |f| f.min(t)));
        }
        self.last = Some(self.last.map_or(ended, |l| l.max(ended)));
        let stat = self.names.entry(name.into()).or_default();
        stat.count += 1;
        stat.fails += u64::from(fail);
        stat.total += elapsed;
        stat.max = stat.max.max(elapsed);
        let mut item = json!({"name":name,"trace_id":trace,"span_id":span,"duration_ms":elapsed,"ended_at":stamp(ended)});
        push(&mut self.slowest, elapsed, item.clone(), 10);
        if fail {
            let cause = v["exit"]["cause"]
                .as_str()
                .or(v["status"]["message"].as_str())
                .unwrap_or("Failure");
            item["cause"] = json!(cause);
            push(&mut self.latest, ended, item.clone(), 20);
            let f = self.failures.entry((name.into(), cause.into())).or_insert((
                0,
                ended,
                item.clone(),
            ));
            f.0 += 1;
            if ended >= f.1 {
                f.1 = ended;
                f.2 = item;
            }
        }
        for event in v["events"].as_array().into_iter().flatten() {
            let attrs = &event["attributes"];
            let Some(level) = attrs["effect.logLevel"]
                .as_str()
                .or(attrs["log.level"].as_str())
            else {
                continue;
            };
            let level = level.to_owned();
            *self.levels.entry(level.clone()).or_default() += 1;
            if matches!(
                level.to_ascii_uppercase().as_str(),
                "WARN" | "WARNING" | "ERROR" | "FATAL"
            ) {
                let t = ns(&event["timeUnixNano"]).unwrap_or(ended);
                let msg = attrs["log.message"]
                    .as_str()
                    .or(event["name"].as_str())
                    .unwrap_or("");
                push(
                    &mut self.logs,
                    t,
                    json!({"span_name":name,"level":level,"message":msg,"seen_at":stamp(t),"trace_id":trace,"span_id":span}),
                    20,
                );
            }
        }
        Some(())
    }
    fn file(&mut self, file: File) -> std::io::Result<()> {
        // Bound individual records, including malformed unterminated data, independently of file size.
        let mut reader = BufReader::new(file);
        let mut line = vec![];
        let mut overflow = false;
        loop {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                if overflow || self.add(&line).is_none() {
                    self.invalid += 1;
                }
                break;
            }
            let n = buf.iter().position(|b| *b == b'\n');
            let len = n.unwrap_or(buf.len());
            if !overflow && line.len() + len <= MAX_RECORD {
                line.extend_from_slice(&buf[..len]);
            } else {
                overflow = true;
            }
            reader.consume(len + usize::from(n.is_some()));
            if n.is_some() {
                if overflow || self.add(&line).is_none() {
                    self.invalid += 1;
                }
                line.clear();
                overflow = false;
            }
        }
        Ok(())
    }
}
pub(super) async fn read(
    State(app): State<Workbench>,
) -> std::result::Result<Json<Value>, super::super::http::ApiError> {
    super::super::http::blocking(move || Ok(summary(&app.0.observability.traces))).await
}
fn summary(trace: &Trace) -> Value {
    let mut s = Read::default();
    let mut paths = vec![];
    let mut found = false;
    let mut failed = false;
    for (p, result) in trace.files() {
        paths.push(p);
        match result {
            Ok(file) => {
                found = true;
                if s.file(file).is_err() {
                    failed = true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => failed = true,
        }
    }
    let mut top = s.names.into_iter().collect::<Vec<_>>();
    top.sort_by(|a, b| {
        b.1.count
            .cmp(&a.1.count)
            .then_with(|| b.1.max.total_cmp(&a.1.max))
    });
    top.truncate(10);
    let by_count=top.into_iter().map(|(name,x)|json!({"name":name,"count":x.count,"failure_count":x.fails,"total_duration_ms":x.total,"average_duration_ms":x.total/x.count as f64,"max_duration_ms":x.max})).collect::<Vec<_>>();
    let mut common = s.failures.into_values().collect::<Vec<_>>();
    common.sort_by_key(|(n, t, _)| std::cmp::Reverse((*n, *t)));
    common.truncate(10);
    let common = common
        .into_iter()
        .map(|(n, last, mut v)| {
            v["count"] = json!(n);
            v["last_seen_at"] = json!(stamp(last));
            v
        })
        .collect::<Vec<_>>();
    let error = if failed {
        json!({"kind":"trace-file-read-failed","message":"could not read a local trace volume"})
    } else if !found {
        json!({"kind":"trace-file-not-found","message":"no local trace volumes were found"})
    } else {
        Value::Null
    };
    json!({"trace_file_path":trace.path(0),"scanned_file_paths":paths,"read_at":stamp(now()),"record_count":s.records,"parse_error_count":s.invalid,"first_span_at":s.first.map(stamp),
        "last_span_at":s.last.map(stamp),"failure_count":s.failed,"interruption_count":s.interrupted,"slow_span_threshold_ms":1000,"slow_span_count":s.slow,
        "log_level_counts":s.levels,"top_spans_by_count":by_count,"slowest_spans":values(s.slowest),"common_failures":common,"latest_failures":values(s.latest),
        "latest_warning_and_error_logs":values(s.logs),"partial_failure":(failed && found).then_some(true),"error":error})
}
