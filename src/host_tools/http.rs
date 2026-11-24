use super::{State, rpc_error};
use axum::{
    Json,
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::{StreamExt, future::join_all, stream};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, task::JoinSet};

const SESSION: &str = "mcp-session-id";
const MAX_BODY: usize = 4 * 1024 * 1024;

pub(super) async fn listen(listener: TcpListener, state: Arc<State>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break };
                let state = Arc::clone(&state);
                connections.spawn(async move {
                    let shutdown = state.shutdown.clone();
                    let service = service_fn(move |request| {
                        let state = Arc::clone(&state);
                        async move { Ok::<_, Infallible>(tokio::select! {
                            _ = state.shutdown.cancelled() => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                            response = respond(&state, request) => response,
                        }) }
                    });
                    let connection = http1::Builder::new().serve_connection(TokioIo::new(socket), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = shutdown.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = tokio::time::timeout(Duration::from_millis(300), connection).await;
                        }
                    }
                });
            }
        }
    }
    drop(listener);
    state.shutdown.cancel();
    while connections.join_next().await.is_some() {}
}

fn local_origin(headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(value) = origins.next() else {
        return true;
    };
    origins.next().is_none()
        && value
            .to_str()
            .ok()
            .and_then(|s| url::Url::parse(s).ok())
            .is_some_and(|u| matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
}

fn with_session(mut response: Response, session: Option<&HeaderValue>) -> Response {
    if let Some(value) = session {
        response.headers_mut().insert(SESSION, value.clone());
    }
    response
}

fn events(state: &State, session: Option<&HeaderValue>) -> Response {
    let mut changes = state.changes.subscribe();
    let seen = *changes.borrow_and_update();
    let shutdown = state.shutdown.clone();
    let stream = stream::unfold(
        (changes, seen, shutdown),
        |(mut changes, seen, shutdown)| async move {
            if shutdown.is_cancelled() {
                return None;
            }
            if *changes.borrow_and_update() == seen {
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    updated = changes.changed() => { updated.ok()?; }
                }
            }
            let event = Event::default()
                .event("message")
                .data(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#);
            Some((Ok::<_, Infallible>(event), (changes, seen + 1, shutdown)))
        },
    );
    let response = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    );
    with_session(response.into_response(), session)
}

async fn respond(state: &State, request: Request<Incoming>) -> Response {
    let (parts, incoming) = request.into_parts();
    if !local_origin(&parts.headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let authorized = parts
        .headers
        .get(header::AUTHORIZATION)
        .is_some_and(|h| bool::from(h.as_bytes().ct_eq(state.authorization.as_bytes())));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if parts.uri.path() != "/mcp" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let session = parts.headers.get(SESSION);
    if parts.method == Method::GET
        && parts.headers.get_all(header::ACCEPT).iter().any(|h| {
            h.to_str().is_ok_and(|s| {
                s.split(',').any(|t| {
                    t.split(';')
                        .next()
                        .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
                })
            })
        })
    {
        return events(state, session);
    }
    if parts.method != Method::POST {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, POST")],
        )
            .into_response();
    }
    let mut stream = Body::new(incoming).into_data_stream();
    let mut buffer = Vec::new();
    let mut oversized = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !oversized && chunk.len() <= MAX_BODY - buffer.len() {
            buffer.extend_from_slice(&chunk);
        } else {
            oversized = true;
        }
    }
    if oversized {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(header::CONNECTION, "close")],
        )
            .into_response();
    }
    let invalid = |code, message| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "jsonrpc":"2.0","id":null,"error":rpc_error(code, message)
            })),
        )
            .into_response()
    };
    let Ok(value) = serde_json::from_slice::<Value>(&buffer) else {
        return invalid(-32700, "parse error");
    };
    let batch = value.is_array();
    let messages = match &value {
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            items.as_slice()
        }
        Value::Object(_) => std::slice::from_ref(&value),
        _ => return invalid(-32600, "invalid request"),
    };
    let new_session;
    let session = if messages.iter().any(|m| m["method"] == "initialize") {
        new_session =
            HeaderValue::from_str(&format!("oar-host-tools-{}", uuid::Uuid::new_v4().simple()))
                .unwrap();
        Some(&new_session)
    } else {
        session
    };
    let id = session.and_then(|v| v.to_str().ok()).unwrap_or_default();
    let mut replies: Vec<_> = join_all(messages.iter().map(|m| state.answer(id, m)))
        .await
        .into_iter()
        .flatten()
        .collect();
    if replies.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = if batch {
        Value::Array(replies)
    } else {
        replies.remove(0)
    };
    with_session(Json(result).into_response(), session)
}
