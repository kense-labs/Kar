//! Reconnect to one saved chat using the consumer's last committed event.
use super::{ChatCursor, ChatEvent, ChatFeedSnapshot};
use crate::{Error, Result};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::{StreamExt, stream::BoxStream};
use reqwest::{Client, StatusCode, Url, header};
use std::time::Duration;
use tokio::time::{Instant, sleep_until};

const FIRST_RETRY: Duration = Duration::from_millis(200);
const MAX_RETRY: Duration = Duration::from_secs(5);

struct Connection {
    after: ChatCursor,
    stream: BoxStream<'static, std::result::Result<Event, EventStreamError<reqwest::Error>>>,
}

impl Connection {
    /// A closed response or interrupted frame can be retried at the same committed cursor.
    async fn open(client: &Client, url: &Url, after: ChatCursor) -> Result<Option<Self>> {
        let response = client
            .get(url.clone())
            .header(header::ACCEPT, "text/event-stream")
            .header("last-event-id", after.to_string())
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => return Ok(None),
            Err(e) => return Err(e.without_url().into()),
        };
        let status = response.status();
        if matches!(
            status,
            StatusCode::REQUEST_TIMEOUT
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        ) {
            return Ok(None);
        }
        if status == StatusCode::BAD_REQUEST {
            return Err(Error::InvalidCursor);
        }
        let response = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.split(';').next())
            .map(str::trim);
        if status != StatusCode::OK
            || !mime.is_some_and(|t| t.eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(Error::Protocol(
                "Chat endpoint did not return an SSE stream".into(),
            ));
        }
        Ok(Some(Self {
            after,
            stream: response.bytes_stream().eventsource().boxed(),
        }))
    }

    async fn next(&mut self) -> Result<Option<ChatEvent>> {
        loop {
            let frame = match self.stream.next().await {
                Some(Ok(frame)) if frame.event == "message" => frame,
                Some(Ok(_)) => continue,
                None | Some(Err(EventStreamError::Transport(_))) => return Ok(None),
                Some(Err(EventStreamError::Utf8(e))) if e.utf8_error().error_len().is_none() => {
                    return Ok(None);
                }
                Some(Err(_)) => {
                    return Err(Error::Protocol("Invalid SSE frame in chat feed".into()));
                }
            };
            let event: ChatEvent = serde_json::from_str(&frame.data)?;
            if frame.id.parse::<ChatCursor>()? != event.cursor {
                return Err(Error::InvalidCursor);
            }
            if event.follows(self.after)? {
                self.after = event.cursor;
                return Ok(Some(event));
            }
        }
    }
}

/// An SSE subscriber to a mounted `event_router`. Each subscriber has its own connection.
pub struct HttpChatFeed {
    client: Client,
    snapshot_url: Url,
    events_url: Url,
    connection: Option<Connection>,
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl HttpChatFeed {
    pub fn new(base: &str) -> Result<Self> {
        Self::with_client(base, Client::builder().build()?)
    }

    /// Supply a client with the host's authentication headers and network settings.
    pub fn with_client(base: &str, client: Client) -> Result<Self> {
        let base = Url::parse(base)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| Error::Protocol("Expected an HTTP URL for the chat feed".into()))?;
        let endpoint = |name: &str| {
            let mut url = base.clone();
            url.set_fragment(None);
            url.path_segments_mut()
                .expect("HTTP URL")
                .pop_if_empty()
                .push(name);
            url
        };
        Ok(Self {
            client,
            snapshot_url: endpoint("snapshot"),
            events_url: endpoint("events"),
            connection: None,
            retry_at: None,
            backoff: FIRST_RETRY,
        })
    }

    pub async fn snapshot(&self) -> Result<ChatFeedSnapshot> {
        Ok(self
            .client
            .get(self.snapshot_url.clone())
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(reqwest::Error::without_url)?
            .json()
            .await
            .map_err(reqwest::Error::without_url)?)
    }

    fn retry(&mut self) {
        self.connection = None;
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(MAX_RETRY);
    }

    /// Return the next event after the caller's saved position. After updating the view,
    /// pass the new cursor on the next call. Reusing the old cursor replays that event.
    /// The future may be cancelled, including while the server is offline.
    pub async fn recv(&mut self, after: ChatCursor) -> Result<ChatEvent> {
        if self.connection.as_ref().is_some_and(|c| c.after != after) {
            self.connection = None;
        }
        loop {
            if let Some(at) = self.retry_at {
                sleep_until(at).await;
                self.retry_at = None;
            }
            if self.connection.is_none() {
                self.connection = Connection::open(&self.client, &self.events_url, after).await?;
            }
            if let Some(connection) = &mut self.connection {
                match connection.next().await {
                    Ok(Some(event)) => {
                        self.backoff = FIRST_RETRY;
                        return Ok(event);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.connection = None;
                        return Err(e);
                    }
                }
            }
            self.retry();
        }
    }
}
