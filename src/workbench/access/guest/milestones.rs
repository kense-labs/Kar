use super::*;
use crate::workbench::mcp::milestones::Log;
impl super::super::AccessStore {
    pub(in crate::workbench) fn milestone_log(&self) -> Log {
        self.registry.lock().unwrap().milestones.clone()
    }
}
pub(in crate::workbench::access) fn record(old: &Registry, new: &mut Registry) {
    let now = now();
    for (id, share) in &new.shares {
        let prev = old.shares.get(id);
        let mut event = |kind: &str, msg: Uuid, actor: Uuid, at: u64, resolution: Option<&str>| {
            let who = if resolution.is_some() {
                json!({"type":"system"})
            } else {
                json!({"type":"guest","id":actor})
            };
            let mut v = json!({"type":kind,"id":format!("guest:{id}:{msg}:{kind}"),"at":at,"projectId":share.project,"threadId":share.chat,"actor":who,"shareId":id,"guestMessageId":msg});
            if let Some(r) = resolution {
                v["resolution"] = json!(r);
            }
            new.milestones.push(v);
        };
        for (msg, p) in &share.proposals {
            let previous = prev.and_then(|s| s.proposals.get(msg));
            if previous.is_none() && prev.is_none_or(|s| !s.retired.contains_key(msg)) {
                event("thread.guest_request", *msg, p.author, p.created_ms, None);
            }
            if !previous
                .is_some_and(|p| matches!(p.status, Decision::Accepted | Decision::Dismissed))
            {
                let resolution = match p.status {
                    Decision::Accepted => Some("sent"),
                    Decision::Dismissed if share.revoked || share.expires_ms <= now => {
                        Some("closed")
                    }
                    Decision::Dismissed => Some("dismissed"),
                    _ => None,
                };
                if let Some(r) = resolution {
                    event(
                        "thread.guest_request_resolved",
                        *msg,
                        p.author,
                        now,
                        Some(r),
                    );
                }
            }
        }
        if let Some(prev) = prev {
            for (msg, p) in &prev.proposals {
                if matches!(p.status, Decision::Pending | Decision::Sending)
                    && !share.proposals.contains_key(msg)
                    && let Some(r) = share.retired.get(msg)
                {
                    let resolution = if r.status == Decision::Accepted {
                        "sent"
                    } else if share.revoked || share.expires_ms <= now {
                        "closed"
                    } else {
                        "dismissed"
                    };
                    event(
                        "thread.guest_request_resolved",
                        *msg,
                        p.author,
                        now,
                        Some(resolution),
                    );
                }
            }
        }
    }
}
