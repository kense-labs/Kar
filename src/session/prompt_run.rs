use super::{PromptDecision, Session, TurnReport};
use crate::{ControlReceipt, Error, InputImage, InputReceipt, Record, Result};
use std::{future::pending, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct PromptRunOptions {
    pub input_id: Option<Uuid>,
    pub images: Vec<InputImage>,
    pub origin: Option<crate::InputOrigin>,
    /// Measured from admission. After an accepted interrupt, wait for the native terminal event.
    pub timeout: Option<Duration>,
    pub signal: Option<CancellationToken>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptInterrupt {
    Timeout,
    Signal,
}

#[derive(Debug)]
pub enum PromptRun {
    Rejected {
        receipt: ControlReceipt,
        error: Error,
    },
    Handled {
        receipt: ControlReceipt,
        disposition: Arc<Record>,
    },
    Ended {
        receipt: ControlReceipt,
        report: TurnReport,
    },
    Interrupted {
        by: PromptInterrupt,
        receipt: ControlReceipt,
        report: TurnReport,
    },
}

impl Session {
    /// Submit one prompt, observe its exact attempt, and interrupt it if a caller limit fires.
    /// A previously cancelled signal still allows native handled inputs to finish admission.
    pub async fn prompt_and_wait(
        &self,
        text: impl Into<String>,
        options: PromptRunOptions,
    ) -> Result<PromptRun> {
        let turn = match self
            .submit_prompt_with_options(
                text,
                super::InputOptions {
                    input_id: options.input_id,
                    images: options.images,
                    origin: options.origin,
                },
            )
            .await?
        {
            PromptDecision::Rejected { receipt, error } => {
                return Ok(PromptRun::Rejected { receipt, error });
            }
            PromptDecision::Accepted(turn) => turn,
        };
        let receipt = turn
            .admission()
            .cloned()
            .ok_or_else(|| Error::Protocol("accepted prompt has no admission record".into()))?;
        let request = receipt
            .request
            .control_request_id()
            .ok_or_else(|| Error::Protocol("prompt admission has no request identity".into()))?
            .to_owned();
        let input = turn.input_id();
        if let Some(disposition) = self.journal.handled_prompt(&request, input) {
            return Ok(PromptRun::Handled {
                receipt,
                disposition,
            });
        }
        let ended = turn.report();
        tokio::pin!(ended);
        let (report, interrupted) = tokio::select! {
            biased;
            report = &mut ended => (report?, None),
            by = limit(options.signal, options.timeout) => {
                if let Some(disposition) = self.journal.handled_prompt(&request, input) {
                    return Ok(PromptRun::Handled { receipt, disposition });
                }
                let interrupted = match self.abort_prompt(Some(request.clone())).await {
                    Ok(()) => Some(by),
                    Err(Error::Refused(_) | Error::Unsupported(_) | Error::Busy) => None,
                    Err(error) => return Err(error),
                };
                (ended.await?, interrupted)
            }
        };
        if report.native_input_receipt == Some(InputReceipt::Handled) {
            let disposition = self
                .journal
                .handled_prompt(&request, input)
                .ok_or_else(|| {
                    Error::Protocol("native handled input has no matching receipt frame".into())
                })?;
            Ok(PromptRun::Handled {
                receipt,
                disposition,
            })
        } else if let Some(by) = interrupted {
            Ok(PromptRun::Interrupted {
                by,
                receipt,
                report,
            })
        } else {
            Ok(PromptRun::Ended { receipt, report })
        }
    }
}

async fn limit(signal: Option<CancellationToken>, timeout: Option<Duration>) -> PromptInterrupt {
    let cancelled = async {
        match signal {
            Some(signal) => signal.cancelled().await,
            None => pending().await,
        }
    };
    let elapsed = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => pending().await,
        }
    };
    tokio::select! {
        biased;
        () = cancelled => PromptInterrupt::Signal,
        () = elapsed => PromptInterrupt::Timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Direction, Event, Outcome, Protocol, RecordKind, SessionOptions,
        stream::{Journal, RecordStamp},
        transport::Command,
    };
    use serde_json::Value;

    fn session() -> (Session, Journal, tokio::sync::mpsc::Receiver<Command>) {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("owner");
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        (
            Session::remote(
                Protocol::Codex,
                SessionOptions::new("."),
                tx,
                journal.clone(),
                "owner".into(),
                Value::Null,
            ),
            journal,
            rx,
        )
    }

    fn dispatch(journal: &Journal, input: Uuid, request: Uuid) {
        journal.push(
            RecordStamp::from(Direction::ToRuntime)
                .with_prompt_request(input, false, request, None),
            RecordKind::Request,
            Some("owner".into()),
            vec![],
            Value::Null,
            vec![Event::PromptDispatched],
        );
        journal.control_decision(request, None);
    }

    fn report(outcome: Outcome) -> TurnReport {
        TurnReport {
            native_input_receipt: None,
            outcome,
            text: "partial".into(),
            structured_output: None,
            failure: None,
        }
    }

    #[tokio::test]
    async fn caller_limit_waits_for_the_native_result_and_classifies_only_accepted_interrupts() {
        for by in [PromptInterrupt::Signal, PromptInterrupt::Timeout] {
            let (session, journal, mut rx) = session();
            let signal = CancellationToken::new();
            let mut options = PromptRunOptions::default();
            match by {
                PromptInterrupt::Signal => {
                    signal.cancel();
                    options.signal = Some(signal);
                }
                PromptInterrupt::Timeout => options.timeout = Some(Duration::ZERO),
            }
            let run = tokio::spawn(async move { session.prompt_and_wait("send", options).await });
            let Some(Command::Prompt(input, message, _, ack, done)) = rx.recv().await else {
                panic!("expected prompt");
            };
            dispatch(&journal, input, message.request_id);
            ack.send(Ok(())).unwrap();
            let Some(Command::Abort(abort)) = rx.recv().await else {
                panic!("expected interrupt");
            };
            assert_eq!(abort.target_request, Some(message.request_id.to_string()));
            let ack = abort.for_current(&journal).unwrap();
            let outcome = match by {
                PromptInterrupt::Signal => {
                    ack.send(Ok(Value::Null)).unwrap();
                    Outcome::Aborted
                }
                PromptInterrupt::Timeout => {
                    ack.send(Err(Error::Refused("native turn ended".into())))
                        .unwrap();
                    Outcome::Completed
                }
            };
            assert!(!run.is_finished());
            done.send(Ok(report(outcome.clone()))).unwrap();
            let (observed, result) = match run.await.unwrap().unwrap() {
                PromptRun::Interrupted { by, report, .. } => (Some(by), report),
                PromptRun::Ended { report, .. } => (None, report),
                other => panic!("unexpected result {other:?}"),
            };
            assert_eq!(observed, (by == PromptInterrupt::Signal).then_some(by));
            assert_eq!(result.outcome, outcome);
            assert_eq!(result.text, "partial");
        }
    }

    #[tokio::test]
    async fn turn_target_is_rechecked_even_when_the_next_attempt_reuses_the_input_id() {
        let (session, journal, mut rx) = session();
        let (input, first, next) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        dispatch(&journal, input, first);
        assert!(matches!(
            session.abort_turn(input, Some(next)).await,
            Err(Error::Refused(_))
        ));
        let interrupt = tokio::spawn(async move { session.abort_turn(input, None).await });
        let Some(Command::Abort(abort)) = rx.recv().await else {
            panic!("expected scoped abort");
        };
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            Some("owner".into()),
            vec![],
            Value::Null,
            vec![Event::TurnEnded {
                outcome: Outcome::Completed,
            }],
        );
        dispatch(&journal, input, next);
        assert!(abort.for_current(&journal).is_none());
        assert!(
            matches!(interrupt.await.unwrap(), Err(Error::Refused(reason)) if reason == "prompt no longer owns the active turn")
        );
        assert_eq!(
            journal.active_prompt(),
            Some((input.to_string(), next.to_string()))
        );
    }
    #[tokio::test]
    async fn stale_steer_returns_one_local_refusal_and_never_targets_the_next_attempt() {
        let (session, journal, mut rx) = session();
        let (owner, initial, next, input) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        dispatch(&journal, owner, initial);
        let sending = tokio::spawn(async move {
            session
                .steer_turn(owner, None, input, "follow up", vec![])
                .await
        });
        let Some(Command::Steer(id, message, target, ack)) = rx.recv().await else {
            panic!("expected targeted steer")
        };
        let request = message.request_id;
        assert_eq!(id, input);
        assert_eq!(target.unwrap().request_id, Some(initial));
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            Some("owner".into()),
            vec![],
            Value::Null,
            vec![Event::TurnEnded {
                outcome: Outcome::Completed,
            }],
        );
        dispatch(&journal, owner, next);
        assert!(
            journal
                .admit_steer(id, &message, target, true, &[], ack)
                .is_none()
        );
        assert!(
            matches!(sending.await.unwrap(), Err(Error::Refused(ref message)) if message == "prompt no longer owns the active turn")
        );
        let refusal = journal.control_receipt(&request.to_string()).unwrap();
        assert_eq!(
            refusal.request.native["targetRequestId"],
            initial.to_string()
        );
        assert_eq!(refusal.request.native["inputId"], input.to_string());
        assert_eq!(refusal.response.native["code"], "no_active_turn");
        assert_eq!(
            journal.active_prompt(),
            Some((owner.to_string(), next.to_string()))
        );
    }
}
