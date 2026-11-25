//! The run reservation, optional coalescing deadline and replay key share one durable update.
use super::{
    Data, Request, Workflow,
    rules::{Resolver, Rules},
};
use crate::{
    Error, Result,
    workbench::workflows::{Status, Step, Trigger, insert_run},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(in crate::workbench::workflows) struct Window {
    replay: BTreeMap<String, (i64, Uuid)>,
    pending: BTreeMap<String, Uuid>,
    accepted: Vec<i64>,
    #[serde(default)]
    coalesced: Vec<i64>,
}
pub(super) struct Decision {
    pub outcome: &'static str,
    pub run: Option<Uuid>,
    pub error: Option<String>,
    pub missing: Vec<String>,
}
impl Decision {
    pub(super) fn new(outcome: &'static str, run: Option<Uuid>) -> Self {
        Self {
            outcome,
            run,
            error: None,
            missing: Vec::new(),
        }
    }
}
impl Request {
    pub(super) fn admit(
        &self,
        d: &mut Data,
        w: &Workflow,
        rules: &Rules,
        now: i64,
    ) -> Result<Decision> {
        let origin_ms = self.origin_ms(now);
        if rules
            .max_delivery_age_minutes
            .is_some_and(|min| now.saturating_sub(origin_ms) > i64::from(min) * 60_000)
        {
            return Ok(Decision::new("expired", None));
        }
        let resolve = Resolver::new(self, &rules.signature.header);
        if let Some(reason) = resolve.failed(&rules.filters) {
            let mut result = Decision::new("filtered", None);
            result.error = Some(reason);
            return Ok(result);
        }
        let replay = resolve.key(&rules.replay_key)?;
        let coalesce = resolve.key(&rules.coalesce_key)?;
        let (key, input) = self.input()?;
        let window = d.webhook_windows.entry(w.id).or_default();
        window.replay.retain(|_, (expires, _)| *expires > now);
        window.accepted.retain(|t| now.saturating_sub(*t) < 60_000);
        window.pending.retain(|_, id| {
            d.runs.get(id).is_some_and(|r| {
                r.status == Status::Ready && r.webhook_due_ms.is_some_and(|due| due > now)
            })
        });
        let trigger = if replay.is_some() || coalesce.is_some() {
            Trigger::Webhook(Uuid::new_v4().to_string())
        } else {
            Trigger::Webhook(key)
        };
        if let Some(id) = replay
            .as_ref()
            .and_then(|key| window.replay.get(key))
            .map(|r| r.1)
        {
            return Ok(Decision::new("duplicate", Some(id)));
        }
        if let Some(old) = d
            .runs
            .values()
            .find(|r| r.workflow == w.id && r.trigger == trigger)
        {
            if old.input != input {
                return Err(Error::Refused(
                    "delivery identifier reused with different input".into(),
                ));
            }
            return Ok(Decision::new("duplicate", Some(old.id)));
        }
        let owner = coalesce
            .as_ref()
            .and_then(|key| window.pending.get(key))
            .copied();
        window.coalesced.retain(|t| now.saturating_sub(*t) < 60_000);
        if owner.is_some() && window.coalesced.len() >= 60 {
            return Ok(Decision::new("rate_limited", None));
        }
        if owner.is_none() {
            let queued = d
                .runs
                .values()
                .filter(|r| r.workflow == w.id && r.status == Status::Ready)
                .count();
            if window.accepted.len() >= 60 {
                return Ok(Decision::new("rate_limited", None));
            }
            if queued >= 20 {
                return Ok(Decision::new("queue_full", None));
            }
        }
        let original = owner
            .and_then(|id| d.runs.get(&id))
            .and_then(|r| r.webhook_templates.clone())
            .unwrap_or_else(|| w.definition.steps.clone());
        let mut steps = original.clone();
        let mut missing = Vec::new();
        let frame = Uuid::new_v4();
        let render_agent_prompts = owner
            .and_then(|id| d.runs.get(&id))
            .map_or(rules.render_agent_prompts, |r| r.webhook_rendered);
        if render_agent_prompts {
            for step in &mut steps {
                if let Step::Agent { prompt, .. } = step {
                    let (value, absent) = resolve.render(prompt, Some(frame))?;
                    *prompt = value;
                    for key in absent {
                        if !missing.contains(&key) {
                            missing.push(key);
                        }
                    }
                }
            }
        }
        let (id, outcome) = if let Some(id) = owner {
            let run = d.runs.get_mut(&id).unwrap();
            run.steps = steps;
            run.input = input;
            d.webhook_windows
                .entry(w.id)
                .or_default()
                .coalesced
                .push(now);
            (id, "coalesced")
        } else {
            let id = insert_run(d, w, trigger, input, now)?.id;
            let run = d.runs.get_mut(&id).unwrap();
            run.steps = steps;
            run.webhook_rendered = render_agent_prompts;
            if coalesce.is_some() {
                run.webhook_templates = Some(original);
                run.webhook_due_ms = Some(now.saturating_add(
                    i64::from(rules.coalesce_window_seconds.unwrap_or(120)) * 1000,
                ));
            }
            d.webhook_windows
                .entry(w.id)
                .or_default()
                .accepted
                .push(now);
            (id, "accepted")
        };
        let window = d.webhook_windows.entry(w.id).or_default();
        if let Some(key) = coalesce {
            window.pending.insert(key, id);
        }
        if let Some(key) = replay {
            window
                .replay
                .insert(key, (now.saturating_add(7 * 86_400_000), id));
        }
        Ok(Decision {
            outcome,
            run: Some(id),
            error: None,
            missing,
        })
    }
}
