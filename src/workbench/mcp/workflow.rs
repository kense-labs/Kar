//! Project-scoped MCP control of the same persistent engine as the workbench clients.
use super::*;
use crate::{
    Error, Result,
    workbench::workflows::{Action, Message, Status},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Store {
        key: Option<String>,
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    StoreSet {
        key: String,
        value: Value,
        command_id: Uuid,
    },
    StoreDelete {
        key: String,
        command_id: Uuid,
    },
    Definitions {
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    List {
        workflow_id: Option<Uuid>,
        status: Option<Status>,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        subject: Option<String>,
        cursor: Option<usize>,
        limit: Option<usize>,
    },
    Questions {
        #[serde(default)]
        kind: Option<String>,
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
        #[serde(default)]
        parent_run_id: Option<Uuid>,
    },
    Resume {
        run_id: Uuid,
        command_id: Uuid,
        attempt_id: Uuid,
    },
    Signal {
        run_id: Option<Uuid>,
        attempt_id: Option<Uuid>,
        workflow_id: Option<Uuid>,
        channel: Option<String>,
        command_id: Uuid,
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
        #[serde(default)]
        reason: Option<String>,
    },
    Stop {
        run_id: Uuid,
        command_id: Uuid,
        #[serde(default)]
        reason: Option<String>,
    },
    Skip {
        run_id: Uuid,
        command_id: Uuid,
        #[serde(default)]
        reason: Option<String>,
    },
    Withdraw {
        run_id: Uuid,
        command_id: Uuid,
        #[serde(default)]
        attempt_id: Option<Uuid>,
        #[serde(default)]
        reason: Option<String>,
    },
}

pub(super) fn call(app: &Workbench, caller: &Caller, args: Value) -> Result<Value> {
    // Parse the target independently so action-specific fields can remain strict.
    let mut args = args;
    let fields = args
        .as_object_mut()
        .ok_or_else(|| Error::Refused("expected workflow arguments".into()))?;
    let project: Option<Uuid> =
        serde_json::from_value(fields.remove("project_id").unwrap_or(Value::Null))?;
    let thread: Option<Uuid> =
        serde_json::from_value(fields.remove("thread_id").unwrap_or(Value::Null))?;

    let (project, chat_scope) = if caller.grant.is_some() {
        let (p, c) = toolkit::target(app, caller, project, thread)?;
        (p, Some(c))
    } else {
        let p = project.ok_or_else(|| Error::Refused("project_id is required".into()))?;
        let registered = app.project(p)?;
        if registered.metadata.deleted {
            return Err(super::super::missing());
        }
        if let Some(c) = thread {
            app.binding(p, c)?;
        }
        (p, thread)
    };

    let authorize = |write: bool| -> Result<()> {
        let Some(grant) = caller.grant.as_ref() else {
            if write && caller.plan {
                return Err(Error::Refused(
                    "workflow writes require an execute grant".into(),
                ));
            }
            return Ok(());
        };
        let chat = chat_scope.expect("model grant chat");
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
            parent_run_id,
        } => {
            let parent = if parent_run_id.is_some() {
                parent_run_id
            } else {
                let mut cur = chat_scope;
                let mut seen = std::collections::BTreeSet::new();
                let mut found = None;
                while let Some(c) = cur {
                    if !seen.insert(c) {
                        break;
                    }
                    if let Some(run_id) = app.0.workflows.active_run_for_chat(c)
                        && app.workflow_run(project, run_id).is_ok()
                    {
                        found = Some(run_id);
                        break;
                    }
                    cur = super::orchestrator::milestone_parent(&app.0.catalog.lock().unwrap(), c);
                }
                found
            };
            Ok(json!({
                "run": app.trigger_workflow_checked(
                    project,
                    workflow_id,
                    command_id,
                    input,
                    parent,
                    || authorize(true),
                )?
            }))
        }
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
        Operation::Cancel {
            run_id,
            command_id,
            reason,
        } => mutate(run_id, command_id, Action::Cancel { reason }),
        Operation::Stop {
            run_id,
            command_id,
            reason,
        } => mutate(run_id, command_id, Action::Stop { reason }),
        Operation::Skip {
            run_id,
            command_id,
            reason,
        } => mutate(run_id, command_id, Action::Skip { reason }),
        Operation::Withdraw {
            run_id,
            command_id,
            attempt_id,
            reason,
        } => mutate(
            run_id,
            command_id,
            Action::Withdraw {
                attempt: attempt_id,
                reason,
            },
        ),
        Operation::Store { key, cursor, limit } => toolkit::page(
            app.stored_workflow_values(project, key.as_deref().unwrap_or(""))?,
            "entries",
            cursor.unwrap_or(0),
            limit,
        ),
        Operation::StoreSet {
            key,
            value,
            command_id,
        } => app.workflow_message(project, command_id, Message::Set { key, value }, || {
            authorize(true)
        }),
        Operation::StoreDelete { key, command_id } => {
            app.workflow_message(project, command_id, Message::Delete { key }, || {
                authorize(true)
            })
        }
        Operation::Signal {
            run_id: Some(run),
            attempt_id: Some(attempt),
            workflow_id: None,
            channel: None,
            command_id,
            value,
        } => {
            let mut result = mutate(
                run,
                command_id,
                Action::Answer {
                    attempt,
                    answer: value,
                },
            )?;
            result["delivery"] = "delivered".into();
            Ok(result)
        }
        Operation::Signal {
            run_id: None,
            attempt_id: None,
            workflow_id: Some(workflow),
            channel: Some(channel),
            command_id,
            value,
        } => app.workflow_message(
            project,
            command_id,
            Message::Signal {
                workflow,
                channel,
                value,
            },
            || authorize(true),
        ),
        Operation::Signal { .. } => Err(Error::Refused(
            "signal requires a run and attempt or a workflow and channel".into(),
        )),
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
                    tag,
                    subject,
                    cursor,
                    limit,
                } => {
                    let status = status.map(|s| json!(s));
                    let rows = take(&mut index, "runs")
                        .into_iter()
                        .filter(|v| {
                            workflow_id.is_none_or(|id| v["workflow"] == json!(id))
                                && status.as_ref().is_none_or(|s| &v["status"] == s)
                                && tag.as_ref().is_none_or(|t| {
                                    v["tags"].as_array().is_some_and(|ts| {
                                        ts.iter().any(|x| x.as_str() == Some(t.as_str()))
                                    })
                                })
                                && subject
                                    .as_ref()
                                    .is_none_or(|s| v["subject"].as_str() == Some(s.as_str()))
                        })
                        .collect();
                    ("runs", cursor, limit, rows)
                }
                Operation::Definitions { cursor, limit } => {
                    ("workflows", cursor, limit, take(&mut index, "workflows"))
                }
                Operation::Questions {
                    kind,
                    cursor,
                    limit,
                } => {
                    let rows = take(&mut index, "questions")
                        .into_iter()
                        .filter(|v| {
                            kind.as_ref()
                                .is_none_or(|k| v["kind"].as_str() == Some(k.as_str()))
                        })
                        .collect();
                    ("questions", cursor, limit, rows)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbench::workflows::Definition;

    #[tokio::test]
    async fn external_and_owner_mcp_callers_work_without_grant_and_support_withdraw_and_filters()
    -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root)?;
        let app = Workbench::open(
            crate::chat::ChatStore::open(tmp.path().join("store"))?,
            std::slice::from_ref(&root),
        )?;
        let project = app.register(&root, "proj")?.id;

        let wf_id = Uuid::new_v4();
        let def: Definition = serde_json::from_value(json!({
            "name": "External Flow",
            "enabled": true,
            "schedule": {"type": "manual"},
            "tags": ["release", "nightly"],
            "subject": "pr-101",
            "steps": [
                {"type": "ask", "question": "Approve release?", "kind": "approval"},
                {"type": "wait", "duration_ms": 0}
            ]
        }))?;
        app.save_workflow(project, wf_id, def, None)?;

        let owner = Caller {
            identity: Identity::Owner,
            grant: None,
            plan: false,
            files_allowed: true,
            policy: None,
        };
        let readonly_ext = Caller {
            identity: Identity::External(Uuid::new_v4()),
            grant: None,
            plan: true,
            files_allowed: false,
            policy: None,
        };

        // Missing project_id must be refused cleanly when caller.grant.is_none().
        assert!(call(&app, &owner, json!({"action": "list"})).is_err());
        // Read-only external caller cannot mutate workflows.
        assert!(
            call(
                &app,
                &readonly_ext,
                json!({"action": "start", "project_id": project, "workflow_id": wf_id, "command_id": Uuid::new_v4()})
            )
            .is_err()
        );

        // Owner/external caller with execute scope starts workflow without thread_id or model grant!
        let started = call(
            &app,
            &owner,
            json!({
                "action": "start",
                "project_id": project,
                "workflow_id": wf_id,
                "command_id": Uuid::new_v4()
            }),
        )?;
        let run_id: Uuid = serde_json::from_value(started["run"]["id"].clone())?;

        app.tick_workflows()?;

        // Filter questions by kind.
        let matching_q = call(
            &app,
            &readonly_ext,
            json!({"action": "questions", "project_id": project, "kind": "approval"}),
        )?;
        assert_eq!(matching_q["questions"].as_array().unwrap().len(), 1);
        let non_matching_q = call(
            &app,
            &readonly_ext,
            json!({"action": "questions", "project_id": project, "kind": "free_text"}),
        )?;
        assert_eq!(non_matching_q["questions"].as_array().unwrap().len(), 0);

        // Withdraw question via MCP without cancelling workflow!
        let withdrawn = call(
            &app,
            &owner,
            json!({
                "action": "withdraw",
                "project_id": project,
                "run_id": run_id,
                "command_id": Uuid::new_v4(),
                "reason": "no longer needed"
            }),
        )?;
        assert_eq!(withdrawn["run"]["status"], "ready");
        assert_eq!(withdrawn["run"]["receipts"][0]["result"]["withdrawn"], true);

        // Filter list by tag and subject.
        let by_tag = call(
            &app,
            &readonly_ext,
            json!({"action": "list", "project_id": project, "tag": "release", "subject": "pr-101"}),
        )?;
        assert_eq!(by_tag["runs"].as_array().unwrap().len(), 1);
        let miss_tag = call(
            &app,
            &readonly_ext,
            json!({"action": "list", "project_id": project, "tag": "other"}),
        )?;
        assert_eq!(miss_tag["runs"].as_array().unwrap().len(), 0);
        Ok(())
    }
}
