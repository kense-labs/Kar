use super::super::http::{ApiError, blocking};
use super::*;
use axum::{
    Json, Router,
    extract::{Path as Param, Query, State},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use std::convert::Infallible;
type Reply = std::result::Result<Json<Value>, ApiError>;
#[derive(Deserialize, Default)]
struct QueryModels {
    benchmarks: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    model_id: String,
}
#[derive(Deserialize)]
struct Link {
    path: String,
}
#[derive(Deserialize)]
struct Acceleration {
    acceleration: String,
}
#[derive(Deserialize)]
struct Unload {
    timeout: String,
}
pub(in crate::workbench) fn routes() -> Router<Workbench> {
    Router::new()
        .merge(super::local::routes())
        .merge(super::cloud::routes())
        .route("/api/voice/status", get(status))
        .route("/api/voice/models", get(list))
        .route("/api/voice/models/events", get(events))
        .route("/api/voice/models/{action}", post(model_action))
        .route("/api/voice/models/link", post(link))
        .route("/api/voice/acceleration", post(acceleration))
        .route("/api/voice/model-unload-timeout", post(unload))
}
async fn list(State(app): State<Workbench>, Query(q): Query<QueryModels>) -> Reply {
    blocking(move || {
        Ok(app
            .0
            .voice
            .catalog(q.benchmarks.as_deref() == Some("1"), true))
    })
    .await
}
async fn events(
    State(app): State<Workbench>,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let updates = app.0.voice.updates.subscribe();
    let stream = futures_util::stream::unfold(
        (app, updates, true),
        |(app, mut updates, first)| async move {
            if !first && updates.changed().await.is_err() {
                return None;
            }
            let revision = *updates.borrow_and_update();
            let snapshot = app.clone();
            let models = tokio::task::spawn_blocking(move || snapshot.0.voice.catalog(false, true))
                .await
                .ok()?;
            Some((
                Ok(Event::default()
                    .event("models")
                    .id(revision.to_string())
                    .json_data(models)
                    .expect("JSON catalog")),
                (app, updates, false),
            ))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}
async fn model_action(
    State(app): State<Workbench>,
    Param(action): Param<String>,
    Json(r): Json<Request>,
) -> Reply {
    let m = model(&r.model_id)?;
    match action.as_str() {
        "download" => {
            let cancelled = models::download(app.clone(), m).await?;
            blocking(move || {
                let mut view = app.0.voice.catalog(false, true);
                view["cancelled"] = json!(cancelled);
                Ok(view)
            })
            .await
        }
        "cancel" => {
            app.0.voice.cancel(&m.id).await?;
            blocking(move || Ok(app.0.voice.catalog(false, true))).await
        }
        "remove" | "select" => {
            blocking(move || {
                let svc = &app.0.voice;
                if action == "remove" {
                    svc.remove(m)?;
                } else {
                    let _gate = svc.exclusive()?;
                    if !svc.verify(m, &svc.path(m, &svc.config()))? {
                        return Err(Error::Refused(
                            "speech model is not installed or failed verification".into(),
                        ));
                    }
                    svc.update(|s| s.model = m.id.clone())?;
                }
                Ok(svc.catalog(false, true))
            })
            .await
        }
        _ => Err(super::super::missing().into()),
    }
}
async fn link(State(app): State<Workbench>, Json(r): Json<Link>) -> Reply {
    blocking(move || {
        let value = r.path.trim();
        if value.is_empty() || value.len() > 4096 {
            return Err(Error::Refused("invalid speech model source".into()));
        }
        let path = if let Some(rel) = value.strip_prefix("~/") {
            std::env::home_dir()
                .ok_or_else(|| Error::Refused("home unavailable".into()))?
                .join(rel)
        } else {
            PathBuf::from(value)
        };
        if !path.is_absolute() {
            return Err(Error::Refused(
                "speech model link needs an absolute path".into(),
            ));
        }
        app.0.voice.link(&path)?;
        Ok(app.0.voice.catalog(false, true))
    })
    .await
}
async fn status(State(app): State<Workbench>) -> Reply {
    let gpu = match app.0.voice.devices().await {
        Ok(devices) => devices.gpu.clone(),
        Err(error) => return Ok(Json(json!({"supported":false,"reason":error.to_string()}))),
    };
    blocking(move || app.0.voice.status(&gpu)).await
}
async fn acceleration(State(app): State<Workbench>, Json(r): Json<Acceleration>) -> Reply {
    let value = r.acceleration;
    if !matches!(value.as_str(), "auto" | "cpu")
        && !(value.starts_with("gpu:") && value.len() > 4 && value.len() <= 500)
    {
        return Err(Error::Refused("invalid speech acceleration".into()).into());
    }
    app.0.voice.update(|s| s.acceleration = value)?;
    status(State(app)).await
}
async fn unload(State(app): State<Workbench>, Json(r): Json<Unload>) -> Reply {
    if r.timeout != "immediately" {
        return Err(Error::Unsupported(
            "the native CLI releases the model after each request".into(),
        )
        .into());
    }
    status(State(app)).await
}
