use std::time::Duration;
pub(crate) struct SseClient {
    response: reqwest::Response,
    bytes: Vec<u8>,
}
impl SseClient {
    pub(crate) async fn open(url: &str, initial: &str, saved: Option<&str>) -> Self {
        let mut request = reqwest::Client::new()
            .get(format!("{url}/events"))
            .query(&[("after", initial)]);
        if let Some(saved) = saved {
            request = request.header("last-event-id", saved);
        }
        let response = request.send().await.unwrap().error_for_status().unwrap();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        Self {
            response,
            bytes: vec![],
        }
    }
    pub(crate) async fn next(&mut self) -> kar::chat::ChatEvent {
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(end) = self.bytes.windows(2).position(|s| s == b"\n\n") {
                    let block = String::from_utf8(self.bytes.drain(..end + 2).collect()).unwrap();
                    let Some(data) = block.lines().find_map(|l| l.strip_prefix("data: ")) else {
                        continue;
                    };
                    let event: kar::chat::ChatEvent = serde_json::from_str(data).unwrap();
                    assert_eq!(
                        block.lines().find_map(|l| l.strip_prefix("id: ")),
                        Some(event.cursor.to_string().as_str())
                    );
                    return event;
                }
                self.bytes.extend(
                    self.response
                        .chunk()
                        .await
                        .unwrap()
                        .expect("SSE disconnected"),
                );
            }
        })
        .await
        .expect("SSE event timeout")
    }
}
