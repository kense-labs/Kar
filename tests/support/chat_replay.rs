use crate::sse::SseClient;
use kar::chat::{ChatCursor, ChatReplica, ChatStore};
use serde_json::Value;
use uuid::Uuid;

pub(crate) async fn serve(store: &ChatStore, id: Uuid) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = kar::chat::event_router(store.clone(), id);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, server)
}
pub(crate) async fn receive(
    sub: &mut SseClient,
    replica: &mut ChatReplica,
    observed: &mut Vec<Value>,
    through: ChatCursor,
) {
    while replica.cursor() != through {
        let event = sub.next().await;
        observed.push(serde_json::to_value(&event).unwrap());
        assert!(replica.apply(event.clone()).unwrap());
        assert!(!replica.apply(event).unwrap());
    }
}
