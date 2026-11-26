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
    let next_rev = old.revision.saturating_add(1);
    for r in new.runs.values() {
        let previous = old.runs.get(&r.id);
        let old_status = previous.map(|p| p.status);
        if old_status == Some(r.status)
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
        let mut emit = |kind: &str, id: String, extra: Value| {
            let mut v = json!({
                "type": kind,
                "id": id,
                "at": now,
                "workflowRunId": r.id,
                "workflow": title,
                "runTitle": title,
                "parentWorkflowRunId": r.parent_run_id,
                "isSubagentChild": r.parent_run_id.is_some(),
            });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            new.milestones.push(v);
        };
        if previous.is_none() {
            emit(
                "workflow.run_started",
                format!("workflow.run_started:{}", r.id),
                json!({}),
            );
        }
        if old_status == Some(Status::Asking)
            && let Some(a) = previous.and_then(|v| v.attempt.as_ref())
            && (r.status != Status::Asking || r.attempt.as_ref().map(|x| x.id) != Some(a.id))
        {
            let receipt = r.receipts.iter().rfind(|rc| rc.attempt.id == a.id);
            let resolution = if receipt.is_some_and(|rc| rc.result["expired"] == true) {
                "expired"
            } else if receipt.is_some() {
                "answered"
            } else {
                "cancelled"
            };
            emit(
                "workflow.input_resolved",
                format!("workflow.input_resolved:{}:{}", r.id, a.id),
                json!({"step": a.id, "resolution": resolution}),
            );
        }
        if r.status == Status::Asking
            && let Some(a) = &r.attempt
            && (old_status != Some(Status::Asking)
                || previous.and_then(|p| p.attempt.as_ref()).map(|x| x.id) != Some(a.id))
            && let Some(Step::Ask { question, .. }) = r.steps.get(a.step)
        {
            emit(
                "workflow.needs_input",
                format!("workflow.needs_input:{}:{}", r.id, a.id),
                json!({"step": a.id, "question": question}),
            );
        }
        if matches!(r.status, Status::Failed | Status::Reconcile) {
            if !old_status.is_some_and(|s| matches!(s, Status::Failed | Status::Reconcile)) {
                emit(
                    "workflow.run_paused",
                    format!("workflow.run_paused:{}:{next_rev}", r.id),
                    json!({"reason": r.detail.clone().unwrap_or_else(|| "Step requires reconciliation".into())}),
                );
            }
        } else if old_status.is_some_and(|s| matches!(s, Status::Failed | Status::Reconcile))
            && !matches!(
                r.status,
                Status::Cancelled | Status::Stopped | Status::Skipped
            )
        {
            emit(
                "workflow.run_resumed",
                format!("workflow.run_resumed:{}:{next_rev}", r.id),
                json!({}),
            );
        }
        if let Some(outcome) = r.status.outcome_str()
            && !old_status.is_some_and(Status::is_terminal)
        {
            emit(
                "workflow.run_ended",
                format!("workflow.run_ended:{}", r.id),
                json!({"outcome": outcome, "detail": r.detail}),
            );
        }
    }
}
