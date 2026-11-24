use super::*;
use crate::chat::{ChatTurn, Item, Role, TurnStatus};
use std::{process::Stdio, sync::atomic::Ordering, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const MAX_ACTIVE: usize = 16;

#[derive(Clone)]
enum Outcome {
    Known(bool, Value),
    Unknown(String),
}

fn turn_result(turn: &ChatTurn) -> Outcome {
    match turn.status {
        TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Aborted => {
            let text: String = turn
                .items
                .iter()
                .filter_map(|item| match &item.item {
                    Item::Message {
                        role: Role::Assistant,
                        text,
                        ..
                    } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let text = String::from_utf8_lossy(&text.as_bytes()[..text.len().min(OUTPUT_LIMIT)]);
            Outcome::Known(
                turn.status == TurnStatus::Completed,
                json!({"turn":turn.id,"status":turn.status,"text":text,"structured_output":turn.structured_output}),
            )
        }
        _ => Outcome::Unknown(format!("chat turn has status {:?}", turn.status)),
    }
}

impl Workbench {
    /// One atomic timer sweep and dispatch. Concurrent calls share the same
    /// durable claim. The service calls this automatically while its router is live.
    pub fn tick_workflows(&self) -> Result<usize> {
        let engine = &self.0.workflows;
        let now = engine.clock.now_ms();
        let projects: std::collections::BTreeSet<_> =
            self.projects().into_iter().map(|p| p.id).collect();
        let eligible =
            |w: &Workflow| projects.contains(&w.project) && w.next_ms.is_some_and(|at| at <= now);
        let advance = |r: &Run| {
            projects.contains(&r.project)
                && ((r.status == Status::Ready && r.webhook_due_ms.is_none_or(|at| at <= now))
                    || (r.status == Status::Waiting && r.wake_at_ms.is_some_and(|at| at <= now)))
        };
        {
            let d = engine.db.lock().unwrap();
            let remaining = MAX_ACTIVE.saturating_sub(
                d.runs
                    .values()
                    .filter(|r| r.status == Status::Running)
                    .count(),
            );
            if !d.workflows.values().any(eligible)
                && !d.runs.values().any(|r| {
                    advance(r)
                        && (remaining > 0
                            || r.status == Status::Waiting
                            || matches!(r.steps[r.cursor], Step::Ask { .. } | Step::Wait { .. }))
                })
            {
                return Ok(0);
            }
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Refused("workflow dispatcher needs a Tokio runtime".into()))?;
        let (triggered, jobs) = engine.update(|d| {
            let mut triggered = 0;
            let due: Vec<_> = d
                .workflows
                .values()
                .filter(|w| eligible(w))
                .map(|w| w.id)
                .collect();
            for id in due {
                let w = d.workflows.get_mut(&id).unwrap();
                let at = w.next_ms.unwrap();
                w.next_ms = w.definition.schedule.next_after(now)?;
                if w.definition.schedule.missed(at, now) {
                    w.last_missed_ms = Some(at);
                    continue;
                }
                let w = w.clone();
                insert_run(
                    d,
                    &w,
                    Trigger::Scheduled(at),
                    json!({"scheduled_ms":at}),
                    now,
                )?;
                triggered += 1;
            }
            let mut remaining = MAX_ACTIVE.saturating_sub(
                d.runs
                    .values()
                    .filter(|r| r.status == Status::Running)
                    .count(),
            );
            let mut jobs = Vec::new();
            for run in d.runs.values_mut().filter(|r| advance(r)) {
                if run.status == Status::Waiting {
                    run.finish(true, json!({"woke_at_ms":now}), now);
                }
                if run.status == Status::Ready {
                    let external = matches!(
                        run.steps[run.cursor],
                        Step::Agent { .. } | Step::Script { .. }
                    );
                    if external && remaining == 0 {
                        continue;
                    }
                    run.webhook_due_ms = None;
                    run.webhook_templates = None;
                    run.begin(now);
                    if external {
                        remaining -= 1;
                        jobs.push(run.clone());
                    }
                }
            }
            Ok((triggered, jobs))
        })?;
        for run in jobs {
            let app = self.clone();
            handle.spawn(async move {
                let attempt = run.attempt.as_ref().unwrap();
                let outcome = match app.execute(&run, attempt).await {
                    Ok(outcome) => outcome,
                    Err(e) => Outcome::Unknown(e.to_string()),
                };
                // A transient persistence error must not replay an already dispatched call.
                let mut delay = Duration::from_millis(100);
                while let Err(e) = app.record_outcome(run.id, attempt.id, outcome.clone()) {
                    eprintln!("Kar workflow {} receipt could not be saved: {e}", run.id);
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            });
        }
        Ok(triggered)
    }
    pub(in crate::workbench) fn start_workflows(&self) -> Result<()> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Refused("workbench router needs a Tokio runtime for workflows".into())
        })?;
        if self.0.workflows.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.watch_sources();
        let weak = Arc::downgrade(&self.0);
        let mut changed = self.0.workflows.changed.subscribe();
        handle.spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let app = Workbench(inner);
                if let Err(e) = app
                    .recover_agent_receipts()
                    .and_then(|_| app.tick_workflows())
                {
                    eprintln!("Kar workflow dispatcher: {e}");
                }
                drop(app);
                tokio::select! {
                    r=changed.changed()=>{if r.is_err(){break;}},
                    _=tokio::time::sleep(Duration::from_millis(250))=>{},
                }
            }
        });
        Ok(())
    }

    fn record_outcome(&self, run: Uuid, attempt: Uuid, outcome: Outcome) -> Result<()> {
        let e = &self.0.workflows;
        e.update(|d| {
            let r = d.runs.get_mut(&run).ok_or_else(missing)?;
            if !matches!(r.status, Status::Running | Status::Reconcile)
                || r.attempt.as_ref().map(|a| a.id) != Some(attempt)
            {
                return Ok(());
            }
            match outcome {
                Outcome::Known(success, value) => r.finish(success, value, e.clock.now_ms()),
                Outcome::Unknown(reason) => {
                    r.status = Status::Reconcile;
                    r.detail = Some(reason);
                }
            }
            Ok(())
        })
    }
    fn recover_agent_receipts(&self) -> Result<()> {
        let pending: Vec<_> = self
            .0
            .workflows
            .db
            .lock()
            .unwrap()
            .runs
            .values()
            .filter(|r| r.status == Status::Reconcile)
            .filter_map(|r| {
                let a = r.attempt.as_ref()?;
                Some((r.project, r.id, a.chat?, a.id))
            })
            .collect();
        for (project, run, chat, input) in pending {
            if self.binding(project, chat).is_ok()
                && let Ok(history) = self.0.chats.history(chat)
                && let Some(turn) = history.turns.iter().find(|t| t.id == input)
                && let result @ Outcome::Known(..) = turn_result(turn)
            {
                self.record_outcome(run, input, result)?;
            }
        }
        Ok(())
    }

    async fn cancellation(&self, run: Uuid) {
        let mut watch = self.0.workflows.changed.subscribe();
        loop {
            if self
                .0
                .workflows
                .db
                .lock()
                .unwrap()
                .runs
                .get(&run)
                .is_none_or(|r| r.cancel_requested)
            {
                return;
            }
            if watch.changed().await.is_err() {
                return;
            }
        }
    }
    async fn execute(&self, run: &Run, a: &Attempt) -> Result<Outcome> {
        if self.workflow_run(run.project, run.id)?.cancel_requested {
            return Ok(Outcome::Known(
                false,
                json!({"cancelled_before_dispatch":true}),
            ));
        }
        match &run.steps[run.cursor] {
            Step::Script { command } => self.script(run, a, command).await,
            Step::Agent {
                runtime,
                model,
                prompt,
            } => {
                let chat = a.chat.unwrap();
                if let Err(e) = self
                    .create_chat(run.project, chat, runtime, model.clone())
                    .await
                {
                    return Ok(Outcome::Known(
                        false,
                        json!({"chat":chat,"before_input":e.to_string()}),
                    ));
                }
                let controller = self.controller(chat);
                let mut owner = controller.owner.lock().await;
                self.attach(chat, &mut owner).await?;
                let session = owner
                    .as_ref()
                    .ok_or_else(|| Error::Closed("workflow chat is unavailable".into()))?;
                let text = if run.webhook_rendered {
                    prompt.clone()
                } else {
                    let context = context(run)?;
                    format!(
                        "{prompt}\n\nWorkflow input and previous step results (JSON):\n{}",
                        String::from_utf8_lossy(&context)
                    )
                };
                if self.workflow_run(run.project, run.id)?.cancel_requested {
                    return Ok(Outcome::Known(
                        false,
                        json!({"cancelled_before_input":true}),
                    ));
                }
                let send = session.send(a.id, text);
                tokio::pin!(send);
                let turn = tokio::select! {
                    biased;
                    result=&mut send=>result?,
                    _=self.cancellation(run.id)=>{
                        session.abort().await?;
                        send.await?
                    }
                };
                Ok(turn_result(&turn))
            }
            _ => unreachable!("only external steps are dispatched"),
        }
    }

    async fn script(&self, run: &Run, a: &Attempt, source: &str) -> Result<Outcome> {
        let project = self.project(run.project)?;
        let space = self.workspace(&project)?;
        let cwd = space.root_handle()?;
        #[cfg(target_os = "macos")]
        let path = {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let pin = cwd.metadata()?;
            [
                format!("/dev/fd/{}", cwd.as_raw_fd()),
                format!("/.vol/{}/{}", pin.dev(), pin.ino()),
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| {
                std::fs::metadata(p)
                    .is_ok_and(|m| m.is_dir() && m.dev() == pin.dev() && m.ino() == pin.ino())
            })
            .ok_or_else(|| Error::Unsupported("cannot pin workflow working directory".into()))?
        };
        #[cfg(not(target_os = "macos"))]
        let path = project.root.clone();
        #[cfg(unix)]
        let mut cmd = {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(source).process_group(0);
            cmd
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = Command::new("cmd.exe");
            cmd.args(["/D", "/S", "/C", source]);
            cmd
        };
        // A private file supplies context without environment-size limits or a pipe write deadlock.
        let mut input = tempfile::tempfile()?;
        use std::io::{Seek, Write};
        input.write_all(&context(run)?)?;
        input.rewind()?;
        cmd.current_dir(path)
            .env_remove("KAR_WORKBENCH_TOKEN")
            .env("KAR_WORKFLOW_ATTEMPT_ID", a.id.to_string())
            .env("KAR_WORKFLOW_RUN_ID", run.id.to_string())
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return Ok(Outcome::Known(false, json!({"spawn_error":e.to_string()}))),
        };
        drop(cwd);
        #[cfg(unix)]
        let pid = child.id().map(|id| nix::unistd::Pid::from_raw(id as i32));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let wait = async {
            let (status, stdout, stderr) =
                tokio::try_join!(child.wait(), bounded(stdout), bounded(stderr))?;
            io::Result::Ok((status, stdout, stderr))
        };
        tokio::pin!(wait);
        let (status, stdout, stderr) = tokio::select! {
            biased;
            result=&mut wait=> result?,
            _=self.cancellation(run.id)=>{
                #[cfg(unix)]
                if let Some(pid)=pid{let _=nix::sys::signal::killpg(pid,nix::sys::signal::Signal::SIGKILL);}
                // The command may already have modified an external system.
                return Ok(Outcome::Unknown("script cancelled after dispatch; confirm its effects".into()));
            }
        };
        Ok(Outcome::Known(
            status.success(),
            json!({"exit_code":status.code(),"stdout":stdout,"stderr":stderr}),
        ))
    }
}
fn context(r: &Run) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"run_id":r.id,"input":r.input,"receipts":r.receipts}),
    )?)
}
async fn bounded(mut stream: impl AsyncRead + Unpin) -> io::Result<Value> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let n = count.min(OUTPUT_LIMIT - bytes.len());
        bytes.extend_from_slice(&chunk[..n]);
        truncated |= n < count;
    }
    Ok(json!({"text":String::from_utf8_lossy(&bytes),"truncated":truncated}))
}
