//! Serialize creation of the first native Web token for each Kimi home.
use crate::{Error, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Homes = Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>;
static HOMES: LazyLock<Homes> = LazyLock::new(Default::default);

fn canonical_home(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(base) = ancestor.canonicalize() {
            let full = base.join(path.strip_prefix(ancestor).expect("path ancestor"));
            #[cfg(windows)]
            {
                return PathBuf::from(full.to_string_lossy().to_lowercase());
            }
            #[cfg(not(windows))]
            {
                return full;
            }
        }
    }
    path.into()
}
pub(super) async fn acquire(home: PathBuf) -> Result<Option<OwnedMutexGuard<()>>> {
    let key = tokio::task::spawn_blocking(move || {
        let initialized =
            crate::account::read_profile(&home.join("server.token"), crate::process::OUTPUT_LIMIT)?
                .is_some_and(|s| !s.trim().is_empty());
        Ok::<_, std::io::Error>((!initialized).then(|| canonical_home(&home)))
    })
    .await
    .ok()
    .and_then(|v| v.ok())
    .ok_or_else(|| Error::Protocol("Cannot inspect Kimi Web startup state".into()))?;
    let Some(key) = key else {
        return Ok(None);
    };
    let lock = {
        let mut homes = HOMES.lock().unwrap_or_else(|e| e.into_inner());
        homes.retain(|_, lock| lock.strong_count() > 0);
        let slot = homes.entry(key.clone()).or_default();
        match slot.upgrade() {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(AsyncMutex::new(()));
                *slot = Arc::downgrade(&lock);
                lock
            }
        }
    };
    let guard = lock.lock_owned().await;
    // A caller queued during first-run initialization may now start independently.
    let initialized = tokio::task::spawn_blocking(move || {
        crate::account::read_profile(&key.join("server.token"), crate::process::OUTPUT_LIMIT)
    })
    .await
    .ok()
    .and_then(|v| v.ok())
    .ok_or_else(|| Error::Protocol("Cannot inspect Kimi Web startup state".into()))?
    .is_some_and(|s| !s.trim().is_empty());
    Ok((!initialized).then_some(guard))
}
