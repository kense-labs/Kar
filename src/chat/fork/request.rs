//! A stable child UUID binds one public fork request across controller restarts.
use super::*;
use std::path::Path;

pub(super) struct Request {
    pub id: Uuid,
    pub lease: File,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Binding {
    source: Uuid,
    turn: Uuid,
    destination: Option<Checkout>,
    #[serde(default)]
    failed: bool,
}

#[derive(Serialize, Deserialize)]
struct Checkout {
    id: Uuid,
    root: PathBuf,
    workspace: PathBuf,
}

impl Binding {
    fn matches(&self, source: Uuid, turn: Uuid, path: Option<&Path>) -> Result<bool> {
        if self.source != source || self.turn != turn {
            return Ok(false);
        }
        match (&self.destination, path) {
            (None, None) => Ok(true),
            (Some(dest), Some(path)) => {
                let path = path.canonicalize()?;
                Ok((path == dest.workspace || path == dest.root)
                    && crate::workbench_git::pin_checkout(&path)?
                        .is_some_and(|(id, _)| id == dest.id))
            }
            _ => Ok(false),
        }
    }

    fn write(&self, path: &Path, create: bool) -> Result<()> {
        let dir = path.parent().expect("fork request belongs to a chat store");
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(tmp.as_file_mut(), self)?;
        tmp.as_file().sync_all()?;
        if create {
            tmp.persist_noclobber(path)
        } else {
            tmp.persist(path)
        }
        .map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    }
}

impl Request {
    pub(super) fn path(store: &ChatStore, child: Uuid) -> PathBuf {
        store.dir.join(format!("{child}.fork-request"))
    }

    fn read(
        store: &ChatStore,
        source: Uuid,
        turn: Uuid,
        child: Uuid,
        checkout: Option<&Path>,
    ) -> Result<Option<Binding>> {
        let file = match File::open(Self::path(store, child)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let binding: Binding = serde_json::from_reader(file)?;
        if !binding.matches(source, turn, checkout)? {
            return Err(Error::Refused(
                "fork request UUID belongs to a different source, turn or worktree".into(),
            ));
        }
        Ok(Some(binding))
    }

    pub fn published(
        store: &ChatStore,
        source: Uuid,
        turn: Uuid,
        child: Uuid,
        checkout: Option<&Path>,
        policy: ForkPolicy,
    ) -> Result<Option<super::super::ChatHistory>> {
        if !store.path(child).try_exists()? {
            return Ok(None);
        }
        if Self::read(store, source, turn, child, checkout)?.is_none() {
            return Err(Error::Refused(
                "child UUID already belongs to another chat".into(),
            ));
        }
        let h = store.history(child)?;
        if !h
            .chat
            .forked_from
            .as_ref()
            .is_some_and(|origin| origin.chat == source && origin.turn == turn)
        {
            return Err(Error::Refused(
                "published chat does not match its fork request".into(),
            ));
        }
        policy.check(h.chat.forked_from.as_ref().unwrap().mode)?;
        Ok(Some(h))
    }

    pub fn while_busy(
        store: &ChatStore,
        source: Uuid,
        turn: Uuid,
        child: Uuid,
        checkout: Option<&Path>,
        policy: ForkPolicy,
    ) -> Result<super::super::ChatHistory> {
        if let Some(history) = Self::published(store, source, turn, child, checkout, policy)? {
            return Ok(history);
        }
        Self::read(store, source, turn, child, checkout)?;
        Err(Error::Busy)
    }

    pub fn claim(
        store: &ChatStore,
        source: Uuid,
        turn: Uuid,
        child: Uuid,
        checkout: Option<&Path>,
        lease: File,
    ) -> Result<Self> {
        if let Some(binding) = Self::read(store, source, turn, child, checkout)? {
            let stage = store.fork_stage(source);
            if !stage.try_exists()? || log::staged_info(File::open(&stage)?)?.id != child {
                let reason = if binding.failed {
                    format!(
                        "native fork request {child} failed before publication; no second native fork was issued"
                    )
                } else {
                    format!(
                        "the native fork outcome for {child} is unconfirmed after interruption; no second native fork was issued"
                    )
                };
                return Err(Error::Refused(reason));
            }
        }
        Ok(Self {
            id: child,
            path: Self::path(store, child),
            lease,
        })
    }

    pub fn begin(
        &self,
        source: Uuid,
        turn: Uuid,
        dest: Option<&worktree::Destination>,
    ) -> Result<()> {
        Binding {
            source,
            turn,
            destination: dest.map(|d| Checkout {
                id: d.id,
                root: d.root().to_owned(),
                workspace: d.workspace.clone(),
            }),
            failed: false,
        }
        .write(&self.path, true)
    }

    pub fn record_error(&self) -> Result<()> {
        let mut binding: Binding = serde_json::from_reader(File::open(&self.path)?)?;
        binding.failed = true;
        binding.write(&self.path, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ordinary_chat_creation_preserves_dispatched_fork_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("chats")).unwrap();
        let child = Uuid::new_v4();
        let lease = store.lease(child, true).unwrap();
        lease.try_lock().unwrap();
        let request = Request {
            id: child,
            path: Request::path(&store, child),
            lease,
        };
        request.begin(Uuid::new_v4(), Uuid::new_v4(), None).unwrap();
        drop(request);
        let result = store
            .create_with_id(
                child,
                "codex",
                Some(&dir.path().join("unavailable")),
                SessionOptions::new(dir.path()),
            )
            .await;
        assert!(
            matches!(result, Err(Error::Refused(reason)) if reason.contains("belongs to a native fork request"))
        );
        assert!(!store.path(child).exists());
    }
}
