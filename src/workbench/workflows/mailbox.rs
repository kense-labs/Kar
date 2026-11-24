//! Values, waiting runs and buffered signals commit in the same workflow snapshot.
use super::*;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(in crate::workbench) enum Message {
    Set {
        key: String,
        value: Value,
    },
    Delete {
        key: String,
    },
    Signal {
        workflow: Uuid,
        channel: String,
        value: Value,
    },
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Mailbox {
    values: BTreeMap<Uuid, BTreeMap<String, Value>>,
    buffered: Vec<(Uuid, String, Value)>,
    pub(super) commands: BTreeMap<Uuid, (Uuid, Message, Value)>,
}
impl Mailbox {
    fn entries(&self, project: Uuid, prefix: &str) -> Vec<Value> {
        self.values
            .get(&project)
            .into_iter()
            .flat_map(|m| {
                m.range(prefix.to_owned()..)
                    .take_while(|(k, _)| k.starts_with(prefix))
            })
            .map(|(key, value)| json!({"key":key,"value":value}))
            .collect()
    }
    pub(super) fn consume(&mut self, run: &mut Run, now: i64) {
        let Some(channel) = waiting_channel(run) else {
            return;
        };
        if let Some(pos) = self
            .buffered
            .iter()
            .position(|(workflow, ch, _)| *workflow == run.workflow && ch == channel)
        {
            let (_, _, value) = self.buffered.remove(pos);
            run.finish(true, value, now);
        }
    }
    pub(super) fn remove(&mut self, workflow: Uuid) {
        self.buffered.retain(|(id, _, _)| *id != workflow);
    }
}
fn waiting_channel(run: &Run) -> Option<&str> {
    if run.status == Status::Waiting
        && let Some(Step::WaitSignal { channel }) = run.steps.get(run.cursor)
    {
        Some(channel)
    } else {
        None
    }
}
pub(super) fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 1024 && !key.contains('\0')
}

impl Workbench {
    pub(in crate::workbench) fn stored_workflow_values(
        &self,
        project: Uuid,
        prefix: &str,
    ) -> Result<Vec<Value>> {
        self.project(project)?;
        Ok(self
            .0
            .workflows
            .db
            .lock()
            .unwrap()
            .mailbox
            .entries(project, prefix))
    }
    pub(in crate::workbench) fn workflow_message(
        &self,
        project: Uuid,
        id: Uuid,
        msg: Message,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        self.project(project)?;
        let engine = &self.0.workflows;
        let now = engine.clock.now_ms();
        engine.update_checked(
            |_| authorize(),
            |db, ()| {
                if let Some((p, old, result)) = db.mailbox.commands.get(&id) {
                    return if *p == project && old == &msg {
                        Ok(result.clone())
                    } else {
                        Err(Error::Refused("workflow command already used".into()))
                    };
                }
                if db.actions.contains_key(&id) {
                    return Err(Error::Refused("workflow command already used".into()));
                }
                let result = match &msg {
                    Message::Set { key, value } if valid_key(key) => {
                        db.mailbox
                            .values
                            .entry(project)
                            .or_default()
                            .insert(key.clone(), value.clone());
                        json!({"entries":db.mailbox.entries(project,key)})
                    }
                    Message::Delete { key } if valid_key(key) => {
                        if let Some(store) = db.mailbox.values.get_mut(&project) {
                            store.remove(key);
                        }
                        json!({"entries":db.mailbox.entries(project,key)})
                    }
                    Message::Signal {
                        workflow,
                        channel,
                        value,
                    } if valid_key(channel) => {
                        same_project(
                            project,
                            db.workflows.get(workflow).ok_or_else(missing)?.project,
                        )?;
                        let target = db
                            .runs
                            .values()
                            .filter(|r| {
                                r.workflow == *workflow
                                    && waiting_channel(r) == Some(channel.as_str())
                            })
                            .min_by_key(|r| {
                                (r.attempt.as_ref().map(|a| a.started_ms), r.started_ms, r.id)
                            })
                            .map(|r| r.id);
                        if let Some(run) = target {
                            db.runs
                                .get_mut(&run)
                                .unwrap()
                                .finish(true, value.clone(), now);
                            json!({"delivery":"delivered","run_id":run})
                        } else {
                            db.mailbox
                                .buffered
                                .push((*workflow, channel.clone(), value.clone()));
                            json!({"delivery":"buffered"})
                        }
                    }
                    _ => {
                        return Err(Error::Refused(
                            "invalid workflow key or signal channel".into(),
                        ));
                    }
                };
                db.mailbox
                    .commands
                    .insert(id, (project, msg, result.clone()));
                Ok(result)
            },
        )
    }
}
