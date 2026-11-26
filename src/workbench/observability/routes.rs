use super::super::http::ApiError;
use super::*;
use axum::{
    Json, Router,
    extract::{Query, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;

pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .route(
            "/usage/settings",
            get(usage::settings::get).put(usage::settings::put),
        )
        .route("/usage/summary", post(usage::summary))
        .route("/usage/rates/refresh", post(usage::rates))
        .route("/diagnostics/otlp", post(otlp::check))
        .route("/telemetry/v1/traces", post(otlp::browser_traces))
        .route("/diagnostics/triage", post(triage::prepare))
        .route("/diagnostics/traces", get(trace::read))
        .route("/diagnostics/host-resources", get(host))
        .route("/diagnostics/processes", get(processes))
        .route("/diagnostics/processes/signal", post(signal))
        .route("/diagnostics/process-history", get(process_history))
        .route("/telemetry", get(snapshot))
        .route("/telemetry/history", get(history))
        .route("/telemetry/retry", post(retry))
        .route("/telemetry/events", get(events))
}
type Reply = std::result::Result<Json<Value>, ApiError>;
async fn host() -> Reply {
    #[cfg(target_os = "macos")]
    {
        super::super::http::blocking(macos::host).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(Error::Unsupported("native host resources require macOS".into()).into())
    }
}
async fn snapshot(State(app): State<Workbench>) -> Reply {
    let svc = &app.0.observability;
    Ok(Json(svc.sample(&app).await?.snapshot(svc.interval())))
}
async fn retry(State(app): State<Workbench>) -> Reply {
    let svc = &app.0.observability;
    let latest = svc.updates.borrow().clone();
    let mut accepted = false;
    if cfg!(target_os = "macos") && latest.as_ref().is_some_and(|f| f.error.is_some()) {
        svc.state.lock().unwrap().last = None;
        accepted = true;
    }
    let f = match latest.filter(|_| !accepted) {
        None => svc.sample(&app).await?,
        Some(frame) => frame,
    };
    Ok(Json(
        json!({"accepted":accepted,"snapshot":f.snapshot(svc.interval())}),
    ))
}
async fn processes(State(app): State<Workbench>) -> Reply {
    let frame = app.0.observability.sample(&app).await?;
    let rows = frame
        .rows
        .iter()
        .filter(|p| p.category != "server")
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"server_pid":std::process::id(),"read_at":stamp(frame.at),"process_count":rows.len(),
        "total_rss_bytes":rows.iter().map(|r|r.resident_bytes).sum::<u64>(),"total_cpu_percent":rows.iter().map(|r|r.cpu_percent).sum::<f64>(),
        "processes":rows.iter().map(|r|json!({"pid":r.pid,"start_time_ms":r.start_time_ms,"ppid":r.ppid,"status":r.status,"cpu_percent":r.cpu_percent,
        "rss_bytes":r.resident_bytes,"elapsed_ms":r.run_time_ms,"command":r.command,"depth":r.depth.saturating_sub(1),"child_pids":r.child_pids})).collect::<Vec<_>>(),"error":frame.error}),
    ))
}
async fn history_response(app: Workbench, q: history::Window, legacy: bool) -> Reply {
    let svc = &app.0.observability;
    let frame = svc.sample(&app).await?;
    let interval = svc.interval();
    let frames = svc.state.lock().unwrap().history.clone();
    Ok(Json(
        tokio::task::spawn_blocking(move || history::build(&frames, frame.at, q, interval, legacy))
            .await
            .map_err(|e| Error::Closed(e.to_string()))?,
    ))
}
async fn history(State(app): State<Workbench>, Query(q): Query<history::Window>) -> Reply {
    history_response(app, q, false).await
}
async fn process_history(State(app): State<Workbench>, Query(q): Query<history::Window>) -> Reply {
    history_response(app, q, true).await
}
async fn events(State(app): State<Workbench>) -> std::result::Result<Response, ApiError> {
    let svc = &app.0.observability;
    let mut rx = svc.observe(&app);
    let f = svc.sample(&app).await?;
    // Include an already-published frame once, then wait for the next distinct observation.
    let f = rx.borrow_and_update().clone().unwrap_or(f);
    let feed = futures_util::stream::unfold((rx, Some(f)), |(mut rx, first)| async move {
        let frame = match first {
            Some(f) => f,
            None => {
                rx.changed().await.ok()?;
                rx.borrow_and_update().clone()?
            }
        };
        let event = Event::default()
            .event("telemetry")
            .id(frame.at.to_string())
            .json_data(frame.snapshot(INTERVAL))
            .ok()?;
        Some((Ok::<_, std::convert::Infallible>(event), (rx, None)))
    });
    Ok(Sse::new(feed)
        .keep_alive(KeepAlive::default())
        .into_response())
}
#[derive(Deserialize)]
struct Signal {
    pid: u32,
    start_time_ms: u64,
    signal: String,
}
async fn signal(State(app): State<Workbench>, Json(q): Json<Signal>) -> Reply {
    if !matches!(q.signal.as_str(), "SIGINT" | "SIGKILL") {
        return Err(Error::Refused("expected SIGINT or SIGKILL".into()).into());
    }
    let mut result = json!({"pid":q.pid,"signal":q.signal,"signaled":false,"message":null});
    if q.pid <= 1 || q.pid == std::process::id() {
        result["message"] = json!("refusing to signal the host process");
        return Ok(Json(result));
    }
    #[cfg(target_os = "macos")]
    {
        let f = app.0.observability.sample(&app).await?;
        let key = f
            .rows
            .iter()
            .find(|r| {
                r.pid == q.pid
                    && r.start_time_ms == q.start_time_ms
                    && matches!(
                        r.category,
                        "server-child" | "provider-root" | "terminal-root"
                    )
            })
            .and_then(|r| r.key);
        let Json(signaled) = super::super::http::blocking(move || {
            use crate::resources::tree::{descendants, identity};
            use nix::{
                sys::signal::{Signal, kill},
                unistd::Pid,
            };
            let Some(key) = key else {
                return Ok(false);
            };
            let Some(root) = identity(std::process::id()) else {
                return Ok(false);
            };
            if !descendants(root).contains(&key) || identity(key.pid) != Some(key) {
                return Ok(false);
            }
            let signal = if q.signal == "SIGINT" {
                Signal::SIGINT
            } else {
                Signal::SIGKILL
            };
            kill(Pid::from_raw(key.pid as i32), signal)
                .map_err(|e| Error::Io(std::io::Error::from_raw_os_error(e as i32)))?;
            Ok(true)
        })
        .await?;
        result["signaled"] = json!(signaled);
        if !signaled {
            result["message"] =
                json!("the selected identity is no longer a Kar backend descendant");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, q.start_time_ms);
        result["message"] = json!("process signals require macOS");
    }
    Ok(Json(result))
}
