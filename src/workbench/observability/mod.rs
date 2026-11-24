//! Host-owned diagnostics, identified by PID and process birth time.
mod history;
mod otlp;
mod trace;
mod triage;
pub(super) use trace::observe;
#[cfg(target_os = "macos")]
mod macos;
mod routes;
pub(super) use routes::routes;

use super::Workbench;
use crate::{Error, Result, resources::tree::ProcessId};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const INTERVAL: u64 = 1_000;
fn now() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}
fn stamp(at: i64) -> String {
    jiff::Timestamp::from_millisecond(at)
        .expect("system clock")
        .to_string()
}

#[derive(Clone, Serialize)]
struct Row {
    #[serde(skip)]
    key: Option<ProcessId>,
    pid: u32,
    start_time_ms: u64,
    ppid: u32,
    child_pids: Vec<u32>,
    depth: usize,
    name: String,
    command: String,
    status: String,
    category: &'static str,
    cpu_percent: f64,
    cpu_time_ms: u64,
    resident_bytes: u64,
    peak_resident_bytes: u64,
    virtual_bytes: u64,
    io_read_bytes: u64,
    io_write_bytes: u64,
    io_semantics: &'static str,
    io_read_bytes_per_second: f64,
    io_write_bytes_per_second: f64,
    run_time_ms: u64,
    first_seen_at: String,
    last_seen_at: String,
}
#[derive(Default)]
struct Frame {
    at: i64,
    duration_us: u64,
    scanned: usize,
    rows: Vec<Row>,
    error: Option<String>,
    starts: usize,
    exits: usize,
    cpu_ms: u64,
    read_bytes: u64,
    written_bytes: u64,
}
impl Frame {
    fn health(&self) -> Value {
        json!({"native":{"status":if self.error.is_some(){"degraded"}else{"healthy"},"last_sample_at":stamp(self.at),"last_error":self.error},
            "desktop":{"status":"unavailable","last_sample_at":null,"last_error":null},"collection_duration_micros":self.duration_us,
            "scanned_process_count":self.scanned,"retained_process_count":self.rows.len(),"inaccessible_process_count":self.scanned.saturating_sub(self.rows.len())})
    }
    fn snapshot(&self, interval: u64) -> Value {
        let rows = &self.rows;
        let group = json!({"process_count":rows.len(),"current_cpu_percent":rows.iter().map(|r|r.cpu_percent).sum::<f64>(),
            "cpu_time_ms":self.cpu_ms,"current_rss_bytes":rows.iter().map(|r|r.resident_bytes).sum::<u64>(),
            "peak_rss_bytes":rows.iter().map(|r|r.peak_resident_bytes).sum::<u64>(),"io_read_bytes":self.read_bytes,
            "io_write_bytes":self.written_bytes,"io_read_bytes_per_second":rows.iter().map(|r|r.io_read_bytes_per_second).sum::<f64>(),
            "io_write_bytes_per_second":rows.iter().map(|r|r.io_write_bytes_per_second).sum::<f64>(),"process_starts":self.starts,"process_exits":self.exits});
        let mut empty = group.clone();
        for value in empty.as_object_mut().unwrap().values_mut() {
            *value = json!(0);
        }
        let processes = rows
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).expect("process sample");
                v["identity"] = json!({"pid":r.pid,"start_time_ms":r.start_time_ms});
                v
            })
            .collect::<Vec<_>>();
        json!({"read_at":stamp(self.at),"sample_interval_ms":interval,"processes":processes,"groups":{"backend":group,"all_kar":group,"desktop":empty,"monitor":empty},
        "power":{"source":"unknown","idle":"unknown","idle_seconds":null,"locked":"unknown","suspended":false,"on_battery":"unknown","low_power_mode":"unknown","thermal_state":"unknown","stale":true,"updated_at":stamp(self.at)},
        "speed_limit_percent":null,"health":self.health()})
    }
}
struct State {
    #[cfg(target_os = "macos")]
    collector: sysinfo::System,
    history: VecDeque<Arc<Frame>>,
    last: Option<Instant>,
}
pub(super) struct Service {
    traces: Arc<trace::Trace>,
    triage: Arc<triage::Triage>,
    state: Arc<Mutex<State>>,
    updates: watch::Sender<Option<Arc<Frame>>>,
    polling: std::sync::atomic::AtomicBool,
    refresh: tokio::sync::Mutex<()>,
}
impl Service {
    pub(super) fn open(directory: &std::path::Path) -> Self {
        Self {
            traces: Arc::new(trace::Trace::new(directory)),
            triage: Arc::new(triage::Triage::new(directory)),
            state: Arc::new(Mutex::new(State {
                #[cfg(target_os = "macos")]
                collector: sysinfo::System::new(),
                history: VecDeque::new(),
                last: None,
            })),
            updates: watch::channel(None).0,
            polling: false.into(),
            refresh: Default::default(),
        }
    }
}
impl Service {
    fn interval(&self) -> u64 {
        if self.updates.receiver_count() > 0 {
            INTERVAL
        } else {
            0
        }
    }
    async fn sample(&self, app: &Workbench) -> Result<Arc<Frame>> {
        let _gate = self.refresh.lock().await;
        let state = self.state.clone();
        let categories = app.process_categories();
        let frame = tokio::task::spawn_blocking(move || {
            let mut s = state.lock().unwrap();
            if s.last
                .is_some_and(|t| t.elapsed() < Duration::from_millis(200))
                && let Some(f) = s.history.back()
            {
                return f.clone();
            }
            let before = Instant::now();
            let at = now();
            #[cfg(target_os = "macos")]
            let result = {
                let first = s.history.is_empty();
                macos::sample(&mut s.collector, &categories, at, first)
            };
            #[cfg(not(target_os = "macos"))]
            let result: Result<(usize, Vec<Row>)> = {
                let _ = categories;
                Err(Error::Unsupported(
                    "native host telemetry requires macOS".into(),
                ))
            };
            let mut frame = match result {
                Ok((scanned, rows)) => Frame {
                    rows,
                    scanned,
                    ..Default::default()
                },
                Err(e) => Frame {
                    error: Some(e.to_string()),
                    ..Default::default()
                },
            };
            frame.at = at;
            frame.duration_us = u64::try_from(before.elapsed().as_micros()).unwrap_or(u64::MAX);
            if let Some(previous) = s.history.iter().rev().find(|f| f.error.is_none())
                && frame.error.is_none()
            {
                apply_deltas(&mut frame, previous);
            } else {
                frame.starts = frame.rows.len();
            }
            let frame = Arc::new(frame);
            // Retain a predecessor for a window starting between samples. Repeated HTTP reads
            // inside one second share a history slot, while active subscriptions get fresh frames.
            if s.history
                .back()
                .is_some_and(|f| f.at.div_euclid(1000) == at.div_euclid(1000))
            {
                s.history.pop_back();
            }
            s.history.push_back(frame.clone());
            while s.history.get(1).is_some_and(|f| f.at <= at - 3_600_000) {
                s.history.pop_front();
            }
            s.last = Some(Instant::now());
            frame
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?;
        self.updates.send_if_modified(|old| {
            if old.as_ref().is_some_and(|f| Arc::ptr_eq(f, &frame)) {
                false
            } else {
                *old = Some(frame.clone());
                true
            }
        });
        Ok(frame)
    }
    fn observe(&self, app: &Workbench) -> watch::Receiver<Option<Arc<Frame>>> {
        use std::sync::atomic::Ordering;
        let rx = self.updates.subscribe();
        if !self.polling.swap(true, Ordering::AcqRel) {
            let weak = Arc::downgrade(&app.0);
            tokio::spawn(async move {
                loop {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let app = Workbench(inner);
                    let svc = &app.0.observability;
                    if svc.updates.receiver_count() == 0 {
                        svc.polling.store(false, Ordering::Release);
                        if svc.updates.receiver_count() == 0
                            || svc.polling.swap(true, Ordering::AcqRel)
                        {
                            return;
                        }
                    }
                    let _ = svc.sample(&app).await;
                    drop(app);
                    tokio::time::sleep(Duration::from_millis(INTERVAL)).await;
                }
            });
        }
        rx
    }
}
fn apply_deltas(frame: &mut Frame, previous: &Frame) {
    let older = previous
        .rows
        .iter()
        .map(|p| ((p.pid, p.start_time_ms), p))
        .collect::<BTreeMap<_, _>>();
    let elapsed = frame.at - previous.at;
    let seconds = elapsed as f64 / 1000.;
    frame.cpu_ms = previous.cpu_ms;
    frame.read_bytes = previous.read_bytes;
    frame.written_bytes = previous.written_bytes;
    let mut matched = 0;
    for p in &mut frame.rows {
        if let Some(old) = older.get(&(p.pid, p.start_time_ms)) {
            p.first_seen_at = old.first_seen_at.clone();
            p.peak_resident_bytes = p.resident_bytes.max(old.peak_resident_bytes);
            if (1..=30_000).contains(&elapsed) {
                let cpu = p.cpu_time_ms.saturating_sub(old.cpu_time_ms);
                let read = p.io_read_bytes.saturating_sub(old.io_read_bytes);
                let written = p.io_write_bytes.saturating_sub(old.io_write_bytes);
                p.cpu_percent = cpu as f64 * 100. / elapsed as f64;
                p.io_read_bytes_per_second = read as f64 / seconds;
                p.io_write_bytes_per_second = written as f64 / seconds;
                frame.cpu_ms += cpu;
                frame.read_bytes += read;
                frame.written_bytes += written;
            }
            matched += 1;
        }
    }
    frame.starts = previous.starts + frame.rows.len() - matched;
    frame.exits = previous.exits + previous.rows.len() - matched;
}
impl Workbench {
    fn process_categories(&self) -> BTreeMap<ProcessId, &'static str> {
        let mut types = BTreeMap::new();
        for c in self.0.controllers.lock().unwrap().values() {
            if let Some(session) = c.live.lock().unwrap().upgrade()
                && let Some(root) = &session.process_root
            {
                types.insert(root.identity(), "provider-root");
            }
        }
        for id in self.0.terminal_preview.process_roots() {
            types.insert(id, "terminal-root");
        }
        types
    }
}
