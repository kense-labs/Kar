//! Producers can add inputs while another process owns the native session. The queue
//! stores admission order. The chat journal is authoritative about dispatch attempts.
mod commands;
mod promotion;
use super::{ChatInfo, ChatSession, ChatStore, Sender, Sink, TurnStatus};
use crate::{Error, Result};
pub use commands::{QueueAction, QueueReceipt};
pub(crate) use promotion::Claim;
pub use promotion::{QueuePromotion, SteerTarget};
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
    /// The provider explicitly refused the input at admission.
    Refused,
    /// The provider cancelled this input before starting it.
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedInput {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<super::Attachment>,
    /// Also the id of the resulting chat turn.
    pub id: Uuid,
    pub text: String,
    pub status: InputStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion: Option<QueuePromotion>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<super::Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initial_attachments: Option<Vec<super::Attachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    promotion: Option<QueuePromotion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initial_text: Option<String>,
    id: Uuid,
    text: String,
    withdrawn: bool,
}
impl Entry {
    fn status(&self, state: &super::log::State) -> InputStatus {
        let info = &state.info;
        let turn = state.find(self.id);
        if turn.is_none()
            && let Some(status) = self
                .promotion
                .as_ref()
                .and_then(|p| p.status(self.id, state))
        {
            return status;
        }
        match turn {
            Some(t) if t.native_input_receipt == Some(crate::InputReceipt::Refused) => {
                InputStatus::Refused
            }
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
            None if self.refusal.is_some() => InputStatus::Refused,
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
    fn receipt(&self, state: &super::log::State) -> QueuedInput {
        QueuedInput {
            attachments: self.attachments.clone(),
            id: self.id,
            text: self.text.clone(),
            status: self.status(state),
            refusal: self.refusal.clone(),
            promotion: self.promotion.clone(),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Inbox {
    chat: Uuid,
    native_id: String,
    entries: Vec<Entry>,
    #[serde(default)]
    held: bool,
    #[serde(default)]
    promotions: std::collections::BTreeMap<Uuid, (Uuid, SteerTarget)>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    returned_promotions: std::collections::BTreeMap<Uuid, QueuedInput>,
    #[serde(default)]
    released: Option<Uuid>,
    #[serde(default)]
    commands: std::collections::BTreeMap<Uuid, commands::SavedCommand>,
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
                    native_id: self.initial_info(chat.id)?.native_id,
                    entries: vec![],
                    held: false,
                    promotions: Default::default(),
                    returned_promotions: Default::default(),
                    released: None,
                    commands: Default::default(),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let inbox: Inbox = serde_json::from_reader(std::io::BufReader::new(file))?;
        if inbox.chat != chat.id
            || (inbox.native_id != chat.native_id
                && inbox.native_id != self.initial_info(chat.id)?.native_id)
        {
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
    pub(super) fn prepare_handoff(
        &self,
        log: &mut super::log::Log,
        handoff: super::Handoff,
    ) -> Result<()> {
        let _lock = self.inbox_lock(log.state.info.id)?;
        let inbox = self.read_inbox(&log.state.info)?;
        if inbox.entries.iter().any(|e| {
            matches!(
                e.status(&log.state),
                InputStatus::Pending | InputStatus::Sending
            )
        }) {
            return Err(Error::Busy);
        }
        log.commit(super::ChatUpdate::HandoffPrepared {
            handoff: Box::new(handoff),
        })
    }
    /// Persist before returning. Multiple producers are ordered by admission under the file lock.
    pub fn enqueue(&self, chat: Uuid, id: Uuid, text: impl Into<String>) -> Result<QueuedInput> {
        self.enqueue_attached(chat, id, text, &[])
    }
    pub fn enqueue_attached(
        &self,
        chat: Uuid,
        id: Uuid,
        text: impl Into<String>,
        attachment_ids: &[Uuid],
    ) -> Result<QueuedInput> {
        let attachments = self.attachments(chat, attachment_ids)?;
        self.change_input(chat, id, Some((text.into(), attachments)))
    }
    /// Withdraw a pending input. Repeated withdrawal returns its original receipt.
    pub fn withdraw(&self, chat: Uuid, id: Uuid) -> Result<QueuedInput> {
        self.change_input(chat, id, None)
    }
    fn change_input(
        &self,
        chat: Uuid,
        id: Uuid,
        text: Option<(String, Vec<super::Attachment>)>,
    ) -> Result<QueuedInput> {
        let file = File::open(self.path(chat))?;
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(file, chat)?;
        let turn = state.find(id);
        let mut inbox = self.read_inbox(&state.info)?;
        let receipt = if let Some((text, attachments)) = text {
            if let Some(entry) = inbox.entries.iter().find(|e| e.id == id) {
                let same = entry.text == text && entry.attachments == attachments;
                let original = entry.initial_text.as_ref().unwrap_or(&entry.text) == &text
                    && entry
                        .initial_attachments
                        .as_ref()
                        .unwrap_or(&entry.attachments)
                        == &attachments;
                if !same && !original {
                    return Err(different_prompt());
                }
                return Ok(entry.receipt(&state));
            }
            if turn.is_none() {
                state.check_new_prompt_id(id)?;
            }
            if turn
                .is_some_and(|t| t.prompt() != Some(text.as_str()) || t.attachments != attachments)
            {
                return Err(different_prompt());
            }
            if turn.is_none() {
                if state.pending_handoff().is_some() {
                    return Err(Error::Busy);
                }
                crate::input::check_text(&text, crate::input::text_limit(&state.info.runtime))?;
            }
            let entry = Entry {
                refusal: None,
                attachments,
                initial_attachments: None,
                initial_text: None,
                promotion: None,
                id,
                text,
                withdrawn: false,
            };
            let receipt = entry.receipt(&state);
            inbox.entries.push(entry);
            receipt
        } else {
            let entry = inbox
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| Error::Refused("unknown queued input".into()))?;
            if entry.refusal.is_some() {
                return Err(Error::Refused("input was already refused".into()));
            }
            if turn.is_some()
                || entry
                    .promotion
                    .as_ref()
                    .is_some_and(|p| !p.returned_to_queue)
            {
                return Err(Error::Refused("input dispatch has already started".into()));
            }
            if entry.withdrawn {
                return Ok(entry.receipt(&state));
            }
            entry.withdrawn = true;
            entry.receipt(&state)
        };
        self.save_inbox(&inbox)?;
        Ok(receipt)
    }
    /// Evaluate an idle-chat change against the same admission fence used by the sender.
    pub(crate) fn with_admission_state<T>(
        &self,
        id: Uuid,
        f: impl FnOnce(super::ChatHistory, bool) -> Result<T>,
    ) -> Result<T> {
        let _lock = self.inbox_lock(id)?;
        let state = self.read_state(File::open(self.path(id))?, id)?;
        let queued = self.read_inbox(&state.info)?.entries.iter().any(|e| {
            matches!(
                e.status(&state),
                InputStatus::Pending | InputStatus::Sending | InputStatus::Uncertain
            )
        });
        f(self.history_from_state(id, state)?, queued)
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
            .map(|e| e.receipt(&state))
            .collect())
    }
}
fn different_prompt() -> Error {
    Error::Refused("request id already belongs to a different prompt".into())
}

/// Executed by the actor after its busy check and before its first native prompt write.
/// A durable Begin consumes the input even if the process dies before a receipt arrives.
pub(crate) struct Admission {
    pub(super) attachments: Vec<super::Attachment>,
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
        let _dispatch = self.store.stop_fence(chat, false)?;
        let _lock = self.store.inbox_lock(chat)?;
        let mut log = self.sink.0.lock().unwrap();
        if log
            .existing(self.id, &self.text, &self.attachments)?
            .is_some()
        {
            return Err(Error::Refused("input dispatch has already started".into()));
        }
        let inbox = self.store.read_inbox(&log.state.info)?;
        if let Some(entry) = inbox.entries.iter().find(|e| e.id == self.id) {
            if entry.text != self.text || entry.attachments != self.attachments {
                return Err(different_prompt());
            }
            if entry
                .promotion
                .as_ref()
                .is_some_and(|p| !p.returned_to_queue)
            {
                return Err(Error::Refused("input was claimed for steering".into()));
            }
            if entry.withdrawn {
                return Err(Error::Refused("input was withdrawn".into()));
            }
            if let Some(reason) = &entry.refusal {
                return Err(Error::Refused(reason.clone()));
            }
        }
        if inbox.held
            && inbox.entries.iter().any(|e| e.id == self.id)
            && inbox.released != Some(self.id)
        {
            return Err(Error::Busy);
        }
        if let Some(head) = inbox.next(&log.state)
            && head.id != self.id
        {
            return Err(Error::Busy);
        }
        log.begin(
            self.id,
            &self.text,
            &self.checkpoint.before,
            self.attachments,
        )?;
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
    // Check the same head while holding the lock used by editors and native admission.
    // A concurrent edit or hold leaves the new queue untouched for the next iteration.
    fn refuse_queue_head(
        &self,
        id: Uuid,
        text: &str,
        attachments: &[super::Attachment],
        reason: &str,
    ) -> Result<()> {
        let _lock = self.store.inbox_lock(self.info.id)?;
        let log = self.sink.0.lock().unwrap();
        log.healthy()?;
        let mut inbox = self.store.read_inbox(&self.info)?;
        if inbox
            .next(&log.state)
            .is_some_and(|e| e.id == id && e.text == text && e.attachments == attachments)
        {
            let entry = inbox
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .expect("selected queue head");
            entry.refusal = Some(reason.to_owned());
            self.store.save_inbox(&inbox)?;
        }
        Ok(())
    }

    async fn pump_one(&self) -> Result<bool> {
        let inbox = self.store.read_inbox(&self.info)?;
        let next = {
            let log = self.sink.0.lock().unwrap();
            log.healthy()?;
            inbox
                .next(&log.state)
                .map(|e| (e.id, e.text.clone(), e.attachments.clone()))
        };
        let Some((id, text, attachments)) = next else {
            return Ok(false);
        };
        if text.is_empty() && attachments.is_empty() {
            self.refuse_queue_head(id, &text, &attachments, crate::input::EMPTY_INPUT)?;
            return Ok(true);
        }
        if attachments
            .iter()
            .any(|a| a.kind == super::AttachmentKind::Image)
            && !self.session.capabilities().image_input
        {
            self.refuse_queue_head(
                id,
                &text,
                &attachments,
                "this provider does not accept images",
            )?;
            return Ok(true);
        }
        match self.run_turn(id, text.clone(), attachments.clone()).await {
            Ok(_) => Ok(true),
            Err(error) => {
                if self
                    .sink
                    .0
                    .lock()
                    .unwrap()
                    .state
                    .find(id)
                    .is_some_and(|turn| {
                        turn.native_input_receipt == Some(crate::InputReceipt::Refused)
                    })
                {
                    return Ok(true);
                }
                // A producer may withdraw the selected head while we capture its checkpoint.
                if self.store.read_inbox(&self.info)?.entries.iter().any(|e| {
                    e.id == id
                        && (e.withdrawn
                            || e.text != text
                            || e.attachments != attachments
                            || e.promotion.as_ref().is_some_and(|p| !p.returned_to_queue))
                }) {
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
