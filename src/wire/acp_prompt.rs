use super::failure::AcpFailures;
use crate::{Error, Event, FailureClass, Outcome, Result, TurnFailure};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Steering {
    GrokInterject,
    OpenCodeExtension,
    ConcurrentPrompt,
    SupersedingPrompt,
}

impl Steering {
    pub fn negotiate(runtime: &str, opencode_v1: bool, init: &Value) -> Option<Self> {
        match runtime {
            "grok" => Some(Self::GrokInterject),
            "opencode" if opencode_v1 => Some(Self::ConcurrentPrompt),
            "opencode" if init.pointer("/_meta/steering/supported") == Some(&Value::Bool(true)) => {
                Some(Self::OpenCodeExtension)
            }
            "antigravity" => {
                let version = init.pointer("/agentInfo/version")?.as_str()?;
                let version = semver::Version::parse(version.trim_start_matches('v')).ok()?;
                (version.pre.is_empty() && version >= semver::Version::new(1, 5, 0))
                    .then_some(Self::SupersedingPrompt)
            }
            _ => None,
        }
    }
}

pub(crate) struct Completion {
    outcome: Outcome,
    failure: Option<TurnFailure>,
    error: Option<Error>,
}
impl Completion {
    pub fn from_reply(
        failures: &AcpFailures,
        result: &Result<Value>,
        packet: &Value,
        received_ms: u64,
        aborted: bool,
        opens_turn: bool,
    ) -> Self {
        let mut end = Self {
            outcome: Outcome::Failed,
            failure: None,
            error: None,
        };
        match result {
            Err(_) if aborted => end.outcome = Outcome::Aborted,
            Err(e) => {
                end.failure = Some(failures.rejected(&packet["error"], received_ms));
                // Failure after a replacement took over belongs in the original
                // TurnReport. Its opening prompt itself succeeded.
                if opens_turn {
                    end.error = Some(Error::Refused(e.to_string()));
                }
            }
            Ok(body) => {
                end.failure = failures.result_failure(body);
                if end.failure.is_none() {
                    match body["stopReason"].as_str() {
                        Some("cancelled") => end.outcome = Outcome::Aborted,
                        Some("refusal") => {
                            end.failure =
                                Some(TurnFailure::new(FailureClass::ModelRefused, "refusal"))
                        }
                        Some("end_turn" | "max_tokens" | "max_turn_requests") => {
                            end.outcome = Outcome::Completed
                        }
                        _ => {}
                    }
                }
            }
        }
        end
    }

    pub fn grok_output(&mut self, body: &Value, events: &mut Vec<Event>) {
        if self.outcome != Outcome::Completed {
            return;
        }
        if let Some(reason) = body
            .pointer("/_meta/structuredOutputError")
            .and_then(Value::as_str)
        {
            self.outcome = Outcome::Failed;
            self.failure = Some(TurnFailure::new(FailureClass::OutputSchema, reason));
        } else if let Some(value) = body.pointer("/_meta/structuredOutput") {
            events.push(Event::StructuredOutput {
                value: value.clone(),
            });
        }
    }

    pub fn publish(self, events: &mut Vec<Event>) -> Option<Error> {
        if let Some(failure) = self.failure {
            events.push(Event::TurnFailure { failure });
        }
        events.push(Event::TurnEnded {
            outcome: self.outcome,
        });
        self.error
    }
}

/// Antigravity can settle an older prompt immediately when a newer prompt takes over.
/// The last *submitted* non-refused prompt owns the outcome, regardless of reply order.
#[derive(Default)]
pub(crate) struct SupersedingPrompts {
    pending: BTreeSet<u64>,
    latest: Option<(u64, Completion)>,
}
impl SupersedingPrompts {
    pub fn sent(&mut self, id: u64) {
        self.pending.insert(id);
    }
    pub fn refused(&self, id: u64, code: Option<i64>) -> bool {
        code == Some(-32602) || self.pending.range(..id).next().is_some()
    }
    pub fn settle(&mut self, id: u64, end: Option<Completion>) -> Option<Completion> {
        if !self.pending.remove(&id) {
            return None;
        }
        if let Some(end) = end
            && self.latest.as_ref().is_none_or(|(last, _)| id > *last)
        {
            self.latest = Some((id, end));
        }
        if self.pending.is_empty() {
            return self.latest.take().map(|(_, end)| end);
        }
        None
    }
}
