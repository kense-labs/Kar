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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestDeliveryStatus {
    Failed,
    Unconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestDeliveryError {
    pub cursor: Cursor,
    pub request_id: Value,
    pub method: String,
    pub request_kind: RequestKind,
    pub status: RequestDeliveryStatus,
    pub reason: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrphanedQuestion {
    pub cursor: Cursor,
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<uuid::Uuid>,
    pub form: crate::QuestionForm,
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

    /// Converts an ordinary root-agent user question into a restart-recoverable record.
    ///
    /// Permissions, subagent prompts, MCP/URL/secret elicitations, and Pi UI widgets are excluded.
    pub fn recoverable_question(
        &self,
        root_session: Option<&str>,
        turn_id: Option<uuid::Uuid>,
    ) -> Option<OrphanedQuestion> {
        if self.kind != RequestKind::Question || !self.agent_path.is_empty() {
            return None;
        }
        if let (Some(req_session), Some(root)) = (self.session_id.as_deref(), root_session)
            && req_session != root
        {
            return None;
        }
        let allowed = match self.method.as_str() {
            "item/tool/requestUserInput"
            | "cursor/ask_question"
            | "droid.ask_user"
            | "droid.pick_options"
            | "kimi/question" => true,
            "can_use_tool" => {
                request_body(&self.method, &self.body)["tool_name"].as_str()
                    == Some("AskUserQuestion")
                    || self.body["tool_name"].as_str() == Some("AskUserQuestion")
            }
            "userInput/requested" => self.body["subagentOrigin"].is_null(),
            "dsh/question" => self.body["questions"].as_array().is_some_and(|qs| {
                !qs.iter()
                    .any(|q| q["intent"]["kind"].as_str() == Some("approval"))
            }),
            _ => false,
        };
        if !allowed {
            return None;
        }
        let form = self.question_form()?;
        if form.url.is_some() || form.fields.is_empty() || form.fields.iter().any(|f| f.secret) {
            return None;
        }
        Some(OrphanedQuestion {
            cursor: self.cursor.clone(),
            id: self.id.clone(),
            method: self.method.clone(),
            turn_id,
            form,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn valid_question_field(field: &crate::QuestionField, v: &Value) -> bool {
    use crate::FieldKind;
    use std::collections::HashSet;

    let schema = field.schema.as_ref().unwrap_or(&Value::Null);
    let choice = |v: &Value| {
        v.as_str()
            .is_some_and(|s| field.allow_other || field.options.iter().any(|o| o.value == s))
    };
    let between = |n: u64, min: &str, max: &str| {
        schema[min].as_u64().is_none_or(|m| n >= m) && schema[max].as_u64().is_none_or(|m| n <= m)
    };
    match field.kind {
        FieldKind::Select => choice(v),
        FieldKind::Text | FieldKind::Editor => v
            .as_str()
            .is_some_and(|s| between(s.chars().count() as u64, "minLength", "maxLength")),
        FieldKind::MultiSelect => v.as_array().is_some_and(|a| {
            between(a.len() as u64, "minItems", "maxItems")
                && (schema["uniqueItems"] != true || {
                    let mut seen = HashSet::new();
                    a.iter().all(|v| seen.insert(v.as_str()))
                })
                && a.iter()
                    .all(|v| (field.options.is_empty() && v.is_string()) || choice(v))
        }),
        FieldKind::Boolean => v.is_boolean(),
        FieldKind::Integer | FieldKind::Number => v.as_f64().is_some_and(|n| {
            (field.kind != FieldKind::Integer || n.fract() == 0.0)
                && schema["minimum"].as_f64().is_none_or(|m| n >= m)
                && schema["maximum"].as_f64().is_none_or(|m| n <= m)
        }),
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl OrphanedQuestion {
    pub(crate) fn validate_and_format(
        &self,
        values: &serde_json::Map<String, Value>,
        files: &std::collections::BTreeMap<String, String>,
    ) -> crate::Result<String> {
        use crate::{Error, FieldKind};

        let mut values = values.clone();
        let mut choice_notes = std::collections::BTreeMap::new();
        for (id, note) in files {
            let field = self
                .form
                .fields
                .iter()
                .find(|f| f.id == *id)
                .ok_or_else(|| Error::Refused("unknown attachment question".into()))?;
            if field.secret {
                return Err(Error::Refused(
                    "attachments cannot be added to a secret answer".into(),
                ));
            }
            match field.kind {
                FieldKind::Text | FieldKind::Editor => {
                    let value = values
                        .entry(id.clone())
                        .or_insert_with(|| Value::String(String::new()));
                    let Value::String(s) = value else {
                        return Err(Error::Refused("attachments require a text answer".into()));
                    };
                    if !s.is_empty() {
                        s.push_str("\n\n");
                    }
                    s.push_str(note);
                }
                FieldKind::Select | FieldKind::MultiSelect
                    if matches!(
                        self.method.as_str(),
                        "item/tool/requestUserInput" | "can_use_tool"
                    ) =>
                {
                    if values.get(id).is_none() && field.allow_other {
                        values.insert(
                            id.clone(),
                            if field.kind == FieldKind::MultiSelect {
                                serde_json::json!([note])
                            } else {
                                Value::String(note.clone())
                            },
                        );
                    } else {
                        choice_notes.insert(id.clone(), note.clone());
                    }
                }
                _ => {
                    return Err(Error::Refused(
                        "this question cannot carry file references".into(),
                    ));
                }
            }
        }
        if values
            .keys()
            .any(|key| !self.form.fields.iter().any(|f| f.id == *key))
        {
            return Err(Error::Refused("answer contains an unknown field".into()));
        }
        for f in &self.form.fields {
            if !values
                .get(&f.id)
                .map_or(!f.required, |v| valid_question_field(f, v))
            {
                return Err(Error::Refused(format!(
                    "missing or invalid answer for field {}",
                    f.id
                )));
            }
        }
        match self.method.as_str() {
            "userInput/requested" => {
                for field in &self.form.fields {
                    let Some(inputs) = values.get(&field.id) else {
                        continue;
                    };
                    let inputs = inputs
                        .as_array()
                        .map_or_else(|| std::slice::from_ref(inputs), Vec::as_slice);
                    let (selected, other): (Vec<_>, Vec<_>) = inputs
                        .iter()
                        .filter_map(Value::as_str)
                        .partition(|s| field.options.iter().any(|o| o.value == *s));
                    let custom = other.join("\n");
                    if custom.encode_utf16().count() > 500
                        || (selected.is_empty() && custom.is_empty())
                    {
                        return Err(Error::Refused(
                            "invalid Muse question selection or custom answer length".into(),
                        ));
                    }
                }
            }
            "dsh/question" => {
                for field in &self.form.fields {
                    let Some(value) = values.get(&field.id) else {
                        continue;
                    };
                    let choices: Vec<_> = match value.as_array() {
                        Some(items) => items.iter().filter_map(Value::as_str).collect(),
                        None => value.as_str().into_iter().collect(),
                    };
                    let other: Vec<_> = choices
                        .into_iter()
                        .filter(|s| !field.options.iter().any(|o| o.value == *s))
                        .collect();
                    match other.as_slice() {
                        [] => {}
                        [text] if !text.trim().is_empty() => {}
                        _ => {
                            return Err(Error::Refused(
                                "DSH accepts one nonempty custom answer per question".into(),
                            ));
                        }
                    }
                }
            }
            "kimi/question" => {
                for field in &self.form.fields {
                    let choices: Vec<_> = values
                        .get(&field.id)
                        .into_iter()
                        .flat_map(|v| match v.as_array() {
                            Some(a) => a.iter().collect::<Vec<_>>(),
                            None => vec![v],
                        })
                        .filter_map(Value::as_str)
                        .collect();
                    let other: Vec<_> = choices
                        .into_iter()
                        .filter(|v| !field.options.iter().any(|o| &o.value == v))
                        .collect();
                    if other.len() > 1 {
                        return Err(Error::Refused(
                            "Kimi accepts one free-text alternative per question".into(),
                        ));
                    }
                }
            }
            _ => {}
        }

        let mut sections = Vec::new();
        if let Some(msg) = self
            .form
            .message
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            sections.push(msg.trim().to_owned());
        }
        for field in &self.form.fields {
            let rendered = match values.get(&field.id) {
                Some(v) => format_field_answer(field, v),
                None => "(skipped)".to_owned(),
            };
            let mut answer = if rendered.trim().is_empty() {
                "(empty)".to_owned()
            } else {
                rendered
            };
            if let Some(note) = choice_notes.get(&field.id) {
                answer.push_str("\n\n");
                answer.push_str(note);
            }
            sections.push(format!("Question: {}\nAnswer: {answer}", field.label));
        }
        Ok(sections.join("\n\n"))
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn format_field_answer(field: &crate::QuestionField, value: &Value) -> String {
    let render_one = |v: &Value| match v {
        Value::String(s) => match field.options.iter().find(|o| o.value == *s) {
            Some(opt) if opt.label != opt.value => format!("{} ({})", opt.label, opt.value),
            Some(opt) => opt.label.clone(),
            None => s.clone(),
        },
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    };
    match value {
        Value::Array(items) => items.iter().map(render_one).collect::<Vec<_>>().join(", "),
        other => render_one(other),
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
            crate::Event::RequestDeliveryFailed { cursor, id, .. } => {
                let old = pending.len();
                pending.retain(|p| !(&p.id == id && &p.cursor == cursor));
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
        "approval/requested"
        | "approval/updated"
        | "droid.request_permission"
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
        "userInput/requested"
        | "cursor/ask_question"
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
        "approval/requested" | "approval/updated" => {
            let choices = body["availableChoices"].as_array()?;
            let matches = |c: &Value| match decision {
                AllowOnce => c["decision"] == "approved",
                AllowForSession => {
                    c["decision"] == "approvedForSession"
                        || (c["decision"] == "approvedPolicyAmendment" && c["scope"] == "session")
                }
                AllowAlways => {
                    c["decision"] == "approvedPolicyAmendment" && c["scope"] == "localPersistent"
                }
                Deny => c["decision"] == "denied" || c["decision"] == "deniedPolicyAmendment",
                Cancel => c["decision"] == "abort",
                _ => false,
            };
            let choice = choices.iter().find(|c| matches(c)).or_else(|| {
                (decision == Deny)
                    .then(|| choices.iter().find(|c| c["decision"] == "abort"))
                    .flatten()
            })?;
            Some(json!({"choiceId":choice["choiceId"].as_str()?}))
        }
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

    #[test]
    fn recoverable_question_strictly_filters_ordinary_root_questions() {
        let cursor = Cursor {
            stream_id: uuid::Uuid::nil(),
            after_seq: 7,
        };
        let turn = uuid::Uuid::from_u128(42);
        let make = |method: &str, body: Value| PendingRequest {
            cursor: cursor.clone(),
            id: json!(1),
            method: method.into(),
            kind: classify(method, &body),
            session_id: Some("root-s".into()),
            agent_path: vec![],
            body,
        };

        let codex = make(
            "item/tool/requestUserInput",
            json!({"questions":[{"id":"q1","question":"Database?","options":[{"label":"SQLite"},{"label":"Postgres"}]}]}),
        );
        let recovered = codex
            .recoverable_question(Some("root-s"), Some(turn))
            .expect("codex root question is recoverable");
        assert_eq!(recovered.turn_id, Some(turn));
        assert_eq!(recovered.form.fields[0].id, "q1");

        // Mismatched root session or non-empty agent_path is excluded.
        assert!(
            codex
                .recoverable_question(Some("other"), Some(turn))
                .is_none()
        );
        let mut subagent = codex.clone();
        subagent.agent_path = vec!["child".into()];
        assert!(
            subagent
                .recoverable_question(Some("root-s"), Some(turn))
                .is_none()
        );

        // Claude AskUserQuestion allowed, Claude permission excluded.
        let claude_q = make(
            "can_use_tool",
            json!({"tool_name":"AskUserQuestion","input":{"questions":[{"question":"Target?","options":[{"label":"dev"}]}]}}),
        );
        assert!(
            claude_q
                .recoverable_question(Some("root-s"), Some(turn))
                .is_some()
        );
        let claude_perm = make("can_use_tool", json!({"tool_name":"Bash","input":{}}));
        assert!(
            claude_perm
                .recoverable_question(Some("root-s"), Some(turn))
                .is_none()
        );

        // Cursor, Factory, Kimi, Copilot (only without subagentOrigin), DSH (only without approval intent).
        assert!(
            make(
                "cursor/ask_question",
                json!({"questions":[{"id":"q","prompt":"Pick","options":[{"id":"a","label":"Alpha"}]}]}),
            )
            .recoverable_question(Some("root-s"), Some(turn))
            .is_some()
        );
        assert!(
            make(
                "userInput/requested",
                json!({"questions":[{"id":"q","question":"Pick","options":["A"]}],"subagentOrigin":{"id":"sub"}}),
            )
            .recoverable_question(Some("root-s"), Some(turn))
            .is_none()
        );
        assert!(
            make(
                "dsh/question",
                json!({"questions":[{"id":"q","question":"Approve?","intent":{"kind":"approval"},"options":["yes"]}]}),
            )
            .recoverable_question(Some("root-s"), Some(turn))
            .is_none()
        );

        // Permissions, MCP elicitations, Pi prompts, and secret questions are excluded.
        for (method, body) in [
            (
                "droid.request_permission",
                json!({"specContent":"plan","options":[{"value":"proceed_edit"}]}),
            ),
            (
                "elicitation/create",
                json!({"message":"Name?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}}}}),
            ),
            (
                "mcpServer/elicitation/request",
                json!({"mode":"url","url":"https://example.com"}),
            ),
            ("select", json!({"title":"Pick","options":["a"]})),
            ("confirm", json!({"title":"Proceed?"})),
            ("input", json!({"title":"Enter text"})),
            ("editor", json!({"title":"Edit"})),
            (
                "item/tool/requestUserInput",
                json!({"questions":[{"id":"tok","question":"API token?","isSecret":true}]}),
            ),
        ] {
            assert!(
                make(method, body)
                    .recoverable_question(Some("root-s"), Some(turn))
                    .is_none(),
                "{method} must not be recoverable"
            );
        }
    }
}
