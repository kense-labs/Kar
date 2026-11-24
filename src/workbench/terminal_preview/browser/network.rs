//! Chromium sends page traffic through a proxy that only recognizes this profile's live grants.
use super::super::preview::{Grant, proxy};
use crate::Result;
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::{net::TcpListener, sync::oneshot};

type Origins = Arc<Mutex<HashMap<String, Weak<Grant>>>>;
pub(super) struct Network {
    pub port: u16,
    origins: Origins,
    _stop: oneshot::Sender<()>,
}
impl Network {
    pub async fn listen() -> Result<Self> {
        let origins = Origins::default();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let (stop, done) = oneshot::channel();
        let app = Router::new().fallback(forward).with_state(origins.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = done.await;
                })
                .await;
        });
        Ok(Self {
            port,
            origins,
            _stop: stop,
        })
    }
    pub fn allow(&self, grant: &Arc<Grant>) {
        let mut origins = self.origins.lock().unwrap();
        origins.retain(|_, grant| grant.strong_count() > 0);
        origins.insert(grant.authority().into(), Arc::downgrade(grant));
    }
}
async fn forward(State(origins): State<Origins>, mut req: Request) -> Response {
    if req.uri().scheme_str() != Some("http") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(grant) = req.uri().authority().and_then(|a| {
        origins
            .lock()
            .unwrap()
            .get(a.as_str())
            .and_then(Weak::upgrade)
    }) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Ok(uri) = req
        .uri()
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .parse::<Uri>()
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *req.uri_mut() = uri;
    proxy(State(grant), req).await
}
