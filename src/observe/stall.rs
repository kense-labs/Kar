use super::{AgentStatus, Phase, SessionReducer};
use crate::{Cursor, Record, RecordKind, Result, Session, Subscription};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallInfo {
    pub since_seq: u64,
    pub silent_for_ms: u64,
    pub phase: Phase,
    pub last_record: Cursor,
    pub last_record_kind: String,
}

/// A passive observer. Each continuous silence period produces at most one report.
pub struct StallSubscription {
    records: Subscription,
    reducer: SessionReducer,
    last: Option<Arc<Record>>,
    threshold: Duration,
    reported: bool,
    rearmed_at: Option<tokio::time::Instant>,
}
impl StallSubscription {
    pub(crate) fn new(session: &Session, threshold: Duration) -> Result<Self> {
        let mut reducer = SessionReducer::new(session.id());
        let records = session.records();
        for r in &records {
            reducer.apply(r)?;
        }
        let last = records.last().cloned();
        let records = session.subscribe(last.as_ref().map(|r| r.cursor()).as_ref())?;
        Ok(Self {
            records,
            reducer,
            last,
            threshold,
            reported: false,
            rearmed_at: None,
        })
    }
    /// Cancellation-safe. Dropping this observer leaves native work running.
    pub async fn next(&mut self) -> Result<Option<StallInfo>> {
        loop {
            let status = &self.reducer.snapshot().status;
            let record = if let AgentStatus::Running {
                last_event_ms,
                since_seq,
                phase,
                ..
            } = status
                && !self.reported
            {
                let now = crate::stream::timestamp_ms();
                let remaining = if now < *last_event_ms {
                    self.threshold
                        .saturating_add(Duration::from_millis(*last_event_ms - now))
                } else {
                    self.threshold
                        .saturating_sub(Duration::from_millis(now - *last_event_ms))
                };
                let quiet = self.rearmed_at.map_or(Duration::ZERO, |at| {
                    self.threshold.saturating_sub(at.elapsed())
                });
                // Long thresholds are re-evaluated without overflowing the platform timer.
                let delay = remaining.max(quiet).min(Duration::from_secs(86400));
                tokio::select! {
                    biased;
                    next=self.records.next()=>next,
                    _=tokio::time::sleep(delay)=>{
                        if self.rearmed_at.is_some_and(|at|at.elapsed()<self.threshold){continue;}
                        let Some(silent_for_ms)=status.silent_for(crate::stream::timestamp_ms(),self.threshold) else {continue;};
                        if let Some(last)=&self.last {
                            self.reported=true;
                            let kind=match last.kind {
                                RecordKind::Frame=>last.events.last().and_then(|e|serde_json::to_value(e).ok()).and_then(|e|e["kind"].as_str().map(|k|format!("frame:{k}"))).unwrap_or_else(||"frame".into()),
                                RecordKind::Request=>"request".into(),
                                RecordKind::Response=>"response".into(),
                            };
                            return Ok(Some(StallInfo{since_seq:*since_seq,silent_for_ms,phase:phase.clone(),last_record:last.cursor(),last_record_kind:kind}));
                        }
                        continue;
                    }
                }
            } else {
                self.records.next().await
            };
            let Some(record) = record else {
                return Ok(None);
            };
            self.reducer.apply(&record)?;
            self.last = Some(record);
            self.reported = false;
            self.rearmed_at = Some(tokio::time::Instant::now());
        }
    }
}
