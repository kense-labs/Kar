//! Concurrent retries observe the first attempt without occupying another UI queue slot.
use super::*;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};
use tokio::sync::watch;
type Key = (PathBuf, String);
type Shared = Option<std::result::Result<LimitResetReport, String>>;
type Attempts = HashMap<Key, (ResetKind, watch::Sender<Shared>)>;
static ATTEMPTS: LazyLock<Mutex<Attempts>> = LazyLock::new(Default::default);

pub(super) enum Claim {
    First(Active),
    Existing(watch::Receiver<Shared>),
}
pub(super) struct Active {
    key: Key,
}
pub(super) fn claim(dir: PathBuf, key: &str, kind: ResetKind) -> Result<Claim> {
    use std::collections::hash_map::Entry;
    let id = (dir, key.to_owned());
    let mut attempts = ATTEMPTS.lock().unwrap_or_else(|p| p.into_inner());
    match attempts.entry(id.clone()) {
        Entry::Vacant(e) => {
            let (tx, _) = watch::channel(None);
            e.insert((kind, tx));
            Ok(Claim::First(Active { key: id }))
        }
        Entry::Occupied(e) if e.get().0 == kind => Ok(Claim::Existing(e.get().1.subscribe())),
        _ => Err(protocol(
            "This idempotency key already names a different Claude reset kind",
        )),
    }
}
impl Active {
    pub fn finish(&self, result: &Result<LimitResetReport>) {
        if let Some((_, tx)) = ATTEMPTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&self.key)
        {
            tx.send_replace(Some(result.as_ref().cloned().map_err(|_| {
                "Claude limit reset failed. Retry with the same key.".into()
            })));
        }
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        ATTEMPTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.key);
    }
}
pub(super) async fn wait(
    mut rx: watch::Receiver<Shared>,
    end: Instant,
) -> Result<Option<LimitResetReport>> {
    let received = timeout_at(end, rx.wait_for(Option::is_some)).await;
    match received {
        Ok(Ok(value)) => value
            .as_ref()
            .unwrap()
            .clone()
            .map(Some)
            .map_err(|s| protocol(&s)),
        Err(_) => Ok(Some(refused(NotAttemptedReason::Deadline).into())),
        // Owner was cancelled: the durable receipt decides what a retry may do.
        Ok(Err(_)) => Ok(None),
    }
}
