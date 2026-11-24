use super::{Delivery, InputOptions, Session, TurnReport};
use crate::{
    ControlReceipt, Direction, Error, Event, InputImage, InputOrigin, Result, input::Input,
};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, Notify, oneshot, watch};
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeliverWhen {
    #[default]
    Now,
    AfterTurn,
    WhenIdle,
    Interrupt {
        max_wait: Duration,
    },
}

#[derive(Clone, Debug)]
pub struct MergeInput {
    pub input_id: Uuid,
    pub text: String,
    pub images: Vec<InputImage>,
    pub origin: Option<InputOrigin>,
}

#[derive(Default)]
pub struct MergedInput {
    pub text: String,
    pub images: Vec<InputImage>,
}

type Merger = Arc<dyn Fn(&[MergeInput]) -> Result<MergedInput> + Send + Sync>;

#[derive(Clone, Default)]
pub struct DeliverOptions {
    pub when: DeliverWhen,
    pub input: InputOptions,
    /// The first interrupt in a batch supplies its merger. The default joins text with a blank line.
    pub merge: Option<Merger>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverLanded {
    Prompted,
    Queued,
    Steered,
    Rejected,
    Unconfirmed,
}

#[derive(Clone, Debug)]
pub struct DeliverReceipt {
    pub input_id: Uuid,
    pub landed: DeliverLanded,
    pub control: Option<ControlReceipt>,
    pub merged_input_ids: Vec<Uuid>,
    pub undelivered_input_ids: Vec<Uuid>,
    pub reason: Option<String>,
}
impl DeliverReceipt {
    fn new(input_id: Uuid, landed: DeliverLanded, control: Option<ControlReceipt>) -> Self {
        Self {
            input_id,
            landed,
            control,
            merged_input_ids: Vec::new(),
            undelivered_input_ids: Vec::new(),
            reason: None,
        }
    }
    fn failure(id: Uuid, reason: String, ids: Vec<Uuid>, uncertain: bool) -> Self {
        let mut result = Self::new(
            id,
            if uncertain {
                DeliverLanded::Unconfirmed
            } else {
                DeliverLanded::Rejected
            },
            None,
        );
        result.undelivered_input_ids = ids;
        result.reason = Some(reason);
        result
    }
}

pub(super) struct Batch {
    root: Option<(Uuid, Uuid)>,
    since: u64,
    merge: Option<Merger>,
    deadline: Mutex<Instant>,
    update: Notify,
    admitted: AtomicBool,
    closing: AtomicBool,
    done: watch::Sender<Option<DeliverReceipt>>,
}

#[derive(Default)]
pub(crate) struct Coordinator {
    batch: AsyncMutex<Option<Arc<Batch>>>,
    pub(super) queue_gate: AsyncMutex<()>,
    held: Mutex<Vec<(Uuid, Input)>>,
    paused: AtomicBool,
    pub(crate) resume: Notify,
}
impl Coordinator {
    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    // Keep the original image bytes only for the lifetime of the queue attempt.
    pub(super) fn track(
        self: &Arc<Self>,
        id: Uuid,
        input: &Input,
        receiver: oneshot::Receiver<Result<TurnReport>>,
    ) -> oneshot::Receiver<Result<TurnReport>> {
        self.held.lock().unwrap().push((id, input.clone()));
        let weak = Arc::downgrade(self);
        let request = input.request_id;
        let (done, out) = oneshot::channel();
        tokio::spawn(async move {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(Error::Closed("queue owner stopped".into())));
            if let Some(coordinator) = weak.upgrade() {
                coordinator
                    .held
                    .lock()
                    .unwrap()
                    .retain(|(_, input)| input.request_id != request);
            }
            let _ = done.send(result);
        });
        out
    }
}

async fn completed(batch: &Batch) -> Result<DeliverReceipt> {
    let mut watch = batch.done.subscribe();
    let result = watch
        .wait_for(|r| r.is_some())
        .await
        .map_err(|_| Error::Closed("delivery worker stopped".into()))?;
    Ok(result.clone().unwrap())
}

impl Session {
    pub(super) async fn delivery_gate(&self) -> MutexGuard<'_, Option<Arc<Batch>>> {
        loop {
            let gate = self.journal.delivery.batch.lock().await;
            let Some(batch) = gate.as_ref().cloned() else {
                return gate;
            };
            drop(gate);
            let _ = completed(&batch).await;
        }
    }

    fn running_since(&self) -> Option<u64> {
        if let crate::observe::AgentStatus::Running { since_seq, .. } = self.status() {
            Some(since_seq)
        } else {
            None
        }
    }

    pub(super) async fn deliver_single(
        &self,
        id: Uuid,
        input: Input,
        when: DeliverWhen,
    ) -> Result<(Delivery, Option<ControlReceipt>)> {
        for _ in 0..4 {
            if self.running_since().is_none() {
                let mut attempt = input.clone();
                attempt.request_id = Uuid::new_v4();
                match self.prompt_prepared(id, attempt).await {
                    Ok(turn) => {
                        let receipt = turn.admission().cloned();
                        return Ok((Delivery::Started(turn), receipt));
                    }
                    Err(Error::Busy) => {}
                    Err(error) => return Err(error),
                }
            }
            let Some(target) = self.running_since() else {
                continue;
            };
            if when == DeliverWhen::WhenIdle {
                return Err(Error::Busy);
            }
            if when != DeliverWhen::AfterTurn && self.capabilities().steering {
                let mut attempt = input.clone();
                attempt.request_id = Uuid::new_v4();
                let request = attempt.request_id;
                match self.steer_input(id, attempt, None).await {
                    Ok(()) => {
                        return Ok((
                            Delivery::Steered,
                            self.control_receipt(&request.to_string()),
                        ));
                    }
                    Err(Error::Unsupported(_) | Error::Refused(_))
                        if self.running_since() != Some(target) =>
                    {
                        continue;
                    }
                    Err(Error::Unsupported(_)) => {}
                    Err(Error::Refused(reason))
                        if matches!(
                            reason.as_str(),
                            "no active turn"
                                | "no active DSH turn"
                                | "runtime has not acknowledged this turn"
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            let turn = self.queue_prepared(id, input).await?;
            let receipt = turn.admission().cloned();
            return Ok((Delivery::Queued { input_id: id, turn }, receipt));
        }
        Err(Error::Busy)
    }

    pub async fn deliver_with_options(
        &self,
        text: impl Into<String>,
        options: DeliverOptions,
    ) -> Result<DeliverReceipt> {
        let (id, input) = self.prepare_input(text.into(), options.input);
        let c = &self.journal.delivery;
        let mut races = 0;
        loop {
            let mut gate = c.batch.lock().await;
            let batch = gate.clone();
            if let Some(active) = &batch
                && (active.closing.load(Ordering::Acquire)
                    || !matches!(options.when, DeliverWhen::Interrupt { .. }))
            {
                drop(gate);
                completed(active).await?;
                continue;
            }
            let (since, root) = match self.status() {
                crate::observe::AgentStatus::Running {
                    since_seq,
                    input_id,
                    request_id,
                    ..
                } => {
                    let owner = input_id.zip(request_id).and_then(|(id, request)| {
                        Some((Uuid::parse_str(&id).ok()?, Uuid::parse_str(&request).ok()?))
                    });
                    (Some(since_seq), owner)
                }
                _ => (None, None),
            };
            if let DeliverWhen::Interrupt { max_wait } = options.when
                && (batch.is_some() || (since.is_some() && !self.capabilities().steering))
            {
                let deadline = Instant::now()
                    .checked_add(max_wait)
                    .ok_or_else(|| Error::Refused("interrupt deadline out of range".into()))?;
                let active = match batch {
                    Some(existing) => existing,
                    None => {
                        let b = Arc::new(Batch {
                            root,
                            since: since.unwrap(),
                            merge: options.merge.clone(),
                            deadline: Mutex::new(deadline),
                            update: Notify::new(),
                            admitted: AtomicBool::new(false),
                            closing: AtomicBool::new(false),
                            done: watch::channel(None).0,
                        });
                        c.paused.store(true, Ordering::Release);
                        *gate = Some(b.clone());
                        let (session, worker) = (self.clone(), b.clone());
                        tokio::spawn(async move {
                            session.run_batch(worker).await;
                        });
                        b
                    }
                };
                {
                    let mut d = active.deadline.lock().unwrap();
                    *d = (*d).min(deadline);
                }
                active.update.notify_one();
                let request = input.request_id;
                let turn = match self.queue_prepared(id, input).await {
                    Ok(turn) => turn,
                    Err(error) => {
                        let mut r = DeliverReceipt::failure(id, error.to_string(), vec![id], false);
                        r.control = self.control_receipt(&request.to_string());
                        if r.control.is_none()
                            && matches!(error, Error::Timeout(_) | Error::Closed(_))
                        {
                            r.undelivered_input_ids.clear();
                            r.landed = DeliverLanded::Unconfirmed;
                        }
                        return Ok(r);
                    }
                };
                active.admitted.store(true, Ordering::Release);
                let queued = turn.admission().cloned();
                drop(gate);
                let mut result = completed(&active).await?;
                let still_owned_by_queue = match result.landed {
                    DeliverLanded::Prompted => !result.merged_input_ids.contains(&id),
                    DeliverLanded::Rejected => !result.undelivered_input_ids.contains(&id),
                    _ => false,
                };
                if still_owned_by_queue {
                    let mut own = DeliverReceipt::new(id, DeliverLanded::Queued, queued);
                    own.undelivered_input_ids = result.undelivered_input_ids;
                    return Ok(own);
                }
                result.input_id = id;
                return Ok(result);
            }
            // An idle waiter must release the coordinator so that another caller can interrupt.
            if options.when == DeliverWhen::WhenIdle && since.is_some() {
                drop(gate);
                self.await_idle().await?;
                continue;
            }
            let first_seq = self.journal.latest_record().map_or(0, |r| r.seq + 1);
            let (delivery, control) =
                match self.deliver_single(id, input.clone(), options.when).await {
                    Ok(result) => result,
                    Err(Error::Busy) if options.when == DeliverWhen::WhenIdle && races < 3 => {
                        races += 1;
                        drop(gate);
                        continue;
                    }
                    Err(error) => {
                        let key = id.to_string();
                        let control = self
                            .records()
                            .iter()
                            .rev()
                            .take_while(|r| r.seq >= first_seq)
                            .find(|r| {
                                r.prompt.as_ref().is_some_and(|p| p.input_id == key)
                                    || r.native["inputId"].as_str() == Some(&key)
                            })
                            .and_then(|r| {
                                r.control_request_id()
                                    .and_then(|req| self.control_receipt(req))
                            });
                        let uncertain = control.is_none()
                            && matches!(error, Error::Timeout(_) | Error::Closed(_));
                        let mut r = DeliverReceipt::failure(
                            id,
                            error.to_string(),
                            if uncertain { vec![] } else { vec![id] },
                            uncertain,
                        );
                        r.control = control;
                        return Ok(r);
                    }
                };
            let landed = match delivery {
                Delivery::Started(_) => DeliverLanded::Prompted,
                Delivery::Steered => DeliverLanded::Steered,
                Delivery::Queued { .. } => DeliverLanded::Queued,
            };
            return Ok(DeliverReceipt::new(id, landed, control));
        }
    }

    async fn run_batch(&self, batch: Arc<Batch>) {
        let r = self.collect_batch(&batch).await;
        let mut gate = self.journal.delivery.batch.lock().await;
        *gate = None;
        self.journal.delivery.paused.store(false, Ordering::Release);
        self.journal.delivery.resume.notify_one();
        batch.done.send_replace(Some(r));
    }

    /// An adapter queue loses its senders on exit. The accepted/dequeued journal pair
    /// still proves which entries never left that queue. Native queues retain ownership.
    fn undelivered_on_close(&self, since: u64, withdrawn: &[Uuid]) -> Vec<Uuid> {
        let mut pending = HashSet::new();
        let mut admitted = std::collections::HashMap::new();
        let mut terminal = false;
        for r in self.records() {
            if !r.agent_path.is_empty() || r.session_id.as_deref().is_some_and(|s| s != self.id()) {
                continue;
            }
            for event in &r.events {
                match event {
                    Event::InputQueued { input_id } => {
                        if let Ok(id) = Uuid::parse_str(input_id) {
                            admitted.insert(id, r.seq);
                            if r.native["queued"] == "adapter" {
                                pending.insert(id);
                            } else {
                                pending.remove(&id);
                            }
                        }
                    }
                    Event::InputRemoved { input_id } => {
                        if let Ok(id) = Uuid::parse_str(input_id) {
                            pending.remove(&id);
                        }
                    }
                    Event::InputDropped { input_id } if r.seq < since => {
                        if let Ok(id) = Uuid::parse_str(input_id) {
                            pending.remove(&id);
                        }
                    }
                    Event::Released | Event::Exited { .. } if r.seq >= since => {
                        terminal = true;
                    }
                    _ => {}
                }
            }
            if r.events.contains(&Event::PromptDispatched) {
                for id in &r.carries {
                    if let Ok(id) = Uuid::parse_str(id) {
                        pending.remove(&id);
                    }
                }
            }
            if terminal {
                break;
            }
        }
        if !terminal {
            return withdrawn.to_vec();
        }
        pending.extend(withdrawn);
        let mut ids: Vec<_> = pending.into_iter().collect();
        ids.sort_by_key(|id| admitted.get(id).copied().unwrap_or(u64::MAX));
        ids
    }

    async fn collect_batch(&self, batch: &Batch) -> DeliverReceipt {
        let c = &self.journal.delivery;
        // Replay the running root from its start and then follow the same stream cursor.
        // Only the native end of this root authorizes a replacement prompt.
        let mut cursor = None;
        let mut tools = HashSet::new();
        let mut ended = false;
        let mut closed = false;
        let observe = |record: &crate::Record,
                       tools: &mut HashSet<String>,
                       ended: &mut bool,
                       closed: &mut bool| {
            if record.seq < batch.since
                || !record.agent_path.is_empty()
                || record.session_id.as_deref().is_some_and(|s| s != self.id())
            {
                return;
            }
            if record.kind == crate::RecordKind::Request
                && record.direction == Direction::ToRuntime
                && record.native["type"] == "dispose"
            {
                *closed = true;
            }
            for e in &record.events {
                match e {
                    Event::Released | Event::Exited { .. } => *closed = true,
                    Event::ToolStarted { id, .. } if !*ended => {
                        tools.insert(id.clone());
                    }
                    Event::ToolEnded { id } => {
                        tools.remove(id);
                    }
                    Event::TurnEnded { .. } if record.direction == Direction::FromRuntime => {
                        *ended = true
                    }
                    Event::TurnEnded { .. } if !*ended => *closed = true,
                    _ => {}
                }
            }
        };
        for r in self.records() {
            observe(&r, &mut tools, &mut ended, &mut closed);
            cursor = Some(r.cursor());
        }
        let mut live = match self.subscribe(cursor.as_ref()) {
            Ok(sub) => sub,
            Err(e) => return DeliverReceipt::failure(Uuid::nil(), e.to_string(), vec![], false),
        };
        loop {
            // Wait for all currently joined queue admissions before closing the batch.
            let gate = c.batch.lock().await;
            if !batch.admitted.load(Ordering::Acquire) {
                batch.closing.store(true, Ordering::Release);
                return DeliverReceipt::failure(
                    Uuid::nil(),
                    "no interrupt input was admitted".into(),
                    Vec::new(),
                    false,
                );
            }
            let deadline = *batch.deadline.lock().unwrap();
            if tools.is_empty() || ended || closed || Instant::now() >= deadline {
                batch.closing.store(true, Ordering::Release);
                drop(gate);
                break;
            }
            drop(gate);
            tokio::select! {
                _=batch.update.notified()=>{},
                _=tokio::time::sleep_until(deadline)=>{},
                r=live.next()=>match r{Some(r)=>observe(&r,&mut tools,&mut ended,&mut closed),None=>closed=true},
            }
        }
        let gate = c.queue_gate.lock().await;
        let held = c.held.lock().unwrap().clone();
        let mut inputs = Vec::new();
        let mut ids = Vec::new();
        let mut failure = None;
        let mut seen = HashSet::new();
        if held.iter().any(|(id, _)| !seen.insert(*id)) {
            return DeliverReceipt::failure(
                Uuid::nil(),
                "multiple held attempts share an input ID".into(),
                Vec::new(),
                false,
            );
        }
        for (id, input) in held {
            match self.call(|tx| super::Command::Withdraw(id, tx)).await {
                Ok(()) => {
                    ids.push(id);
                    inputs.push(MergeInput {
                        input_id: id,
                        text: input.text,
                        images: input.images,
                        origin: input.origin,
                    });
                }
                Err(Error::Refused(reason)) if reason.contains("not queued") => {}
                Err(e) => {
                    failure = Some(e.to_string());
                }
            }
        }
        drop(gate);
        let merged_id = Uuid::new_v4();
        if failure.is_some() || ids.is_empty() || closed || self.is_closed() {
            let ids = self.undelivered_on_close(batch.since, &ids);
            return DeliverReceipt::failure(
                merged_id,
                failure.unwrap_or_else(|| {
                    if ids.is_empty() {
                        "no held inputs remain".into()
                    } else {
                        "session closed before redelivery".into()
                    }
                }),
                ids,
                false,
            );
        }
        if !ended && let Some((id, request)) = batch.root {
            let target = crate::transport::TurnTarget {
                input_id: id,
                request_id: Some(request),
            };
            if target.pin(&self.journal).is_some()
                && let Err(error) = self.abort_turn(id, Some(request)).await
                && target.pin(&self.journal).is_some()
            {
                return DeliverReceipt::failure(merged_id, error.to_string(), ids, false);
            }
        }
        if !ended && batch.root.is_none() {
            return DeliverReceipt::failure(
                merged_id,
                "active root has no abortable identity".into(),
                ids,
                false,
            );
        }
        while !ended && !closed {
            match live.next().await {
                Some(r) => observe(&r, &mut tools, &mut ended, &mut closed),
                None => closed = true,
            }
        }
        if closed || self.is_closed() {
            return DeliverReceipt::failure(
                merged_id,
                "session closed before redelivery".into(),
                ids,
                false,
            );
        }
        let merged = if let Some(merge) = &batch.merge {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| merge(&inputs))) {
                Ok(Ok(m)) => m,
                Ok(Err(e)) => return DeliverReceipt::failure(merged_id, e.to_string(), ids, false),
                Err(_) => {
                    return DeliverReceipt::failure(
                        merged_id,
                        "input merger panicked".into(),
                        ids,
                        false,
                    );
                }
            }
        } else {
            MergedInput {
                text: inputs
                    .iter()
                    .map(|i| i.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                images: inputs.into_iter().flat_map(|i| i.images).collect(),
            }
        };
        let mut input = Input::new(merged.text, merged.images);
        input.carried = ids.clone();
        match self.submit_prepared(merged_id, input).await {
            Ok(super::PromptDecision::Accepted(turn)) => {
                let mut r = DeliverReceipt::new(
                    merged_id,
                    DeliverLanded::Prompted,
                    turn.admission().cloned(),
                );
                r.merged_input_ids = ids;
                r
            }
            Ok(super::PromptDecision::Rejected { receipt, error }) => {
                let mut r = DeliverReceipt::failure(merged_id, error.to_string(), ids, false);
                r.control = Some(receipt);
                r
            }
            Err(e) => {
                let attempted = matches!(
                    e,
                    Error::Io(_) | Error::Closed(_) | Error::Timeout(_) | Error::Http(_)
                ) || self.records().iter().any(|r| {
                    r.prompt
                        .as_ref()
                        .is_some_and(|p| p.input_id == merged_id.to_string())
                        && r.events.contains(&Event::PromptDispatched)
                });
                DeliverReceipt::failure(
                    merged_id,
                    e.to_string(),
                    if attempted { vec![] } else { ids },
                    attempted,
                )
            }
        }
    }
}
