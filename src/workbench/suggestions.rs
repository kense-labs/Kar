//! This SSE publishes only the current volatile value. Reconnecting reads a fresh snapshot.
use super::{
    Workbench,
    http::{ApiError, blocking},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use uuid::Uuid;

impl Workbench {
    pub(super) fn suggestion_source(&self, chat: Uuid) -> Option<crate::PromptSuggestionSource> {
        (!self.0.workflows.owns_agent_chat(chat)).then(|| self.0.suggestions.source(chat))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Setting {
    enabled: bool,
}
async fn setting(State(app): State<Workbench>) -> Json<Setting> {
    Json(Setting {
        enabled: app.0.suggestions.enabled(),
    })
}
async fn save(
    State(app): State<Workbench>,
    Json(input): Json<Setting>,
) -> Result<Json<Setting>, ApiError> {
    blocking(move || {
        app.update(|catalog| {
            catalog.prompt_suggestions = input.enabled;
            Ok(input)
        })
    })
    .await
}
async fn stream(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    app.binding(project, chat)?;
    let updates = app.0.suggestions.changes();
    let feed = futures_util::stream::unfold(
        (app, updates, None),
        move |(app, mut updates, mut previous)| async move {
            loop {
                if previous.is_some() && updates.changed().await.is_err() {
                    return None;
                }
                updates.borrow_and_update();
                // Only publish to the project that still owns an accessible chat.
                let value = if !app.archived(chat) && app.binding(project, chat).is_ok() {
                    app.0.suggestions.current(chat)
                } else {
                    None
                };
                if previous.as_ref() != Some(&value) {
                    let event = Event::default()
                        .event("suggestion")
                        .json_data(&value)
                        .ok()?;
                    return Some((Ok::<_, Infallible>(event), (app, updates, Some(value))));
                }
                previous = Some(value);
            }
        },
    );
    Ok(Sse::new(feed)
        .keep_alive(KeepAlive::default())
        .into_response())
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/settings/prompt-suggestions", get(setting).put(save))
        .route(
            "/projects/{project}/chats/{chat}/prompt-suggestion",
            get(stream),
        )
}
