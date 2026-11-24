//! Admission epochs for account changes on one Kar host, including chats opening during sign-out.
use crate::{Error, Result, Session, SessionOptions};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    runtime: String,
    home: Option<PathBuf>,
}
impl Key {
    pub(crate) fn new(runtime: &str, options: &SessionOptions) -> Self {
        let home_env = match runtime {
            "codex" => Some(("CODEX_HOME", ".codex")),
            "claude" => Some(("CLAUDE_CONFIG_DIR", ".claude")),
            "pi" => Some(("KAR_PI_AGENT_DIR", ".pi/agent")),
            _ => None,
        };
        let home = home_env
            .and_then(|(key, default)| {
                options
                    .env_value(key)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        (runtime == "pi")
                            .then(|| {
                                options
                                    .env_value("PI_CODING_AGENT_DIR")
                                    .filter(|s| !s.is_empty())
                                    .map(PathBuf::from)
                            })
                            .flatten()
                    })
                    .or_else(|| {
                        options
                            .env_value("HOME")
                            .map(PathBuf::from)
                            .or_else(std::env::home_dir)
                            .map(|h| h.join(default))
                    })
            })
            .or_else(|| {
                crate::credentials::for_runtime(runtime)?
                    .usage_directory_env
                    .as_ref()
                    .and_then(|key| options.env_value(key))
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
            })
            .map(|path| {
                let absolute = options.cwd.join(path);
                absolute.canonicalize().unwrap_or(absolute)
            });
        Self {
            runtime: runtime.to_owned(),
            home,
        }
    }
}
#[derive(Clone, Debug)]
pub struct AccountAdmission {
    key: Key,
    epoch: u64,
}
#[derive(Default)]
struct Row {
    epoch: u64,
    changing: bool,
    live: Vec<Weak<Session>>,
}
#[derive(Default, Clone)]
pub(crate) struct Bindings(Arc<Mutex<BTreeMap<Key, Row>>>);
impl Bindings {
    pub(crate) fn admit(&self, key: Key) -> Result<AccountAdmission> {
        let mut db = self.0.lock().unwrap();
        let r = db.entry(key.clone()).or_default();
        if r.changing {
            return Err(Error::Busy);
        }
        Ok(AccountAdmission {
            key,
            epoch: r.epoch,
        })
    }
    pub(crate) fn bind(&self, ticket: &AccountAdmission, session: &Arc<Session>) -> Result<()> {
        let mut db = self.0.lock().unwrap();
        let r = db.entry(ticket.key.clone()).or_default();
        if r.changing || r.epoch != ticket.epoch {
            return Err(Error::Refused(
                "provider credentials changed while opening this chat; retry".into(),
            ));
        }
        r.live
            .retain(|s| s.upgrade().is_some_and(|s| !s.is_closed()));
        r.live.push(Arc::downgrade(session));
        Ok(())
    }
    pub(crate) async fn change(&self, key: Key) -> Result<Changing> {
        let (guard, live) = {
            let mut db = self.0.lock().unwrap();
            let r = db.entry(key.clone()).or_default();
            if r.changing {
                return Err(Error::Busy);
            }
            r.epoch = r
                .epoch
                .checked_add(1)
                .ok_or_else(|| Error::Protocol("account epoch exhausted".into()))?;
            r.changing = true;
            (
                Changing {
                    key,
                    bindings: self.clone(),
                },
                r.live
                    .iter()
                    .filter_map(Weak::upgrade)
                    .filter(|s| !s.is_closed())
                    .collect::<Vec<_>>(),
            )
        };
        let stop = async {
            for s in live {
                if !s.is_closed() {
                    s.close().await?;
                }
            }
            Ok::<_, Error>(())
        };
        tokio::time::timeout(Duration::from_secs(30), stop)
            .await
            .map_err(|_| {
                Error::Timeout("could not release all chats using this credential profile".into())
            })??;
        Ok(guard)
    }
}
pub(crate) struct Changing {
    key: Key,
    bindings: Bindings,
}
impl Drop for Changing {
    fn drop(&mut self) {
        if let Some(r) = self.bindings.0.lock().unwrap().get_mut(&self.key) {
            r.changing = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn changing_one_profile_blocks_its_chats_and_invalidates_old_admissions() {
        let bindings = Bindings::default();
        let profile = Key {
            runtime: "codex".into(),
            home: Some("/first".into()),
        };
        let independent = Key {
            runtime: "codex".into(),
            home: Some("/second".into()),
        };
        let original = bindings.admit(profile.clone()).unwrap();
        let changing = bindings.change(profile.clone()).await.unwrap();
        assert!(matches!(bindings.admit(profile.clone()), Err(Error::Busy)));
        assert!(matches!(
            bindings.change(profile.clone()).await,
            Err(Error::Busy)
        ));
        let other = bindings.admit(independent.clone()).unwrap();
        drop(changing);
        assert_ne!(original.epoch, bindings.admit(profile).unwrap().epoch);
        assert_eq!(other.epoch, bindings.admit(independent).unwrap().epoch);
    }
}
