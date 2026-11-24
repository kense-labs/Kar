//! A matching JSON-RPC response is the admission boundary for a normal Codex prompt.
use crate::{Direction, FailureClass, RecordKind, stream::Journal};
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn refusal(native: &Value) -> Value {
    let message = native["message"]
        .as_str()
        .unwrap_or("Codex refused the request");
    let code = native["code"].as_i64();
    let data = &native["data"];
    let kind = if code == Some(-32600) && message.starts_with("thread not found: ") {
        "session_not_found"
    } else {
        "runtime_refused"
    };
    let failure = match code {
        Some(-32602) if data["input_error_code"] == "input_too_large" => {
            Some(FailureClass::InputTooLarge)
        }
        Some(-32001) => Some(FailureClass::Overloaded),
        Some(-32600)
            if message.starts_with("failed to load configuration: ")
                || data["type"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("modelProvider")) =>
        {
            Some(FailureClass::InvalidRequest)
        }
        _ => None,
    };
    let mut result = json!({"type":"rejected","code":kind,"reason":message,"native":native});
    if let Some(failure) = failure {
        result["failure"] = json!(failure);
    }
    result
}

pub(super) fn publish(journal: &Journal, request: Uuid, mut decision: Value, secrets: &[String]) {
    if let Some(turn) = decision.pointer_mut("/native/turn") {
        crate::input::omit_codex_turn_input(turn);
    }
    super::redact(&mut decision, secrets);
    decision["requestId"] = request.to_string().into();
    journal.push(
        Direction::Host,
        RecordKind::Response,
        None,
        vec![],
        decision,
        vec![],
    );
}
