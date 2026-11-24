use super::*;
use crate::WorkbenchRegistration;
use crate::{McpServer, SessionOptions};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const IDLE: Duration = Duration::from_secs(24 * 3600);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Identity {
    Owner,
    Device(Uuid),
    External(Uuid),
    Provider(Uuid),
}

pub(super) struct Grant {
    pub id: Uuid,
    pub project: Uuid,
    pub chat: Uuid,
    pub runtime: String,
    pub loaded: Mutex<std::collections::BTreeSet<&'static str>>,
    pub changes: watch::Sender<u64>,
    pub revoked: CancellationToken,
    clock: Mutex<(Instant, u64, bool)>,
    process: OnceLock<Weak<crate::Session>>,
}
impl Grant {
    fn idle(&self) -> Duration {
        let mut clock = self.clock.lock().unwrap();
        let elapsed = clock
            .0
            .elapsed()
            .max(Duration::from_millis(access::now().saturating_sub(clock.1)));
        clock.2 |= elapsed >= IDLE;
        if clock.2 {
            Duration::ZERO
        } else {
            IDLE.saturating_sub(elapsed)
        }
    }
    pub(super) fn caller_turn(&self) -> Option<Uuid> {
        self.process.get()?.upgrade()?.model_caller_turn()
    }
    pub fn active(&self) -> bool {
        !self.revoked.is_cancelled()
            && !self.idle().is_zero()
            && self
                .process
                .get()
                .is_none_or(|p| p.upgrade().is_some_and(|s| !s.is_closed()))
    }
    pub async fn expired(self: Arc<Self>) {
        while self.active() {
            tokio::select! {
                _ = self.revoked.cancelled() => return,
                _ = tokio::time::sleep(self.idle().min(Duration::from_secs(5))) => {}
            }
        }
    }
}
pub(in crate::workbench) struct Lease {
    grant: Arc<Grant>,
    registration: WorkbenchRegistration,
}
impl Lease {
    pub(in crate::workbench) fn configure(&self, opts: &mut SessionOptions) {
        if !opts.mcp_servers.contains_key(WorkbenchRegistration::NAME) {
            opts.workbench_registration = Some(self.registration.clone());
        }
    }
    pub(in crate::workbench) fn attach(&self, chat: &mut crate::chat::ChatSession) {
        chat.forget_workbench_registration();
        let _ = self.grant.process.set(chat.live_session());
    }
    pub(super) fn tools_changed(&self) {
        self.grant.changes.send_modify(|n| *n += 1);
    }
    pub(in crate::workbench) fn touch(&self) {
        *self.grant.clock.lock().unwrap() = (Instant::now(), access::now(), false);
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.grant.revoked.cancel();
    }
}

pub(super) struct HttpSession {
    pub identity: Identity,
    pub closed: CancellationToken,
    last: Mutex<Instant>,
}
impl Drop for HttpSession {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

#[derive(Default)]
pub(in crate::workbench) struct Registry {
    endpoint: OnceLock<SocketAddr>,
    commands: Mutex<HashMap<Uuid, Weak<tokio::sync::Mutex<()>>>>,
    grants: Mutex<HashMap<String, Weak<Grant>>>,
    sessions: Mutex<HashMap<Uuid, Arc<HttpSession>>>,
}
impl Registry {
    pub(in crate::workbench) fn origin(&self)->Option<String>{self.endpoint.get().map(|a|format!("http://{a}"))}
    pub(super) fn command_gate(&self, id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.commands.lock().unwrap();
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(id, Arc::downgrade(&gate));
        gate
    }

    pub(in crate::workbench) fn set_endpoint(&self, addr: SocketAddr) -> crate::Result<()> {
        if !addr.ip().is_loopback()
            || addr.port() == 0
            || *self.endpoint.get_or_init(|| addr) != addr
        {
            return Err(crate::Error::Refused(
                "Kar requires its bound loopback address for local model tools".into(),
            ));
        }
        Ok(())
    }
    pub(in crate::workbench) fn issue(
        &self,
        project: Uuid,
        chat: Uuid,
        runtime: &str,
    ) -> crate::Result<Option<Lease>> {
        let Some(addr) = self.endpoint.get() else {
            return Ok(None);
        };
        if !crate::runtimes().iter().any(|r| r.id == runtime) {
            return Ok(None);
        }
        let raw = access::credential()?;
        let grant = Arc::new(Grant {
            id: Uuid::new_v4(),
            project,
            chat,
            runtime: runtime.into(),
            loaded: Default::default(),
            changes: watch::channel(0).0,
            revoked: CancellationToken::new(),
            process: OnceLock::new(),
            clock: Mutex::new((Instant::now(), access::now(), false)),
        });
        let mut grants = self.grants.lock().unwrap();
        grants.retain(|_, g| g.strong_count() > 0);
        grants.insert(access::digest(&raw), Arc::downgrade(&grant));
        drop(grants);
        #[allow(unused_mut)]
        let mut registration = WorkbenchRegistration::new(McpServer::Http {
            url: format!("http://{addr}/mcp"),
            headers: BTreeMap::from([
                ("Authorization".into(), format!("Bearer {raw}")),
                ("Kar-Tool-Exposure".into(), "deferred".into()),
            ]),
        });
        #[cfg(target_os = "macos")]
        if matches!(
            runtime,
            "dsh"
                | "devin"
                | "cursor"
                | "antigravity"
                | "kiro"
                | "fx"
                | "opencode"
                | "kimi"
                | "codebuddy"
                | "grok"
                | "qwen"
        ) {
            registration.stdio =
                super::stdio_bridge::listen(*addr, raw, &grant, &registration).ok();
        }
        Ok(Some(Lease {
            grant,
            registration,
        }))
    }
    pub(super) fn resolve(&self, key: &str) -> Option<Arc<Grant>> {
        self.grants
            .lock()
            .unwrap()
            .get(&access::digest(key))?
            .upgrade()
            .filter(|g| g.active())
    }
    pub(super) fn open(&self, who: Identity) -> Option<Uuid> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| {
            let keep = s.last.lock().unwrap().elapsed() < IDLE;
            if !keep {
                s.closed.cancel()
            }
            keep
        });
        if sessions.len() >= 4096 {
            return None;
        }
        let id = Uuid::new_v4();
        sessions.insert(
            id,
            Arc::new(HttpSession {
                identity: who,
                closed: CancellationToken::new(),
                last: Mutex::new(Instant::now()),
            }),
        );
        Some(id)
    }
    pub(super) fn session(&self, who: Identity, id: Uuid) -> Option<Arc<HttpSession>> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get(&id)?.clone();
        if s.identity != who {
            return None;
        }
        let mut last = s.last.lock().unwrap();
        if last.elapsed() >= IDLE {
            s.closed.cancel();
            sessions.remove(&id);
            return None;
        }
        *last = Instant::now();
        drop(last);
        Some(s)
    }
    pub(super) fn close(&self, who: Identity, id: Uuid) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.get(&id).is_none_or(|s| s.identity != who) {
            return false;
        }
        if let Some(s) = sessions.remove(&id) {
            s.closed.cancel();
        }
        true
    }
}
