//! Local usage reports. Only normalized counters and provider identities enter the on-disk index.
mod aggregation;
mod pricing;
mod records;
mod scans;
use super::super::http::ApiError;
use super::{Workbench, now, stamp};
use crate::{Error, Result};
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
struct Totals {
    uncached_input: u64,
    cached_input: u64,
    cache_creation: u64,
    output: u64,
    reasoning: u64,
}
impl Totals {
    fn from_array(
        [
            uncached_input,
            cached_input,
            cache_creation,
            output,
            reasoning,
        ]: [u64; 5],
    ) -> Self {
        Self {
            uncached_input,
            cached_input,
            cache_creation,
            output,
            reasoning,
        }
    }
    fn array(self) -> [u64; 5] {
        [
            self.uncached_input,
            self.cached_input,
            self.cache_creation,
            self.output,
            self.reasoning,
        ]
    }
    fn zip(self, other: Self, f: fn(u64, u64) -> u64) -> Self {
        let a = self.array();
        let b = other.array();
        Self::from_array(std::array::from_fn(|i| f(a[i], b[i])))
    }
    fn sum(self) -> u64 {
        self.array()[..4]
            .iter()
            .copied()
            .fold(0, u64::saturating_add)
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Entry {
    timestamp: i64,
    provider: String,
    model: String,
    session: String,
    tokens: Totals,
    cost: Option<f64>,
    one_hour: u64,
    speed: String,
    rate_model: Option<String>,
    key: Option<String>,
}
pub(super) struct Service {
    rates: pricing::Cache,
    index_file: PathBuf,
    latest: Mutex<Option<Arc<scans::Snapshot>>>,
    gate: tokio::sync::Mutex<()>,
}
impl Service {
    pub(super) fn new(dir: &Path) -> Self {
        Self {
            rates: pricing::Cache::new(dir),
            index_file: dir.join("usage-index-v1.json"),
            latest: Default::default(),
            gate: Default::default(),
        }
    }
    async fn scan(&self, chat_dir: PathBuf) -> Result<Arc<scans::Snapshot>> {
        let _g = self.gate.lock().await;
        let previous = self.latest.lock().unwrap().clone();
        let file = self.index_file.clone();
        let next = tokio::task::spawn_blocking(move || scans::read(&file, previous, chat_dir))
            .await
            .map_err(|e| Error::Closed(e.to_string()))??;
        let next = Arc::new(next);
        *self.latest.lock().unwrap() = Some(next.clone());
        Ok(next)
    }
}
pub(super) async fn rates(
    State(app): State<Workbench>,
) -> std::result::Result<Json<Value>, ApiError> {
    let rates = &app.0.observability.usage.rates;
    Ok(Json(rates.get(true).await.view(&rates.url)))
}
pub(super) async fn summary(
    State(app): State<Workbench>,
    Json(input): Json<aggregation::Request>,
) -> std::result::Result<Json<Value>, ApiError> {
    let window = input.window()?;
    let svc = app.0.observability.usage.clone();
    let chat_dir = app.0.chats.directory().to_path_buf();
    let previous = svc.latest.lock().unwrap().clone();
    let provisional = input.provisional && !input.await_refresh && previous.is_some();
    let (snapshot, rates) = if provisional {
        let active = svc.clone();
        if let Ok(guard) = svc.gate.try_lock() {
            drop(guard);
            tokio::spawn(async move {
                let _ = tokio::join!(active.scan(chat_dir), active.rates.get(false));
            });
        }
        (previous.unwrap(), svc.rates.latest())
    } else {
        let (s, r) = tokio::join!(svc.scan(chat_dir), svc.rates.get(false));
        (s?, r)
    };
    let url = svc.rates.url.clone();
    Ok(Json(
        tokio::task::spawn_blocking(move || {
            aggregation::build(&snapshot, &rates, &url, &input, window, provisional)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?,
    ))
}
