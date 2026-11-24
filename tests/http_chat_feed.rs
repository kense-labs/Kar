//! A real TCP subscriber resumes after interrupted HTTP bodies with its committed cursor.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use kar::{
    Error,
    chat::{ChatCursor, ChatEvent, ChatUpdate, HttpChatFeed},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

fn frame(chat: Uuid, before: u64, after: u64) -> String {
    let cursor = |n| format!("{chat}:{n}").parse::<ChatCursor>().unwrap();
    let event = ChatEvent {
        previous: cursor(before),
        cursor: cursor(after),
        update: ChatUpdate::RequestsChanged { requests: vec![] },
    };
    format!(
        "id: {}\r\ndata: {}\r\n\r\n",
        event.cursor,
        serde_json::to_string(&event).unwrap()
    )
}
#[derive(Clone)]
struct Peer {
    chat: Uuid,
    calls: Arc<Mutex<Vec<String>>>,
}

async fn endpoint(State(peer): State<Peer>, headers: HeaderMap) -> Response {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer local-fixture");
    let mut calls = peer.calls.lock().unwrap();
    calls.push(headers["last-event-id"].to_str().unwrap().to_owned());
    let one = frame(peer.chat, 1, 2);
    let two = frame(peer.chat, 2, 3);
    let body = match calls.len() {
        1 => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        // Disconnect in the middle of a UTF-8 code point, after the first complete event.
        2 => [one.as_bytes(), b"data: \"", &[0xe4, 0xb8]].concat(),
        // Redelivery of the last complete event must be skipped.
        3 => format!(": heartbeat\r\n\r\n{one}{two}").into_bytes(),
        // The consumer asks for the same event again after a local commit failed.
        4 => two.into_bytes(),
        5 => frame(peer.chat, 4, 5).into_bytes(),
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn resumes_at_the_consumers_commit_boundary() {
    let chat = Uuid::new_v4();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = Router::new()
        .route("/mount/chat/events", get(endpoint))
        .with_state(Peer {
            chat,
            calls: calls.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mount/chat/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer local-fixture".parse().unwrap(),
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let mut feed = HttpChatFeed::with_client(&url, client).unwrap();
    let start = format!("{chat}:1").parse().unwrap();
    tokio::time::timeout(Duration::from_secs(8),async {
        let one=feed.recv(start).await.unwrap();
        assert_eq!(one.previous,start);
        let two=feed.recv(one.cursor).await.unwrap();
        assert_eq!(two.previous,one.cursor);
        assert_eq!(feed.recv(one.cursor).await.unwrap().cursor,two.cursor);
        assert!(matches!(feed.recv(two.cursor).await,Err(Error::InvalidCursor)));
        assert!(matches!(feed.recv(two.cursor).await,Err(Error::Http(e)) if e.status()==Some(StatusCode::UNAUTHORIZED)));
        assert_eq!(*calls.lock().unwrap(),[start,start,one.cursor,one.cursor,two.cursor,two.cursor].map(|c|c.to_string()));
    }).await.unwrap();
    task.abort();
}
