use super::seconds_ms;
use crate::Event;
use serde_json::Value;

/// Account notifications carry no thread ID. Once a child has been seen, an
/// app-server's later account snapshots can no longer be assigned to its root.
#[derive(Default)]
pub(crate) struct CodexQuota {
    shared: bool,
    turn: Option<String>,
    terminal_error: bool,
    reset: Option<u64>,
}

impl CodexQuota {
    pub(crate) fn observe(&mut self, root: Option<&str>, packet: &Value, events: &mut [Event]) {
        let (Some(root), Some(method)) = (root, packet["method"].as_str()) else {
            return;
        };
        let p = &packet["params"];
        let thread = p["threadId"]
            .as_str()
            .or_else(|| p["thread"]["id"].as_str());
        let item = p["item"]["type"].as_str();
        if thread.is_some_and(|id| id != root)
            || matches!(
                item,
                Some("collabAgentToolCall" | "collabToolCall" | "subAgentActivity")
            )
        {
            self.shared = true;
            self.reset = None;
        }
        if self.shared {
            return;
        }
        let id = p["turn"]["id"].as_str();
        match method {
            "turn/started" => {
                *self = Self {
                    turn: id.map(str::to_owned),
                    ..Self::default()
                }
            }
            "turn/completed" => {
                if thread == Some(root)
                    && id.is_some()
                    && id == self.turn.as_deref()
                    && p["turn"]["error"]["codexErrorInfo"] == "usageLimitExceeded"
                    && let Some(Event::TurnFailure { failure }) = events
                        .iter_mut()
                        .find(|e| matches!(e, Event::TurnFailure { .. }))
                {
                    failure.resets_at_ms = self.reset;
                }
                *self = Self::default();
            }
            "item/started" if item == Some("contextCompaction") => self.reset = None,
            "error"
                if self.turn.is_some()
                    && self.turn.as_deref() == p["turnId"].as_str()
                    && p["willRetry"] == false =>
            {
                self.terminal_error = true
            }
            "account/rateLimits/updated" if self.turn.is_some() && !self.terminal_error => {
                self.reset = exhausted_reset(&p["rateLimits"]);
            }
            _ => {}
        }
    }
}

fn exhausted_reset(limits: &Value) -> Option<u64> {
    if limits["limitId"] != "codex" {
        return None;
    }
    let mut windows = [&limits["primary"], &limits["secondary"]]
        .into_iter()
        .filter(|w| w["usedPercent"].as_f64().is_some_and(|p| p >= 100.0))
        .map(|w| seconds_ms(&w["resetsAt"]));
    let first = windows.next()??;
    windows.all(|reset| reset == Some(first)).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn notice(method: &str, params: Value) -> Value {
        json!({"method":method, "params":params})
    }
    fn start(id: &str) -> Value {
        notice("turn/started", json!({"threadId":"root","turn":{"id":id}}))
    }
    fn limits(windows: [Value; 2]) -> Value {
        notice(
            "account/rateLimits/updated",
            json!({"rateLimits":{"limitId":"codex","primary":windows[0],"secondary":windows[1]}}),
        )
    }
    fn reset(notices: &[Value], id: &str, code: &str) -> Option<u64> {
        let mut state = CodexQuota::default();
        for n in notices {
            state.observe(Some("root"), n, &mut []);
        }
        let e = json!({"codexErrorInfo":code});
        let finish = notice(
            "turn/completed",
            json!({"threadId":"root","turn":{"id":id,"error":e}}),
        );
        let mut events = [Event::TurnFailure {
            failure: super::super::codex(&e),
        }];
        state.observe(Some("root"), &finish, &mut events);
        let [Event::TurnFailure { failure }] = events else {
            unreachable!()
        };
        failure.resets_at_ms
    }

    #[test]
    fn only_current_unambiguous_windows_supply_a_reset() {
        let full = json!({"usedPercent":100,"resetsAt":1900000000});
        let update = limits([Value::Null, full.clone()]);
        let quota = "usageLimitExceeded";
        let current = [start("t"), update.clone()];
        assert_eq!(reset(&current, "t", quota), Some(1_900_000_000_000));
        assert_eq!(reset(&current, "other", quota), None);
        assert_eq!(reset(&current, "t", "rateLimitExceeded"), None);
        let both = limits([full.clone(), full.clone()]);
        assert_eq!(
            reset(&[start("t"), both], "t", quota),
            Some(1_900_000_000_000)
        );
        for suffix in [
            start("next"),
            limits([json!({"usedPercent":100,"resetsAt":17}), full.clone()]),
            limits([json!({"usedPercent":100}), full]),
            limits([json!({"usedPercent":99,"resetsAt":17}), Value::Null]),
            notice(
                "account/rateLimits/updated",
                json!({"rateLimits":{"limitId":"other","primary":{"usedPercent":100,"resetsAt":17}}}),
            ),
            notice(
                "item/started",
                json!({"threadId":"root","item":{"type":"contextCompaction"}}),
            ),
            notice(
                "item/started",
                json!({"threadId":"root","item":{"type":"collabAgentToolCall"}}),
            ),
            notice("thread/started", json!({"thread":{"id":"child"}})),
        ] {
            assert_eq!(
                reset(&[start("t"), update.clone(), suffix], "t", quota),
                None
            );
        }
        let child = notice("thread/started", json!({"thread":{"id":"child"}}));
        assert_eq!(
            reset(&[child, start("t"), update.clone()], "t", quota),
            None
        );
        assert_eq!(reset(&[update, start("t")], "t", quota), None);
    }
}
