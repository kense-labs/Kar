use super::{TurnFailure, prose};
use crate::{Event, Outcome};
use serde_json::Value;

/// Pi Durable can already have switched to a new run when the prior inputs settle.
pub(crate) fn submission_end(receipts: &[Value]) -> Vec<Event> {
    if let Some(receipt) = receipts.iter().find(|r| r["status"] == "unanswered") {
        if receipt["reason"] == "aborted" {
            return vec![Event::TurnEnded {
                outcome: Outcome::Aborted,
            }];
        }
        let reason = receipt["detail"]
            .as_str()
            .or_else(|| receipt["reason"].as_str())
            .unwrap_or("Pi Durable input was unanswered");
        return vec![
            Event::TurnFailure {
                failure: TurnFailure::new(prose(reason), reason),
            },
            Event::TurnEnded {
                outcome: Outcome::Failed,
            },
        ];
    }
    if !receipts.is_empty() && receipts.iter().all(|r| r["status"] == "done") {
        vec![Event::TurnEnded {
            outcome: Outcome::Completed,
        }]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FailureClass;
    use serde_json::json;

    #[test]
    fn receipts_prove_completion_and_own_the_failure_detail() {
        let done = json!({"status":"done"});
        assert!(submission_end(&[done.clone(), Value::Null]).is_empty());
        assert_eq!(
            submission_end(std::slice::from_ref(&done)),
            [Event::TurnEnded {
                outcome: Outcome::Completed
            }]
        );
        assert_eq!(
            submission_end(&[json!({"status":"unanswered","reason":"aborted","detail":"401"})]),
            [Event::TurnEnded {
                outcome: Outcome::Aborted
            }]
        );
        for (reason, detail, class) in [
            ("auth", "401 Unauthorized", FailureClass::Auth),
            (
                "provider_error",
                "Internal error: This model's maximum context length is 1,024 tokens. However, you requested 2,048 tokens",
                FailureClass::InputTooLarge,
            ),
            ("unknown", "other detail", FailureClass::Unknown),
        ] {
            let events = submission_end(&[
                done.clone(),
                json!({"status":"unanswered","reason":reason,"detail":detail}),
            ]);
            assert!(
                matches!(events.as_slice(),[Event::TurnFailure {failure}, Event::TurnEnded {outcome:Outcome::Failed}] if failure.class == class && failure.reason == detail && failure.status.is_none())
            );
        }
        assert!(
            matches!(submission_end(&[json!({"status":"unanswered","reason":"usage limit reached"})]).first(),
            Some(Event::TurnFailure {failure}) if failure.class == FailureClass::Quota)
        );
    }
}
