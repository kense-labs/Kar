use super::*;
use crate::workbench::mcp::milestones::Log;
impl Engine {
    pub(in crate::workbench) fn milestone_log(&self) -> Log {
        self.db.lock().unwrap().milestones.clone()
    }
    pub(in crate::workbench) fn milestone_run(&self, chat: Uuid) -> Option<Uuid> {
        self.db
            .lock()
            .unwrap()
            .runs
            .values()
            .find(|r| {
                r.attempt
                    .iter()
                    .chain(r.receipts.iter().map(|x| &x.attempt))
                    .any(|a| a.chat == Some(chat))
            })
            .map(|r| r.id)
    }
}
pub(super) fn record(old: &Data, new: &mut Data, now: i64) {
    for r in new.runs.values() {
        let previous = old.runs.get(&r.id);
        let old_status = previous.map(|p| &p.status);
        if old_status == Some(&r.status)
            && previous.and_then(|p| p.attempt.as_ref()).map(|a| a.id)
                == r.attempt.as_ref().map(|a| a.id)
        {
            continue;
        }
        let title = new
            .workflows
            .get(&r.workflow)
            .map(|w| w.definition.name.clone())
            .unwrap_or_else(|| r.workflow.to_string());
        let mut emit = |kind: &str, extra: Value| {
            let mut v = json!({"type":kind,"id":format!("workflow:{}:{}:{kind}",r.id,old.revision+1),"at":now,"workflowRunId":r.id,"workflow":title,"runTitle":title,"parentWorkflowRunId":null});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            new.milestones.push(v);
        };
        if previous.is_none() {
            emit("workflow.run_started", json!({}));
        }
        if old_status == Some(&Status::Asking)
            && r.status != Status::Asking
            && let Some(a) = previous.and_then(|v| v.attempt.as_ref())
        {
            emit(
                "workflow.input_resolved",
                json!({"step":a.id,"resolution":if r.status==Status::Cancelled{"cancelled"}else{"answered"}}),
            );
        }
        if r.status == Status::Asking
            && let Some(a) = &r.attempt
            && let Some(Step::Ask { question }) = r.steps.get(a.step)
        {
            emit(
                "workflow.needs_input",
                json!({"step":a.id,"question":question}),
            );
        }
        if matches!(r.status, Status::Failed | Status::Reconcile) {
            emit(
                "workflow.run_paused",
                json!({"reason":r.detail.clone().unwrap_or_else(||"Step requires reconciliation".into())}),
            );
        } else if old_status.is_some_and(|s| matches!(s, Status::Failed | Status::Reconcile))
            && r.status != Status::Cancelled
        {
            emit("workflow.run_resumed", json!({}));
        }
        if matches!(r.status, Status::Cancelled | Status::Completed) {
            emit(
                "workflow.run_ended",
                json!({"outcome":if r.status==Status::Completed{"completed"}else{"cancelled"},"detail":r.detail}),
            );
        }
    }
}
