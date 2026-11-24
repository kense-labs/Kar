use crate::{Cursor, Record};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Permission,
    Question,
    Service,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    AllowOnce,
    AllowForTurn,
    AllowForSession,
    AllowAlways,
    Deny,
    DenyAlways,
    Cancel,
}

/// An outstanding request on this stream. The cursor identifies this occurrence even if the native id is reused.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PendingRequest {
    pub cursor: Cursor,
    pub id: Value,
    pub method: String,
    pub kind: RequestKind,
    pub session_id: Option<String>,
    pub agent_path: Vec<String>,
    pub body: Value,
}
/// Computed from the displayed native request for Rust and wire clients alike.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RequestControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form: Option<crate::QuestionForm>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<PermissionDecision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<crate::PolicyOption>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<crate::PermissionResource>,
}
impl Serialize for PendingRequest {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("PendingRequest", 8)?;
        s.serialize_field("cursor", &self.cursor)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("method", &self.method)?;
        s.serialize_field("kind", &self.kind)?;
        s.serialize_field("session_id", &self.session_id)?;
        s.serialize_field("agent_path", &self.agent_path)?;
        s.serialize_field("body", &self.body)?;
        s.serialize_field("controls", &self.controls())?;
        s.end()
    }
}
pub(crate) fn request_body<'a>(method: &str, native: &'a Value) -> &'a Value {
    if native["type"] == "control_request" {
        &native["request"]
    } else if native["method"] == method {
        native.get("params").unwrap_or(native)
    } else {
        native
    }
}
impl PendingRequest {
    pub(crate) fn from_record(r: &Record, id: &Value, method: &str) -> Self {
        let body = request_body(method, &r.native);
        Self {
            cursor: r.cursor(),
            id: id.clone(),
            method: method.into(),
            kind: classify(method, body),
            session_id: r.session_id.clone(),
            agent_path: r.agent_path.clone(),
            body: body.clone(),
        }
    }
    pub fn controls(&self) -> RequestControls {
        RequestControls {
            form: self.question_form(),
            decisions: self.decisions(),
            policies: self.policy_options(),
            resources: self.permission_resources(),
        }
    }
    /// Decisions with an unambiguous native scope on this particular request.
    pub fn decisions(&self) -> Vec<PermissionDecision> {
        [
            PermissionDecision::AllowOnce,
            PermissionDecision::Deny,
            PermissionDecision::AllowForTurn,
            PermissionDecision::AllowForSession,
            PermissionDecision::AllowAlways,
            PermissionDecision::DenyAlways,
            PermissionDecision::Cancel,
        ]
        .into_iter()
        .filter(|d| decision_payload(&self.method, &self.body, *d).is_some())
        .collect()
    }
}
pub(crate) fn update_pending(pending: &mut Vec<PendingRequest>, r: &Record, root: bool) -> bool {
    let mut changed = false;
    for e in &r.events {
        match e {
            crate::Event::ApprovalRequested { id, method } => {
                if pending
                    .iter()
                    .any(|p| &p.id == id && p.cursor == r.cursor())
                {
                    continue;
                }
                pending.retain(|p| &p.id != id);
                pending.push(PendingRequest::from_record(r, id, method));
                changed = true;
            }
            crate::Event::RequestResolved { id } => {
                let old = pending.len();
                pending.retain(|p| &p.id != id);
                changed |= old != pending.len();
            }
            crate::Event::Released | crate::Event::Exited { .. } if root && !pending.is_empty() => {
                pending.clear();
                changed = true;
            }
            _ => {}
        }
    }
    changed
}
pub(crate) fn sensitive_field(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "authorization"
            | "apikey"
            | "api_key"
            | "accesstoken"
            | "access_token"
            | "refreshtoken"
            | "refresh_token"
            | "password"
            | "id_token"
    )
}

fn classify(method: &str, body: &Value) -> RequestKind {
    match method {
        "can_use_tool" if body["tool_name"] == "AskUserQuestion" => RequestKind::Question,
        "droid.request_permission"
        | "cursor/create_plan"
        | "session/request_permission"
        | "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval"
        | "execCommandApproval"
        | "applyPatchApproval"
        | "can_use_tool"
        | "approval"
        | "morph/approval"
        | "dsh/approval"
        | "kimi/approval" => RequestKind::Permission,
        "cursor/ask_question"
        | "droid.ask_user"
        | "droid.pick_options"
        | "dsh/question"
        | "kimi/question"
        | "item/tool/requestUserInput"
        | "mcpServer/elicitation/request"
        | "elicitation"
        | "elicitation/create"
        | "select"
        | "confirm"
        | "input"
        | "editor" => RequestKind::Question,
        "item/tool/call"
        | "terminal/create"
        | "terminal/output"
        | "terminal/wait_for_exit"
        | "terminal/kill"
        | "terminal/release"
        | "fs/read_text_file"
        | "fs/write_text_file" => RequestKind::Service,
        _ => RequestKind::Unknown,
    }
}
pub(crate) fn decision_payload(
    method: &str,
    body: &Value,
    decision: PermissionDecision,
) -> Option<Value> {
    use PermissionDecision::*;
    use serde_json::json;
    match method {
        "cursor/create_plan" => Some(json!({"outcome":{"outcome":match decision {
            AllowOnce => "accepted",
            Deny => "rejected",
            Cancel => "cancelled",
            _ => return None,
        }}})),
        "droid.request_permission" => {
            let option = match decision {
                AllowOnce => "proceed_once",
                AllowAlways => "proceed_always",
                Cancel => "cancel",
                _ => return None,
            };
            body["options"]
                .as_array()?
                .iter()
                .any(|v| v["value"] == option)
                .then(|| json!({"selectedOption":option}))
        }
        "dsh/approval" => Some(
            match decision {
                AllowOnce => "allowed-once",
                Deny => "rejected",
                Cancel => "cancelled",
                _ => return None,
            }
            .into(),
        ),
        "kimi/approval" => Some(
            serde_json::json!({"kind":"approval","decision":match decision {AllowOnce=>"approved",Deny=>"rejected",Cancel=>"cancelled",_=>return None}}),
        ),
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let value = match decision {
                AllowOnce => "accept",
                AllowForSession => "acceptForSession",
                Deny => "decline",
                Cancel => "cancel",
                _ => return None,
            };
            if let Some(advertised) = body.get("availableDecisions").filter(|v| !v.is_null())
                && !advertised
                    .as_array()?
                    .iter()
                    .any(|v| v.as_str() == Some(value))
            {
                return None;
            }
            Some(json!({"decision":value}))
        }
        "execCommandApproval" | "applyPatchApproval" => {
            let value = match decision {
                AllowOnce => "approved",
                AllowForSession => "approved_for_session",
                Deny => "denied",
                Cancel => "abort",
                _ => return None,
            };
            Some(json!({"decision":value}))
        }
        "session/request_permission" => {
            if decision == Cancel {
                return Some(json!({"outcome":{"outcome":"cancelled"}}));
            }
            let kind = match decision {
                AllowOnce => "allow_once",
                AllowAlways => "allow_always",
                Deny => "reject_once",
                DenyAlways => "reject_always",
                _ => return None,
            };
            let mut choices = body["options"]
                .as_array()?
                .iter()
                .filter(|option| option["kind"] == kind);
            let first = choices.next();
            // Distinct native choices with the same duration must be selected by native optionId.
            if choices.next().is_some() {
                return None;
            }
            match first {
                Some(option) => Some(
                    json!({"outcome":{"outcome":"selected","optionId":option["optionId"].as_str()?}}),
                ),
                None if decision == Deny => Some(json!({"outcome":{"outcome":"cancelled"}})),
                None => None,
            }
        }
        "can_use_tool" if classify(method, body) == RequestKind::Permission => match decision {
            AllowOnce => {
                Some(json!({"behavior":"allow","updatedInput":body.get("input")?.as_object()?}))
            }
            Deny => Some(json!({"behavior":"deny","message":"Denied by host"})),
            _ => None,
        },
        "item/permissions/requestApproval" => match decision {
            Deny => Some(json!({"permissions":{}})),
            AllowForTurn | AllowForSession => {
                let permissions = body["permissions"].as_object()?;
                if permissions.is_empty()
                    || permissions
                        .keys()
                        .any(|k| !matches!(k.as_str(), "fileSystem" | "network"))
                {
                    return None;
                }
                Some(
                    json!({"permissions":permissions,"scope":if decision == AllowForTurn {"turn"} else {"session"}}),
                )
            }
            _ => None,
        },
        "morph/approval" | "approval" => match decision {
            AllowOnce => Some(json!({"decision":"approve"})),
            Deny => Some(json!({"decision":"deny"})),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_grants_keep_lifetime_and_requested_resources() {
        let acp = json!({"options":[{"kind":"allow_once","optionId":"one"},{"kind":"allow_always","optionId":"many"},{"kind":"reject_always","optionId":"never"}]});
        let select = |choice| {
            decision_payload("session/request_permission", &acp,choice).unwrap()["outcome"]["optionId"].clone()
        };
        assert_eq!(select(PermissionDecision::AllowOnce), "one");
        assert_eq!(select(PermissionDecision::AllowAlways), "many");
        assert_eq!(select(PermissionDecision::DenyAlways), "never");
        let ambiguous = json!({"options":[{"kind":"allow_always","optionId":"workspace"},{"kind":"allow_always","optionId":"global"}]});
        assert!(
            decision_payload(
                "session/request_permission",
                &ambiguous,
                PermissionDecision::AllowAlways
            )
            .is_none()
        );
        let requested = json!({"permissions":{"fileSystem":{"read":["/test/project"]}}});
        for (choice, scope) in [
            (PermissionDecision::AllowForTurn, "turn"),
            (PermissionDecision::AllowForSession, "session"),
        ] {
            assert_eq!(
                decision_payload("item/permissions/requestApproval", &requested, choice),
                Some(json!({"permissions":requested["permissions"],"scope":scope}))
            );
        }
        let restricted = json!({"availableDecisions":["cancel","acceptForSession"]});
        assert_eq!(
            decision_payload(
                "item/commandExecution/requestApproval",
                &restricted,
                PermissionDecision::AllowForSession
            ),
            Some(json!({"decision":"acceptForSession"}))
        );
        assert!(
            decision_payload(
                "item/commandExecution/requestApproval",
                &restricted,
                PermissionDecision::Deny
            )
            .is_none()
        );
        assert!(
            decision_payload(
                "can_use_tool",
                &json!({"input":{}}),
                PermissionDecision::AllowAlways
            )
            .is_none()
        );
    }
    #[test]
    fn one_time_choice_never_expands_to_a_persistent_grant() {
        let permanent = json!({"options":[{"kind":"allow_always","optionId":"always"}]});
        assert!(
            decision_payload(
                "session/request_permission",
                &permanent,
                PermissionDecision::AllowOnce
            )
            .is_none()
        );
        assert_eq!(
            decision_payload(
                "session/request_permission",
                &permanent,
                PermissionDecision::Deny
            ),
            Some(json!({"outcome":{"outcome":"cancelled"}}))
        );
        let restricted = json!({"availableDecisions":["cancel","acceptForSession"]});
        assert!(
            decision_payload(
                "item/commandExecution/requestApproval",
                &restricted,
                PermissionDecision::AllowOnce
            )
            .is_none()
        );
        let question = json!({"tool_name":"AskUserQuestion","input":{}});
        assert!(
            decision_payload("can_use_tool", &question, PermissionDecision::AllowOnce).is_none()
        );
    }
}
