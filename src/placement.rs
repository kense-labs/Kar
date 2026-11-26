//! A runtime's requested placement and the placement observed for one session.
use crate::{Direction, Error, Record, RecordKind, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    Process,
    InProcess,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementRequest {
    #[default]
    Auto,
    Shared,
    Isolated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlacement {
    Isolated,
    Shared,
    InProcess,
}

impl PlacementRequest {
    /// Returns true when `runtime_id` supports multiplexing multiple Kar sessions
    /// over a shared native daemon process (`PlacementRequest::Shared`).
    pub fn supports_shared_process(runtime_id: &str) -> bool {
        matches!(runtime_id, "opencode" | "grok")
    }

    /// Resolve the effective session placement for a specific runtime identifier.
    ///
    /// Runtimes with verified shared daemon multiplexing (`"opencode"` and `"grok"`)
    /// resolve `(Some(Execution::Process), PlacementRequest::Shared)` to
    /// `Ok(Some(SessionPlacement::Shared))`, while single-session stdio runtimes
    /// return `Err(Error::Unsupported(_))`.
    pub fn resolve_for_runtime(
        self,
        runtime_id: &str,
        execution: Option<Execution>,
    ) -> Result<Option<SessionPlacement>> {
        match (execution, self) {
            (Some(Execution::Process), Self::Auto | Self::Isolated) => {
                Ok(Some(SessionPlacement::Isolated))
            }
            (Some(Execution::Process), Self::Shared)
                if Self::supports_shared_process(runtime_id) =>
            {
                Ok(Some(SessionPlacement::Shared))
            }
            (Some(Execution::InProcess), Self::Auto) => Ok(Some(SessionPlacement::InProcess)),
            (None, Self::Auto) => Ok(None),
            (Some(Execution::Process), Self::Shared) => Err(Error::Unsupported(
                "placement: this runtime has no verified shared process implementation".into(),
            )),
            (Some(Execution::InProcess), _) => Err(Error::Unsupported(
                "placement: this runtime executes in the host process".into(),
            )),
            (None, _) => Err(Error::Unsupported(
                "placement: this runtime does not control native process placement".into(),
            )),
        }
    }

    pub(crate) fn resolve(self, execution: Option<Execution>) -> Result<Option<SessionPlacement>> {
        self.resolve_for_runtime("", execution)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Opening {
    Open {
        request_id: String,
        placement: PlacementRequest,
    },
    Opened {
        placement: SessionPlacement,
    },
}

impl SessionPlacement {
    /// Read the paired opening records of a live or recorded stream. Older
    /// recordings and adapters without an execution declaration have no pair.
    pub fn from_records<'a>(records: impl IntoIterator<Item = &'a Record>) -> Option<Self> {
        let mut records = records.into_iter();
        Self::from_pair(records.next()?, records.next()?)
    }

    pub(crate) fn from_pair(first: &Record, second: &Record) -> Option<Self> {
        let Opening::Open { request_id, .. } = first.opening.as_ref()? else {
            return None;
        };
        let Opening::Opened { placement } = second.opening.as_ref()? else {
            return None;
        };
        (!request_id.is_empty()
            && first.agent_path.is_empty()
            && second.agent_path.is_empty()
            && first.seq == 0
            && second.seq == 1
            && first.stream_id == second.stream_id
            && first.kind == RecordKind::Request
            && first.direction == Direction::ToRuntime
            && second.kind == RecordKind::Response
            && second.response_to.as_deref() == Some(request_id)
            && first.session_id == second.session_id)
            .then_some(*placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Journal;
    #[test]
    fn paired_opening_is_distinct_from_legacy_or_unrelated_records() {
        let old = Journal::new(None).unwrap();
        old.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            serde_json::json!({"type":"ready"}),
            vec![],
        );
        assert_eq!(old.placement(), None);
        let opened =
            Journal::for_open(Some(1), PlacementRequest::Auto, Some(Execution::InProcess)).unwrap();
        opened.bind_root("original");
        assert_eq!(opened.placement(), Some(SessionPlacement::InProcess));
        let records = opened.records();
        let mut wrong = (*records[1]).clone();
        wrong.response_to = Some("unrelated".into());
        assert_eq!(
            SessionPlacement::from_records([records[0].as_ref(), &wrong]),
            None
        );
        assert!(
            PlacementRequest::Isolated
                .resolve(Some(Execution::InProcess))
                .is_err()
        );
        assert!(PlacementRequest::Shared.resolve(None).is_err());
        assert_eq!(PlacementRequest::Auto.resolve(None).unwrap(), None);
    }
}

#[cfg(test)]
mod acp_admission_tests {
    use super::*;

    #[tokio::test]
    async fn production_shared_refusals_precede_open_resume_and_fork() {
        let dir = tempfile::tempdir().unwrap();
        for (name, reason) in [
            ("opencode", "configured plugins"),
            ("grok", "configured stdio MCP servers"),
        ] {
            let mut rt = crate::runtime(name)
                .unwrap()
                .with_placement(PlacementRequest::Shared);
            rt.program = dir.path().join("never-started");
            let opts = crate::SessionOptions::new(dir.path());
            let mut resume = opts.clone();
            resume.resume = Some("previous".into());
            for result in [
                rt.open(opts.clone()).await,
                rt.open(resume).await,
                rt.fork("parent", None, opts).await,
            ] {
                assert!(
                    matches!(result, Err(Error::Unsupported(detail)) if detail.contains("placement") && detail.contains(reason))
                );
            }
        }
    }
}

/// A controller released its lease. The native task may still be running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Dispose,
    AbortFallback,
    ConnectionLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRelease {
    pub reason: ReleaseReason,
    #[serde(alias = "nativeTurnMayContinue")]
    pub native_turn_may_continue: bool,
    #[serde(alias = "workStartedAfterCleanupMayContinue")]
    pub work_started_after_cleanup_may_continue: bool,
}
impl SessionRelease {
    pub fn new(reason: ReleaseReason) -> Self {
        Self {
            reason,
            native_turn_may_continue: true,
            work_started_after_cleanup_may_continue: true,
        }
    }
}
impl crate::Record {
    pub fn session_release(&self) -> Option<SessionRelease> {
        if !self.events.contains(&crate::Event::Released) {
            return None;
        }
        let native = self.native.get("body").unwrap_or(&self.native);
        serde_json::from_value(native.get("release").unwrap_or(native).clone()).ok()
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod shared_pool {
    use crate::Result;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::{
        collections::HashMap,
        future::Future,
        ops::Deref,
        path::PathBuf,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };
    use tokio::task::AbortHandle;

    /// Partition key isolating shared daemon instances by runtime, binary, workspace, and env.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    pub struct SharedPoolKey {
        pub runtime_id: String,
        pub executable: PathBuf,
        pub cwd: PathBuf,
        pub env_fingerprint: String,
    }

    impl SharedPoolKey {
        pub fn new(
            runtime_id: impl Into<String>,
            executable: impl Into<PathBuf>,
            cwd: impl Into<PathBuf>,
            env_fingerprint: impl Into<String>,
        ) -> Self {
            Self {
                runtime_id: runtime_id.into(),
                executable: executable.into(),
                cwd: cwd.into(),
                env_fingerprint: env_fingerprint.into(),
            }
        }

        /// Compute a collision-free SHA-256 fingerprint over environment overrides, removals, and launch args.
        pub fn fingerprint_env<'a, K, V>(
            env: impl IntoIterator<Item = (K, V)>,
            unset: impl IntoIterator<Item = &'a str>,
            launch_args: &[String],
        ) -> String
        where
            K: AsRef<[u8]>,
            V: AsRef<[u8]>,
        {
            let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = env
                .into_iter()
                .map(|(k, v)| (k.as_ref().to_vec(), v.as_ref().to_vec()))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

            let mut removed: Vec<&[u8]> = unset.into_iter().map(str::as_bytes).collect();
            removed.sort_unstable();
            removed.dedup();

            let mut hasher = Sha256::new();
            hasher.update(b"kar-shared-pool-v1\0");
            hasher.update((pairs.len() as u64).to_be_bytes());
            for (k, v) in &pairs {
                hasher.update((k.len() as u64).to_be_bytes());
                hasher.update(k);
                hasher.update((v.len() as u64).to_be_bytes());
                hasher.update(v);
            }
            hasher.update((removed.len() as u64).to_be_bytes());
            for name in removed {
                hasher.update((name.len() as u64).to_be_bytes());
                hasher.update(name);
            }
            hasher.update((launch_args.len() as u64).to_be_bytes());
            for arg in launch_args {
                hasher.update((arg.len() as u64).to_be_bytes());
                hasher.update(arg.as_bytes());
            }
            format!("{:x}", hasher.finalize())
        }

        /// Build a `SharedPoolKey` from a runtime program and `SessionOptions`.
        pub fn for_session(
            runtime_id: impl Into<String>,
            executable: impl Into<PathBuf>,
            options: &crate::SessionOptions,
        ) -> Self {
            let executable = executable.into();
            let canonical_exe = std::fs::canonicalize(&executable).unwrap_or(executable);
            let canonical_cwd =
                std::fs::canonicalize(&options.cwd).unwrap_or_else(|_| options.cwd.clone());
            let env_fingerprint = Self::fingerprint_env(
                options
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_encoded_bytes(), v.as_encoded_bytes())),
                options.unset_env.iter().map(String::as_str),
                &options.launch_args,
            );
            Self {
                runtime_id: runtime_id.into(),
                executable: canonical_exe,
                cwd: canonical_cwd,
                env_fingerprint,
            }
        }
    }

    /// Lifecycle hooks implemented by a pooled daemon process or multiplexed connection.
    pub trait PooledDaemon: Send + Sync + 'static {
        /// Returns `true` while the underlying daemon process and transport remain usable.
        fn is_alive(&self) -> bool;

        /// Gracefully stop the underlying daemon once all session leases have drained.
        fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    }

    /// Disposition after releasing one session lease from a [`SharedDaemonPool`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PoolReleaseOutcome {
        /// Other sessions still hold active leases on this daemon.
        Active { refs: usize },
        /// Reference count reached zero and the idle reaper timer is armed.
        IdleScheduled,
        /// Reference count reached zero and the daemon was stopped immediately.
        Stopped,
    }

    struct Entry<D> {
        id: u64,
        daemon: Arc<D>,
        refs: usize,
        retired: bool,
        revision: u64,
        idle_abort: Option<AbortHandle>,
    }

    struct Inner<D: PooledDaemon> {
        idle_timeout: Duration,
        next_id: AtomicU64,
        slots: Mutex<HashMap<SharedPoolKey, Entry<D>>>,
        init_gates: Mutex<HashMap<SharedPoolKey, Arc<tokio::sync::Mutex<()>>>>,
    }

    /// Thread-safe registry multiplexing compatible sessions onto shared daemon processes
    /// with concurrent initialization deduplication, reference counting, and idle reaping.
    pub struct SharedDaemonPool<D: PooledDaemon> {
        inner: Arc<Inner<D>>,
    }

    impl<D: PooledDaemon> Clone for SharedDaemonPool<D> {
        fn clone(&self) -> Self {
            Self {
                inner: Arc::clone(&self.inner),
            }
        }
    }

    impl<D: PooledDaemon> Default for SharedDaemonPool<D> {
        fn default() -> Self {
            Self::new(Self::DEFAULT_IDLE_TIMEOUT)
        }
    }

    impl<D: PooledDaemon> SharedDaemonPool<D> {
        pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

        pub fn new(idle_timeout: Duration) -> Self {
            Self {
                inner: Arc::new(Inner {
                    idle_timeout,
                    next_id: AtomicU64::new(1),
                    slots: Mutex::new(HashMap::new()),
                    init_gates: Mutex::new(HashMap::new()),
                }),
            }
        }

        pub fn idle_timeout(&self) -> Duration {
            self.inner.idle_timeout
        }

        /// Number of live or idle-warm daemon entries currently tracked in the pool.
        pub fn active_daemons(&self) -> usize {
            self.inner.slots.lock().unwrap().len()
        }

        /// Active lease reference count for `key` (`0` if idle-warm or absent).
        pub fn active_leases(&self, key: &SharedPoolKey) -> usize {
            self.inner
                .slots
                .lock()
                .unwrap()
                .get(key)
                .map_or(0, |entry| entry.refs)
        }

        fn try_checkout_existing(
            &self,
            key: &SharedPoolKey,
        ) -> (Option<SharedDaemonLease<D>>, Option<Arc<D>>) {
            let mut slots = self.inner.slots.lock().unwrap();
            let usable = slots
                .get(key)
                .is_some_and(|entry| !entry.retired && entry.daemon.is_alive());
            if usable {
                let entry = slots.get_mut(key).expect("checked present");
                if let Some(abort) = entry.idle_abort.take() {
                    abort.abort();
                }
                entry.refs += 1;
                entry.revision += 1;
                let lease = SharedDaemonLease {
                    pool: self.clone(),
                    key: key.clone(),
                    entry_id: entry.id,
                    daemon: Arc::clone(&entry.daemon),
                    released: false,
                };
                return (Some(lease), None);
            }
            let stale = slots.remove(key).and_then(|mut entry| {
                if let Some(abort) = entry.idle_abort.take() {
                    abort.abort();
                }
                (entry.refs == 0).then_some(entry.daemon)
            });
            (None, stale)
        }

        /// Acquire a lease for `key`, reusing an active or idle-warm daemon when healthy
        /// or deduplicating concurrent `init` calls when starting a fresh daemon.
        pub async fn acquire<F, Fut>(
            &self,
            key: SharedPoolKey,
            init: F,
        ) -> Result<SharedDaemonLease<D>>
        where
            F: FnOnce() -> Fut,
            Fut: Future<Output = Result<D>>,
        {
            let (existing, stale) = self.try_checkout_existing(&key);
            if let Some(stale) = stale {
                let _ = stale.shutdown().await;
            }
            if let Some(lease) = existing {
                return Ok(lease);
            }

            let gate = {
                let mut gates = self.inner.init_gates.lock().unwrap();
                Arc::clone(
                    gates
                        .entry(key.clone())
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
                )
            };
            let _init_guard = gate.lock().await;

            let (existing, stale) = self.try_checkout_existing(&key);
            if let Some(stale) = stale {
                let _ = stale.shutdown().await;
            }
            if let Some(lease) = existing {
                return Ok(lease);
            }

            let daemon = Arc::new(init().await?);
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            {
                let mut slots = self.inner.slots.lock().unwrap();
                slots.insert(
                    key.clone(),
                    Entry {
                        id,
                        daemon: Arc::clone(&daemon),
                        refs: 1,
                        retired: false,
                        revision: 1,
                        idle_abort: None,
                    },
                );
            }
            {
                let mut gates = self.inner.init_gates.lock().unwrap();
                if gates
                    .get(&key)
                    .is_some_and(|stored| Arc::strong_count(stored) <= 2)
                {
                    gates.remove(&key);
                }
            }
            Ok(SharedDaemonLease {
                pool: self.clone(),
                key,
                entry_id: id,
                daemon,
                released: false,
            })
        }

        /// Mark `key` retired; if idle (`refs == 0`), remove and shut it down immediately.
        pub async fn evict(&self, key: &SharedPoolKey) -> Result<bool> {
            let stop_now = {
                let mut slots = self.inner.slots.lock().unwrap();
                match slots.get_mut(key) {
                    None => return Ok(false),
                    Some(entry) if entry.refs == 0 => {
                        let mut removed = slots.remove(key).expect("checked present");
                        if let Some(abort) = removed.idle_abort.take() {
                            abort.abort();
                        }
                        Some(removed.daemon)
                    }
                    Some(entry) => {
                        entry.retired = true;
                        None
                    }
                }
            };
            if let Some(daemon) = stop_now {
                daemon.shutdown().await?;
            }
            Ok(true)
        }

        /// Retire all tracked daemons and shut down all currently idle entries.
        pub async fn shutdown_all(&self) -> Result<usize> {
            let idle: Vec<Arc<D>> = {
                let mut slots = self.inner.slots.lock().unwrap();
                let idle_keys: Vec<SharedPoolKey> = slots
                    .iter_mut()
                    .filter_map(|(k, entry)| {
                        entry.retired = true;
                        if let Some(abort) = entry.idle_abort.take() {
                            abort.abort();
                        }
                        (entry.refs == 0).then(|| k.clone())
                    })
                    .collect();
                idle_keys
                    .into_iter()
                    .filter_map(|k| slots.remove(&k).map(|e| e.daemon))
                    .collect()
            };
            let count = idle.len();
            for daemon in idle {
                daemon.shutdown().await?;
            }
            Ok(count)
        }

        fn release_sync(&self, key: &SharedPoolKey, entry_id: u64) -> ReleaseTransition<D> {
            let mut slots = self.inner.slots.lock().unwrap();
            let Some(entry) = slots.get_mut(key) else {
                return ReleaseTransition::AlreadyStopped;
            };
            if entry.id != entry_id {
                return ReleaseTransition::AlreadyStopped;
            }
            entry.refs = entry.refs.saturating_sub(1);
            entry.revision += 1;
            if entry.refs > 0 {
                return ReleaseTransition::StillActive(entry.refs);
            }
            if self.inner.idle_timeout.is_zero() || entry.retired || !entry.daemon.is_alive() {
                let mut removed = slots.remove(key).expect("checked present");
                if let Some(abort) = removed.idle_abort.take() {
                    abort.abort();
                }
                return ReleaseTransition::StopImmediate(removed.daemon);
            }
            let expected_revision = entry.revision;
            let timeout = self.inner.idle_timeout;
            let pool = self.clone();
            let key_clone = key.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let join = handle.spawn(async move {
                    tokio::time::sleep(timeout).await;
                    let victim = {
                        let mut slots = pool.inner.slots.lock().unwrap();
                        let should_reap = slots.get(&key_clone).is_some_and(|current| {
                            current.id == entry_id
                                && current.refs == 0
                                && current.revision == expected_revision
                        });
                        if should_reap {
                            slots.remove(&key_clone).map(|e| e.daemon)
                        } else {
                            None
                        }
                    };
                    if let Some(daemon) = victim {
                        let _ = daemon.shutdown().await;
                    }
                });
                entry.idle_abort = Some(join.abort_handle());
                ReleaseTransition::ScheduledIdle
            } else {
                let removed = slots.remove(key).expect("checked present");
                ReleaseTransition::StopImmediate(removed.daemon)
            }
        }
    }

    enum ReleaseTransition<D> {
        StillActive(usize),
        ScheduledIdle,
        StopImmediate(Arc<D>),
        AlreadyStopped,
    }

    /// Reference-counted lease over a shared daemon acquired from a [`SharedDaemonPool`].
    pub struct SharedDaemonLease<D: PooledDaemon> {
        pool: SharedDaemonPool<D>,
        key: SharedPoolKey,
        entry_id: u64,
        daemon: Arc<D>,
        released: bool,
    }

    impl<D: PooledDaemon> SharedDaemonLease<D> {
        pub fn daemon(&self) -> &Arc<D> {
            &self.daemon
        }

        pub fn key(&self) -> &SharedPoolKey {
            &self.key
        }

        /// Prevent new sessions from attaching to this daemon; once all existing leases
        /// release, the daemon stops immediately without waiting for the idle reaper.
        pub fn retire(&self) {
            let mut slots = self.pool.inner.slots.lock().unwrap();
            if let Some(entry) = slots.get_mut(&self.key)
                && entry.id == self.entry_id
            {
                entry.retired = true;
            }
        }

        /// Explicitly release this lease and await daemon shutdown if this was the last
        /// reference on a zero-timeout, retired, or unhealthy daemon.
        pub async fn release(mut self) -> Result<PoolReleaseOutcome> {
            self.released = true;
            match self.pool.release_sync(&self.key, self.entry_id) {
                ReleaseTransition::StillActive(refs) => Ok(PoolReleaseOutcome::Active { refs }),
                ReleaseTransition::ScheduledIdle => Ok(PoolReleaseOutcome::IdleScheduled),
                ReleaseTransition::StopImmediate(daemon) => {
                    daemon.shutdown().await?;
                    Ok(PoolReleaseOutcome::Stopped)
                }
                ReleaseTransition::AlreadyStopped => Ok(PoolReleaseOutcome::Stopped),
            }
        }
    }

    impl<D: PooledDaemon> Deref for SharedDaemonLease<D> {
        type Target = D;

        fn deref(&self) -> &Self::Target {
            &self.daemon
        }
    }

    impl<D: PooledDaemon> Drop for SharedDaemonLease<D> {
        fn drop(&mut self) {
            if self.released {
                return;
            }
            self.released = true;
            if let ReleaseTransition::StopImmediate(daemon) =
                self.pool.release_sync(&self.key, self.entry_id)
                && let Ok(handle) = tokio::runtime::Handle::try_current()
            {
                handle.spawn(async move {
                    let _ = daemon.shutdown().await;
                });
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use shared_pool::{
    PoolReleaseOutcome, PooledDaemon, SharedDaemonLease, SharedDaemonPool, SharedPoolKey,
};

#[cfg(all(test, not(target_arch = "wasm32")))]
mod shared_pool_tests {
    use super::*;
    use crate::stream::Journal;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    struct FakeDaemon {
        id: usize,
        alive: AtomicBool,
        shutdowns: Arc<AtomicUsize>,
    }

    impl PooledDaemon for FakeDaemon {
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::Acquire)
        }

        fn shutdown(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.alive.store(false, Ordering::Release);
                self.shutdowns.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        }
    }

    #[test]
    fn resolve_for_runtime_permits_multiplexed_daemons_and_rejects_stdio_runtimes() {
        for runtime_id in ["opencode", "grok"] {
            assert_eq!(
                PlacementRequest::Shared
                    .resolve_for_runtime(runtime_id, Some(Execution::Process))
                    .unwrap(),
                Some(SessionPlacement::Shared)
            );
            let journal = Journal::for_open_runtime(
                None,
                runtime_id,
                PlacementRequest::Shared,
                Some(Execution::Process),
            )
            .unwrap();
            journal.bind_root("shared-root");
            assert_eq!(journal.placement(), Some(SessionPlacement::Shared));
        }

        for single_session in ["codex", "claude", "pi", "dsh", "factory", ""] {
            assert!(matches!(
                PlacementRequest::Shared
                    .resolve_for_runtime(single_session, Some(Execution::Process)),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn pool_key_fingerprints_env_unset_and_args_without_boundary_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts_a = crate::SessionOptions::new(dir.path());
        opts_a.env.insert("AB".into(), "C".into());
        let mut opts_b = crate::SessionOptions::new(dir.path());
        opts_b.env.insert("A".into(), "BC".into());

        let key_a = SharedPoolKey::for_session("opencode", "opencode", &opts_a);
        let key_b = SharedPoolKey::for_session("opencode", "opencode", &opts_b);
        assert_ne!(key_a.env_fingerprint, key_b.env_fingerprint);

        let mut opts_c = opts_a.clone();
        opts_c.unset_env.insert("OPENAI_API_KEY".into());
        let key_c = SharedPoolKey::for_session("opencode", "opencode", &opts_c);
        assert_ne!(key_a.env_fingerprint, key_c.env_fingerprint);
    }

    #[tokio::test]
    async fn pool_deduplicates_concurrent_spawns_reuses_warm_daemons_and_reaps_idle() {
        let pool = SharedDaemonPool::<FakeDaemon>::new(Duration::from_millis(45));
        let spawns = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let key = SharedPoolKey::new("opencode", "/usr/bin/opencode", "/tmp/ws", "fp-1");

        let acquire_one = |pool: SharedDaemonPool<FakeDaemon>,
                           key: SharedPoolKey,
                           spawns: Arc<AtomicUsize>,
                           shutdowns: Arc<AtomicUsize>| async move {
            pool.acquire(key, || async move {
                tokio::time::sleep(Duration::from_millis(15)).await;
                let id = spawns.fetch_add(1, Ordering::AcqRel) + 1;
                Ok(FakeDaemon {
                    id,
                    alive: AtomicBool::new(true),
                    shutdowns,
                })
            })
            .await
            .unwrap()
        };

        // Two concurrent acquires for the same key only spawn 1 daemon and share id == 1.
        let (lease1, lease2) = tokio::join!(
            acquire_one(
                pool.clone(),
                key.clone(),
                Arc::clone(&spawns),
                Arc::clone(&shutdowns)
            ),
            acquire_one(
                pool.clone(),
                key.clone(),
                Arc::clone(&spawns),
                Arc::clone(&shutdowns)
            ),
        );
        assert_eq!(spawns.load(Ordering::Acquire), 1);
        assert_eq!(lease1.id, 1);
        assert_eq!(lease2.id, 1);
        assert_eq!(pool.active_leases(&key), 2);

        // Releasing first lease leaves 1 active ref.
        assert_eq!(
            lease1.release().await.unwrap(),
            PoolReleaseOutcome::Active { refs: 1 }
        );
        assert_eq!(pool.active_leases(&key), 1);

        // Releasing second lease schedules idle reaper without immediate shutdown.
        assert_eq!(
            lease2.release().await.unwrap(),
            PoolReleaseOutcome::IdleScheduled
        );
        assert_eq!(shutdowns.load(Ordering::Acquire), 0);

        // Re-acquiring before idle timeout reuses warm daemon #1 and cancels reaper!
        tokio::time::sleep(Duration::from_millis(10)).await;
        let lease3 = acquire_one(
            pool.clone(),
            key.clone(),
            Arc::clone(&spawns),
            Arc::clone(&shutdowns),
        )
        .await;
        assert_eq!(lease3.id, 1);
        assert_eq!(spawns.load(Ordering::Acquire), 1);

        // Wait past original 45ms window while lease3 is held: daemon must stay alive!
        tokio::time::sleep(Duration::from_millis(55)).await;
        assert_eq!(shutdowns.load(Ordering::Acquire), 0);
        assert!(lease3.is_alive());

        // Release lease3 and let idle reaper fire.
        assert_eq!(
            lease3.release().await.unwrap(),
            PoolReleaseOutcome::IdleScheduled
        );
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(shutdowns.load(Ordering::Acquire), 1);
        assert_eq!(pool.active_daemons(), 0);

        // Next acquire spawns fresh daemon #2; marking it unhealthy forces replacement #3.
        let lease4 = acquire_one(
            pool.clone(),
            key.clone(),
            Arc::clone(&spawns),
            Arc::clone(&shutdowns),
        )
        .await;
        assert_eq!(lease4.id, 2);
        lease4.alive.store(false, Ordering::Release);
        assert_eq!(lease4.release().await.unwrap(), PoolReleaseOutcome::Stopped);
        assert_eq!(shutdowns.load(Ordering::Acquire), 2);
    }
}
