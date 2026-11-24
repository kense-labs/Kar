//! Project-scoped MCP control of the same persistent engine as the workbench clients.
use super::*;
use crate::{
    Error, Result,
    workbench::workflows::{Action, Status},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Definitions {
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    List {
        workflow_id: Option<Uuid>,
        status: Option<Status>,
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    Questions {
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    Hooks {
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    Read {
        run_id: Uuid,
    },
    Start {
        workflow_id: Uuid,
        command_id: Uuid,
        #[serde(default)]
        input: Value,
    },
    Resume {
        run_id: Uuid,
        command_id: Uuid,
        attempt_id: Uuid,
    },
    Signal {
        run_id: Uuid,
        command_id: Uuid,
        attempt_id: Uuid,
        value: Value,
    },
    Reconcile {
        run_id: Uuid,
        command_id: Uuid,
        attempt_id: Uuid,
        succeeded: bool,
        evidence: String,
        value: Value,
    },
    Cancel {
        run_id: Uuid,
        command_id: Uuid,
    },
}

pub(super) fn call(app: &Workbench, caller: &Caller, args: Value) -> Result<Value> {
    // Parse the target independently so action-specific fields can remain strict.
    let mut args = args;
    let fields = args
        .as_object_mut()
        .ok_or_else(|| Error::Refused("expected workflow arguments".into()))?;
    let project = serde_json::from_value(fields.remove("project_id").unwrap_or(Value::Null))?;
    let thread = serde_json::from_value(fields.remove("thread_id").unwrap_or(Value::Null))?;
    let (project, chat) = toolkit::target(app, caller, project, thread)?;
    let grant = caller
        .grant
        .as_ref()
        .ok_or_else(|| Error::Refused("model session required".into()))?;
    let authorize = |write: bool| {
        let db = app.0.catalog.lock().unwrap();
        let current = Policy::current(&db, project, chat);
        if !grant.active()
            || current != caller.policy.as_ref()
            || !current.is_some_and(|p| {
                if write {
                    !caller.plan && p.writes_workflows()
                } else {
                    p.reads_workflows()
                }
            })
        {
            return Err(Error::Refused(
                "workflow access was not granted to this chat".into(),
            ));
        }
        Ok(())
    };
    authorize(false)?;
    let mutate = |run, command, act| {
        app.workflow_action_checked(project, run, command, act, || authorize(true))
            .map(|r| json!({"run":r}))
    };
    let operation = serde_json::from_value::<Operation>(args)?;
    match operation {
        Operation::Read { run_id } => Ok(json!({"run":app.workflow_run(project,run_id)?})),
        Operation::Start {
            workflow_id,
            command_id,
            input,
        } => Ok(
            json!({"run":app.trigger_workflow_checked(project,workflow_id,command_id,input,||authorize(true))?}),
        ),
        Operation::Resume {
            run_id,
            command_id,
            attempt_id,
        } => mutate(
            run_id,
            command_id,
            Action::Retry {
                attempt: attempt_id,
            },
        ),
        Operation::Cancel { run_id, command_id } => mutate(run_id, command_id, Action::Cancel),
        Operation::Signal {
            run_id,
            command_id,
            attempt_id,
            value,
        } => mutate(
            run_id,
            command_id,
            Action::Answer {
                attempt: attempt_id,
                answer: value,
            },
        ),
        Operation::Reconcile {
            run_id,
            command_id,
            attempt_id,
            succeeded,
            evidence,
            value,
        } => mutate(
            run_id,
            command_id,
            Action::Reconcile {
                attempt: attempt_id,
                succeeded,
                evidence,
                result: value,
            },
        ),
        op => {
            let mut index = app.workflow_index(project)?;
            let (key, start, limit, rows) = match op {
                Operation::List {
                    workflow_id,
                    status,
                    cursor,
                    limit,
                } => {
                    let status = status.map(|s| json!(s));
                    let rows = take(&mut index, "runs")
                        .into_iter()
                        .filter(|v| {
                            workflow_id.is_none_or(|id| v["workflow"] == json!(id))
                                && status.as_ref().is_none_or(|s| &v["status"] == s)
                        })
                        .collect();
                    ("runs", cursor, limit, rows)
                }
                Operation::Definitions { cursor, limit } => {
                    ("workflows", cursor, limit, take(&mut index, "workflows"))
                }
                Operation::Questions { cursor, limit } => {
                    ("questions", cursor, limit, take(&mut index, "questions"))
                }
                Operation::Hooks { cursor, limit } => {
                    let hooks=take(&mut index,"workflows").into_iter().filter(|w|w["schedule"]["type"]=="webhook").map(|w|json!({"workflow":w["id"],"path":format!("/hooks/workflows/{project}/{}",w["id"].as_str().unwrap_or(""))})).collect();
                    ("hooks", cursor, limit, hooks)
                }
                _ => unreachable!(),
            };
            toolkit::page(rows, key, start.unwrap_or(0), limit)
        }
    }
}
fn take(index: &mut Value, key: &str) -> Vec<Value> {
    index[key]
        .as_array_mut()
        .map(std::mem::take)
        .unwrap_or_default()
}
