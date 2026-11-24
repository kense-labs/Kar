//! WebAssembly controller for a host-owned Pi Durable Harness.
mod native;
mod projection;

use crate::{Cursor, Direction, Event, Outcome, RecordKind, stream::Journal};
use js_sys::{Array, Object, Promise};
use native::{JsResult, call, ctx, error, get, invoke, js, json, set};
use projection::Projection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
};
use tokio::sync::{Mutex, oneshot};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Options {
    cwd: String,
    resume: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    append_system_prompt: Option<String>,
    retained_bytes: Option<usize>,
}
#[wasm_bindgen]
pub struct PiDurable {
    harness: JsValue,
    models: JsValue,
}
#[wasm_bindgen]
impl PiDurable {
    /// Known composer and tool capabilities. Does not call into the host harness.
    #[wasm_bindgen(js_name = sessionCapabilities)]
    pub fn session_capabilities(&self) -> JsResult<JsValue> {
        js(
            &serde_json::to_value(crate::SessionCapabilityDeclaration::pi_durable())
                .map_err(|e| error(e.to_string()))?,
        )
    }
    /// Credential declarations for the host-owned Pi Durable Harness.
    pub fn credentials(&self) -> JsResult<JsValue> {
        let shape = crate::credentials::for_runtime("pi-durable").expect("Pi Durable declaration");
        js(&serde_json::to_value(shape).map_err(|e| error(e.to_string()))?)
    }
    #[wasm_bindgen(constructor)]
    pub fn new(harness: JsValue, models: JsValue) -> PiDurable {
        PiDurable { harness, models }
    }
    pub fn models(&self) -> JsResult<JsValue> {
        call(&native::catalog(&self.models)?, "getModels", &[])
    }
    pub async fn open(&self, options: JsValue) -> JsResult<DurableSession> {
        let options: Options =
            serde_json::from_value(json(&options)?).map_err(|e| error(e.to_string()))?;
        if options.cwd.is_empty() {
            return Err(error("a Pi Durable workspace is required"));
        }
        let journal = Journal::new(options.retained_bytes).map_err(|e| error(e.to_string()))?;
        let conv =
            native::conversation(&self.harness, &native::catalog(&self.models)?, &options).await?;
        let id = get(&conv, "id")?;
        journal.bind_root(
            &id.as_f64()
                .ok_or_else(|| error("invalid native conversation id"))?
                .to_string(),
        );
        let core = Rc::new(Core {
            harness: self.harness.clone(),
            conv,
            id: id.clone(),
            journal,
            pending: RefCell::new(VecDeque::new()),
            ready: Cell::new(false),
            draining: Cell::new(false),
            detached: Cell::new(false),
            watch: RefCell::new(None),
            projection: RefCell::new(Projection::default()),
            waiters: RefCell::new(vec![]),
            admissions: Mutex::new(()),
            queued: RefCell::new(BTreeMap::new()),
            withdrawn: RefCell::new(BTreeSet::new()),
        });
        let callbacks = Object::new();
        let advance = callback(&core, false);
        let end = callback(&core, true);
        set(&callbacks, "advance", advance.as_ref())?;
        set(&callbacks, "end", end.as_ref())?;
        let watch = invoke(
            &core.harness,
            "observeConversation",
            &[id, callbacks.into(), ctx()],
        )
        .await?;
        core.watch.replace(Some(watch.clone()));
        let session = DurableSession {
            core: core.clone(),
            _advance: advance,
            _end: end,
        };
        let view = json(&get(&watch, "value")?)?;
        let initial = json!({"value":view,"ops":[],"retired":false});
        let events = core.projection.borrow_mut().fold(&initial, true, vec![]);
        core.frame(initial["value"].clone(), events);
        call(&core.harness, "resume", &[])?;
        core.ready.set(true);
        Core::schedule(&core);
        Ok(session)
    }
}
type Observer = Closure<dyn FnMut(JsValue)>;
fn callback(core: &Rc<Core>, end: bool) -> Observer {
    let weak = Rc::downgrade(core);
    Closure::new(move |value: JsValue| {
        if let Some(core) = weak.upgrade()
            && !core.detached.get()
        {
            // Only enqueue on the native commit line. Host callbacks and receipt reads run later.
            core.pending.borrow_mut().push_back((end, value));
            Core::schedule(&core);
        }
    })
}
struct Core {
    harness: JsValue,
    conv: JsValue,
    id: JsValue,
    journal: Journal,
    pending: RefCell<VecDeque<(bool, JsValue)>>,
    ready: Cell<bool>,
    draining: Cell<bool>,
    detached: Cell<bool>,
    watch: RefCell<Option<JsValue>>,
    projection: RefCell<Projection>,
    waiters: RefCell<Vec<oneshot::Sender<()>>>,
    admissions: Mutex<()>,
    queued: RefCell<BTreeMap<u64, String>>,
    withdrawn: RefCell<BTreeSet<u64>>,
}
impl Core {
    fn frame(&self, native: Value, events: Vec<Event>) {
        self.journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            native,
            events,
        );
    }
    fn schedule(this: &Rc<Self>) {
        if !this.ready.get() || this.pending.borrow().is_empty() || this.draining.replace(true) {
            return;
        }
        let core = this.clone();
        spawn_local(async move {
            loop {
                let next = core.pending.borrow_mut().pop_front();
                let Some((end, item)) = next else { break };
                if end {
                    core.frame(
                        json!({"watchEnd":json(&item).unwrap_or(Value::Null)}),
                        vec![Event::Exited {
                            reason: "Pi Durable observation ended".into(),
                        }],
                    );
                    core.detached.set(true);
                    core.journal.close();
                    break;
                }
                if let Err(err) = core.advance(item).await {
                    core.frame(
                        json!({"projectionError":native::summary(&err)}),
                        vec![Event::Exited {
                            reason: "Pi Durable observation failed".into(),
                        }],
                    );
                    core.detach();
                    core.journal.close();
                    break;
                }
            }
            core.draining.set(false);
            for waiter in core.waiters.take() {
                let _ = waiter.send(());
            }
        });
    }
    fn detach(&self) {
        if let Some(watch) = self.watch.take() {
            let _ = call(&watch, "detach", &[]);
        }
        self.detached.set(true);
    }
    fn open(&self) -> JsResult<()> {
        if self.detached.get() || self.journal.is_closed() {
            Err(error("Pi Durable controller is detached"))
        } else {
            Ok(())
        }
    }
    async fn flush(&self) {
        while self.draining.get() {
            let (tx, rx) = oneshot::channel();
            self.waiters.borrow_mut().push(tx);
            let _ = rx.await;
        }
    }
    async fn receipt(&self, id: u64) -> JsResult<Value> {
        let handle = invoke(
            &self.harness,
            "submission",
            &[JsValue::from_f64(id as f64), ctx()],
        )
        .await?;
        if handle.is_undefined() {
            return Ok(Value::Null);
        }
        json(&invoke(&handle, "status", &[ctx()]).await?)
    }
    async fn advance(&self, item: JsValue) -> JsResult<()> {
        let _admission = self.admissions.lock().await;
        if get(&item, "kind")?.as_string().as_deref() == Some("failed") {
            self.frame(
                json!({"watchError":native::summary(&get(&item,"error")?)}),
                vec![],
            );
            return Ok(());
        }
        let change = json(&item)?;
        let (ended, after_run, ids, prior_inbox) = {
            let p = self.projection.borrow();
            (
                projection::ended(&p.view, &change),
                !p.view["docs"]["pi.live"]["run"]["afterRun"].is_null(),
                p.inputs.clone(),
                p.view["docs"]["pi.inbox"]["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|v| v["mode"] != "write")
                    .filter_map(|v| v["id"].as_u64())
                    .collect::<Vec<_>>(),
            )
        };
        let mut receipts = vec![];
        if ended && !after_run {
            for id in ids {
                match self.receipt(id).await {
                    Ok(r) => receipts.push(r),
                    Err(err) => {
                        self.frame(json!({"receiptError":native::summary(&err)}), vec![]);
                        receipts.push(Value::Null);
                    }
                }
            }
            self.frame(json!({"submissions":receipts}), vec![]);
        }
        let terminal = if ended && after_run {
            vec![Event::TurnEnded {
                outcome: Outcome::Completed,
            }]
        } else {
            crate::failure::durable::submission_end(&receipts)
        };
        let mut events = self.projection.borrow_mut().fold(&change, false, terminal);
        let docs = &change["value"]["docs"];
        let remaining: BTreeSet<_> = docs["pi.inbox"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["id"].as_u64())
            .chain(
                docs["pi.live"]["run"]["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64),
            )
            .collect();
        let mut tracked = self.queued.borrow().clone();
        for id in prior_inbox {
            tracked.entry(id).or_default();
        }
        for (id, input) in tracked {
            if remaining.contains(&id) {
                continue;
            }
            if let Ok(r) = self.receipt(id).await
                && matches!(r["status"].as_str(), Some("placed" | "done" | "unanswered"))
            {
                self.queued.borrow_mut().remove(&id);
                let explicitly_withdrawn = self.withdrawn.borrow_mut().remove(&id);
                if let Some(input_id) = r["requestId"]
                    .as_str()
                    .or_else(|| (!input.is_empty()).then_some(input.as_str()))
                {
                    let input_id = input_id.to_owned();
                    let was_dropped = r["status"] == "unanswered"
                        && r["reason"] == "aborted"
                        && r["entry"].is_null()
                        && !explicitly_withdrawn;
                    events.push(if was_dropped {
                        Event::InputDropped { input_id }
                    } else {
                        Event::InputRemoved { input_id }
                    });
                }
            }
        }
        self.frame(
            json!({"ops":change["ops"],"retired":change["retired"]}),
            events,
        );
        Ok(())
    }
    async fn submit(
        &self,
        mode: &str,
        text: String,
        request_id: Option<String>,
    ) -> JsResult<JsValue> {
        let _serial = self.admissions.lock().await;
        self.open()?;
        let mut body = json!({"type":"input","content":text,"whenBusy":mode});
        if mode == "steer" {
            body["whenIdle"] = "reject".into();
        }
        if let Some(id) = &request_id {
            if id.is_empty() {
                return Err(error("request id must be nonempty"));
            }
            let existing = native::lookup(&self.harness, &self.id, id.clone()).await?;
            if !existing.is_undefined() && !existing.is_null() {
                if mode == "steer" && !native::running(&self.conv, &self.id).await? {
                    return Err(error("no active turn"));
                }
                return Ok(existing);
            }
        }
        let request_id =
            request_id.or_else(|| (mode == "reject").then(|| uuid::Uuid::new_v4().to_string()));
        let mut stamp = crate::stream::RecordStamp::from(Direction::ToRuntime);
        if let Some(id) = &request_id {
            if mode == "reject" {
                stamp = stamp.with_prompt(id, false);
            }
            body["requestId"] = id.as_str().into();
        }
        self.journal.push(
            stamp,
            RecordKind::Request,
            None,
            vec![],
            body.clone(),
            vec![],
        );
        let result = invoke(&self.conv, "submit", &[js(&body)?, ctx()]).await;
        let handle = match result {
            Ok(value) => value,
            Err(err) => {
                self.journal.push(
                    Direction::FromRuntime,
                    RecordKind::Response,
                    None,
                    vec![],
                    native::summary(&err),
                    vec![],
                );
                return Err(err);
            }
        };
        let record = invoke(&handle, "status", &[ctx()]).await?;
        let receipt = json(&record)?;
        let mut events = Vec::new();
        if mode == "followUp"
            && receipt["status"] == "queued"
            && let (Some(id), Some(input)) = (receipt["id"].as_u64(), request_id)
        {
            events.push(Event::InputQueued {
                input_id: input.clone(),
            });
            self.queued.borrow_mut().insert(id, input);
        }
        self.journal.push(
            Direction::FromRuntime,
            RecordKind::Response,
            None,
            vec![],
            receipt,
            events,
        );
        Ok(record)
    }
}
#[wasm_bindgen]
pub struct DurableSession {
    core: Rc<Core>,
    _advance: Observer,
    _end: Observer,
}
#[wasm_bindgen]
impl DurableSession {
    #[wasm_bindgen(getter)]
    pub fn id(&self) -> String {
        self.core
            .id
            .as_f64()
            .expect("validated conversation id")
            .to_string()
    }
    #[wasm_bindgen(getter,js_name=streamId)]
    pub fn stream_id(&self) -> String {
        self.core.journal.id().to_string()
    }
    pub fn records(&self) -> JsResult<Array> {
        let result = Array::new();
        for record in self.core.journal.records() {
            result.push(&js(
                &serde_json::to_value(record.as_ref()).map_err(|e| error(e.to_string()))?
            )?);
        }
        Ok(result)
    }
    pub fn snapshot(&self) -> JsResult<JsValue> {
        js(
            &serde_json::to_value(self.core.journal.snapshot())
                .map_err(|e| error(e.to_string()))?,
        )
    }
    #[wasm_bindgen(js_name=modelReadback)]
    pub fn model_readback(&self) -> JsResult<JsValue> {
        js(&json!(
            self.core.journal.snapshot().into_readback(|s| s.model)
        ))
    }
    #[wasm_bindgen(js_name=usageReadback)]
    pub fn usage_readback(&self) -> JsResult<JsValue> {
        js(&json!(
            self.core.journal.snapshot().into_readback(|s| s.usage)
        ))
    }
    #[wasm_bindgen(js_name=contextUsageReadback)]
    pub fn context_usage_readback(&self) -> JsResult<JsValue> {
        js(&json!(
            self.core
                .journal
                .snapshot()
                .into_readback(|s| s.context_usage)
        ))
    }
    #[wasm_bindgen(js_name=subscribeEvents)]
    pub fn subscribe_events(&self, cursor: JsValue) -> JsResult<DurableEvents> {
        let cursor: Option<crate::EventCursor> = if cursor.is_null() || cursor.is_undefined() {
            None
        } else {
            Some(serde_json::from_value(json(&cursor)?).map_err(|e| error(e.to_string()))?)
        };
        let stream = self
            .core
            .journal
            .events(cursor.as_ref())
            .map_err(|e| error(e.to_string()))?;
        Ok(DurableEvents {
            stream: Mutex::new(stream),
        })
    }
    pub fn subscribe(&self, cursor: JsValue) -> JsResult<DurableSubscription> {
        let cursor: Option<Cursor> = if cursor.is_null() || cursor.is_undefined() {
            None
        } else {
            Some(serde_json::from_value(json(&cursor)?).map_err(|e| error(e.to_string()))?)
        };
        let sub = self
            .core
            .journal
            .subscribe(cursor.as_ref())
            .map_err(|e| error(e.to_string()))?;
        Ok(DurableSubscription {
            stream: Mutex::new(sub),
        })
    }
    pub async fn prompt(&self, text: String, input_id: Option<String>) -> JsResult<JsValue> {
        self.core.submit("reject", text, input_id).await
    }
    pub async fn steer(&self, text: String, input_id: Option<String>) -> JsResult<JsValue> {
        self.core.submit("steer", text, input_id).await
    }
    pub async fn queue(&self, text: String, input_id: String) -> JsResult<JsValue> {
        self.core.submit("followUp", text, Some(input_id)).await
    }
    pub async fn withdraw(&self, input_id: String) -> JsResult<()> {
        let _serial = self.core.admissions.lock().await;
        self.core.open()?;
        let row = native::lookup(&self.core.harness, &self.core.id, input_id).await?;
        if row.is_undefined() {
            return Err(error("input is not queued"));
        }
        let id = get(&row, "id")?;
        let handle = invoke(&self.core.harness, "submission", &[id.clone(), ctx()]).await?;
        if handle.is_undefined() {
            return Err(error("input is not queued"));
        }
        let number = id.as_f64().unwrap_or_default() as u64;
        self.core.withdrawn.borrow_mut().insert(number);
        let result = invoke(&handle, "abort", &[ctx()]).await;
        match result {
            Ok(v) if v.as_string().as_deref() == Some("aborted") => Ok(()),
            other => {
                self.core.withdrawn.borrow_mut().remove(&number);
                Err(other
                    .err()
                    .unwrap_or_else(|| error("input is already placed or settled")))
            }
        }
    }
    pub async fn wait(&self, submission: f64) -> JsResult<JsValue> {
        self.core.open()?;
        let handle = invoke(
            &self.core.harness,
            "submission",
            &[submission.into(), ctx()],
        )
        .await?;
        if handle.is_undefined() {
            return Err(error("unknown submission"));
        }
        let record = invoke(&handle, "status", &[ctx()]).await?;
        if get(&record, "conversationId")? != self.core.id {
            return Err(error("submission belongs to another conversation"));
        }
        let settled = invoke(&handle, "wait", &[ctx()]).await?;
        self.core.flush().await;
        Ok(settled)
    }
    pub async fn abort(&self) -> JsResult<()> {
        let _serial = self.core.admissions.lock().await;
        self.core.open()?;
        if !native::running(&self.core.conv, &self.core.id).await? {
            return Err(error("no active turn"));
        }
        let abort = Promise::resolve(&call(&self.core.conv, "abort", &[ctx()])?);
        let timeout = Promise::new(&mut |resolve, _| {
            let global = js_sys::global();
            let _ = call(&global, "setTimeout", &[resolve.into(), 10_000.into()]);
        });
        let racers = Array::new();
        racers.push(&abort);
        racers.push(&timeout);
        JsFuture::from(Promise::race(&racers)).await?;
        drop(_serial);
        self.core.flush().await;
        Ok(())
    }
    pub async fn close(&self) -> JsResult<()> {
        let _serial = self.core.admissions.lock().await;
        self.core.detach();
        drop(_serial);
        self.core.flush().await;
        self.core.journal.host_event(Event::Released);
        self.core.journal.close();
        Ok(())
    }
}
impl Drop for DurableSession {
    fn drop(&mut self) {
        self.core.detach();
        self.core.journal.host_event(Event::Released);
        self.core.journal.close();
    }
}
#[wasm_bindgen]
pub struct DurableSubscription {
    stream: Mutex<crate::Subscription>,
}
#[wasm_bindgen]
pub struct DurableEvents {
    stream: Mutex<crate::EventSubscription>,
}
#[wasm_bindgen]
impl DurableEvents {
    pub async fn next(&self) -> JsResult<JsValue> {
        let Some(item) = self.stream.lock().await.next().await else {
            return Ok(JsValue::UNDEFINED);
        };
        js(&serde_json::to_value(item).map_err(|e| error(e.to_string()))?)
    }
}
#[wasm_bindgen]
impl DurableSubscription {
    pub async fn next(&self) -> JsResult<JsValue> {
        let Some(record) = self.stream.lock().await.next().await else {
            return Ok(JsValue::UNDEFINED);
        };
        js(&serde_json::to_value(record.as_ref()).map_err(|e| error(e.to_string()))?)
    }
}
