//! Producers can add inputs while another process owns the native session. The queue
//! stores admission order. The chat journal is authoritative about dispatch attempts.
use super::{ChatInfo, ChatSession, ChatStore, ChatTurn, Sender, Sink, TurnStatus};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::ErrorKind,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    Pending,
    Sending,
    Sent,
    Uncertain,
    Withdrawn,
    /// An older pending entry exceeds the current native input limit.
    TooLarge,
    /// The provider recognized an input UUID it had previously received.
    Duplicate,
    /// The provider cancelled this input before starting it.
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedInput {
    /// Also the id of the resulting chat turn.
    pub id: Uuid,
    pub text: String,
    pub status: InputStatus,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    id: Uuid,
    text: String,
    withdrawn: bool,
}
impl Entry {
    fn status(&self, info: &ChatInfo, turn: Option<&ChatTurn>) -> InputStatus {
        match turn {
            Some(t) if t.native_input_receipt == Some(crate::InputReceipt::Duplicate) => {
                InputStatus::Duplicate
            }
            Some(t) if t.native_input_receipt == Some(crate::InputReceipt::Cancelled) => {
                InputStatus::Cancelled
            }
            Some(t)
                if t.input_sent
                    || t.native_turn_id.is_some()
                    || t.status == TurnStatus::Completed =>
            {
                InputStatus::Sent
            }
            Some(t) if t.status == TurnStatus::Running => InputStatus::Sending,
            Some(_) => InputStatus::Uncertain,
            None if self.withdrawn => InputStatus::Withdrawn,
            None if crate::input::check_text(
                &self.text,
                crate::input::text_limit(&info.runtime),
            )
            .is_err() =>
            {
                InputStatus::TooLarge
            }
            None => InputStatus::Pending,
        }
    }
    fn receipt(&self, info: &ChatInfo, turn: Option<&ChatTurn>) -> QueuedInput {
        QueuedInput {
            id: self.id,
            text: self.text.clone(),
            status: self.status(info, turn),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Inbox {
    chat: Uuid,
    native_id: String,
    entries: Vec<Entry>,
}
impl ChatStore {
    fn inbox_path(&self, id: Uuid) -> std::path::PathBuf {
        self.dir.join(format!("{id}.inputs"))
    }
    fn read_inbox(&self, chat: &ChatInfo) -> Result<Inbox> {
        let file = match File::open(self.inbox_path(chat.id)) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Inbox {
                    chat: chat.id,
                    native_id: chat.native_id.clone(),
                    entries: vec![],
                });
            }
            Err(e) => return Err(e.into()),
        };
        let inbox: Inbox = serde_json::from_reader(std::io::BufReader::new(file))?;
        if inbox.chat != chat.id || inbox.native_id != chat.native_id {
            return Err(Error::Protocol(
                "input queue belongs to a different session".into(),
            ));
        }
        Ok(inbox)
    }
    // Keep the lock on a separate inode: publishing a new snapshot replaces the data file.
    fn inbox_lock(&self, id: Uuid) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(self.dir.join(format!("{id}.inputs.lock")))?;
        file.lock()?;
        Ok(file)
    }
    fn save_inbox(&self, inbox: &Inbox) -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(tmp.as_file_mut(), inbox)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.inbox_path(inbox.chat))
            .map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
    /// Persist before returning. Multiple producers are ordered by admission under the file lock.
    pub fn enqueue(&self, chat: Uuid, id: Uuid, text: impl Into<String>) -> Result<QueuedInput> {
        self.change_input(chat, id, Some(text.into()))
    }
    /// Withdraw a pending input. Repeated withdrawal returns its original receipt.
    pub fn withdraw(&self, chat: Uuid, id: Uuid) -> Result<QueuedInput> {
        self.change_input(chat, id, None)
    }
    fn change_input(&self, chat: Uuid, id: Uuid, text: Option<String>) -> Result<QueuedInput> {
        let file = File::open(self.path(chat))?;
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(file, chat)?;
        let turn = state.find(id);
        let mut inbox = self.read_inbox(&state.info)?;
        let receipt = if let Some(text) = text {
            if turn.is_none() {
                state.check_new_prompt_id(id)?;
            }
            if turn.is_some_and(|t| t.prompt() != Some(text.as_str())) {
                return Err(different_prompt());
            }
            if let Some(entry) = inbox.entries.iter().find(|e| e.id == id) {
                if entry.text != text {
                    return Err(different_prompt());
                }
                return Ok(entry.receipt(&state.info, turn));
            }
            if turn.is_none() {
                crate::input::check_text(&text, crate::input::text_limit(&state.info.runtime))?;
            }
            let entry = Entry {
                id,
                text,
                withdrawn: false,
            };
            let receipt = entry.receipt(&state.info, turn);
            inbox.entries.push(entry);
            receipt
        } else {
            let entry = inbox
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| Error::Refused("unknown queued input".into()))?;
            if turn.is_some() {
                return Err(Error::Refused("input dispatch has already started".into()));
            }
            if entry.withdrawn {
                return Ok(entry.receipt(&state.info, None));
            }
            entry.withdrawn = true;
            entry.receipt(&state.info, None)
        };
        self.save_inbox(&inbox)?;
        Ok(receipt)
    }
    /// Read all queued inputs, including earlier delivery and withdrawal receipts.
    pub fn queued_inputs(&self, chat: Uuid) -> Result<Vec<QueuedInput>> {
        let file = File::open(self.path(chat))?;
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(file, chat)?;
        Ok(self
            .read_inbox(&state.info)?
            .entries
            .iter()
            .map(|e| e.receipt(&state.info, state.find(e.id)))
            .collect())
    }
}
fn different_prompt() -> Error {
    Error::Refused("request id already belongs to a different prompt".into())
}

/// Executed by the actor after its busy check and before its first native prompt write.
/// A durable Begin consumes the input even if the process dies before a receipt arrives.
pub(crate) struct Admission {
    pub(super) store: ChatStore,
    pub(super) sink: Sink,
    pub(super) id: Uuid,
    pub(super) text: String,
    pub(super) checkpoint: super::log::TurnCapture,
}
impl Admission {
    pub(crate) async fn commit(admission: Option<Self>) -> Result<()> {
        let Some(admission) = admission else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || admission.begin())
            .await
            .map_err(|e| Error::Closed(e.to_string()))?
    }
    fn begin(self) -> Result<()> {
        let chat = self.sink.0.lock().unwrap().state.info.id;
        let _lock = self.store.inbox_lock(chat)?;
        let mut log = self.sink.0.lock().unwrap();
        if log.existing(self.id, &self.text)?.is_some() {
            return Err(Error::Refused("input dispatch has already started".into()));
        }
        let inbox = self.store.read_inbox(&log.state.info)?;
        if let Some(entry) = inbox.entries.iter().find(|e| e.id == self.id) {
            if entry.text != self.text {
                return Err(different_prompt());
            }
            if entry.withdrawn {
                return Err(Error::Refused("input was withdrawn".into()));
            }
        }
        if let Some(head) = inbox
            .entries
            .iter()
            .find(|e| e.status(&log.state.info, log.state.find(e.id)) == InputStatus::Pending)
            && head.id != self.id
        {
            return Err(Error::Busy);
        }
        log.begin(self.id, &self.text, self.checkpoint.before.tree.clone())?;
        log.checkpoint = Some(self.checkpoint);
        Ok(())
    }
}

impl ChatSession {
    pub fn enqueue(&self, id: Uuid, text: impl Into<String>) -> Result<QueuedInput> {
        self.sender.store.enqueue(self.info().id, id, text)
    }
    pub fn withdraw(&self, id: Uuid) -> Result<QueuedInput> {
        self.sender.store.withdraw(self.info().id, id)
    }
    /// Wait for all currently pending inputs to finish, including their checkpoints.
    pub async fn drain_queue(&self) -> Result<()> {
        let (reply, done) = oneshot::channel();
        self.worker
            .drains
            .send(reply)
            .map_err(|_| Error::Closed("input queue closed".into()))?;
        done.await
            .map_err(|_| Error::Closed("input queue closed".into()))?
    }
}
impl Sender {
    async fn pump_one(&self) -> Result<bool> {
        let inbox = self.store.read_inbox(&self.info)?;
        let next = {
            let log = self.sink.0.lock().unwrap();
            log.healthy()?;
            inbox
                .entries
                .into_iter()
                .find(|e| e.status(&log.state.info, log.state.find(e.id)) == InputStatus::Pending)
        };
        let Some(entry) = next else {
            return Ok(false);
        };
        match self.run_turn(entry.id, entry.text).await {
            Ok(_) => Ok(true),
            Err(error) => {
                // A producer may withdraw the selected head while we capture its checkpoint.
                if self
                    .store
                    .read_inbox(&self.info)?
                    .entries
                    .iter()
                    .any(|e| e.id == entry.id && e.withdrawn)
                {
                    Ok(true)
                } else {
                    Err(error)
                }
            }
        }
    }
}

type Drain = oneshot::Sender<Result<()>>;

pub(super) struct QueueWorker {
    drains: mpsc::UnboundedSender<Drain>,
    stop: watch::Sender<()>,
    task: JoinHandle<()>,
}
impl QueueWorker {
    pub(super) fn start(sender: &Arc<Sender>) -> Self {
        let (stop, stopped) = watch::channel(());
        let (drains, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(Self::drive(Arc::downgrade(sender), stopped, requests));
        Self { drains, stop, task }
    }
    async fn drive(
        sender: Weak<Sender>,
        mut stopped: watch::Receiver<()>,
        mut requests: mpsc::UnboundedReceiver<Drain>,
    ) {
        let mut drained = None;
        let mut waiter: Option<Drain> = None;
        let mut more = true;
        loop {
            if !more {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => return,
                    request = requests.recv(), if waiter.is_none() => match request {
                        Some(reply) => waiter = Some(reply),
                        None => return,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                }
            }
            if !matches!(stopped.has_changed(), Ok(false)) {
                return;
            }
            if waiter.is_none() {
                waiter = requests.try_recv().ok();
            }
            let Some(sender) = sender.upgrade() else {
                return;
            };
            // A process that only enqueues cannot notify this worker in memory.
            // A settled queue needs only a metadata check until another snapshot is published.
            let stamp = std::fs::metadata(sender.store.inbox_path(sender.info.id))
                .and_then(|m| Ok((m.len(), m.modified()?)))
                .ok();
            more = false;
            if waiter.is_none() && stamp == drained {
                continue;
            }
            let work = sender.pump_one();
            tokio::pin!(work);
            let result = tokio::select! {
                biased;
                stop = stopped.changed() => {
                    // close() also closes the provider, allowing an active turn to record its
                    // final checkpoint. On drop the worker also owns native shutdown.
                    if stop.is_err() { let _ = sender.session.close().await; }
                    let _ = work.await;
                    return;
                },
                result = &mut work => result,
            };
            match result {
                Ok(true) => more = true,
                Err(Error::Busy) => {}
                result => {
                    if matches!(result, Ok(false)) {
                        drained = stamp;
                    }
                    if let Some(waiter) = waiter.take() {
                        let _ = waiter.send(result.map(|_| ()));
                    }
                }
            }
        }
    }
    pub(super) fn request_stop(&self) {
        self.stop.send_replace(());
    }
    pub(super) async fn join(self) {
        let _ = self.task.await;
    }
}
