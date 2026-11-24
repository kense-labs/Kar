//! Native question forms for a host UI, including MCP elicitation.
mod cursor;
mod factory;
use crate::PendingRequest;
#[cfg(not(target_arch = "wasm32"))]
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
#[cfg(not(target_arch = "wasm32"))]
use serde_json::json;
use serde_json::{Map, Value};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Editor,
    Select,
    MultiSelect,
    Boolean,
    Number,
    Integer,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionField {
    pub id: String,
    pub label: String,
    pub kind: FieldKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
    pub required: bool,
    pub allow_other: bool,
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Provider constraints, format, and defaults for an MCP field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuestionForm {
    pub fields: Vec<QuestionField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum QuestionReply {
    Submit { values: Map<String, Value> },
    Decline,
    Cancel,
}

impl PendingRequest {
    pub fn question_form(&self) -> Option<QuestionForm> {
        form(&self.method, &self.body)
    }
    /// Provider wording for a prompt or the action awaiting approval.
    pub fn display_text(&self) -> Option<String> {
        if let Some(form) = self.question_form() {
            return form.message.or_else(|| {
                (!form.fields.is_empty()).then(|| {
                    form.fields
                        .iter()
                        .map(|f| f.label.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            });
        }
        let field = match self.method.as_str() {
            "can_use_tool" => &self.body["tool_name"],
            "dsh/approval" => &self.body["toolName"],
            "kimi/approval" => &self.body["request"]["action"],
            "session/request_permission" => &self.body["toolCall"]["title"],
            "cursor/create_plan" => [
                &self.body["plan"],
                &self.body["overview"],
                &self.body["name"],
            ]
            .into_iter()
            .find(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))?,
            "item/commandExecution/requestApproval" => &self.body["command"],
            "mcpServer/elicitation/request" | "elicitation" | "elicitation/create" => {
                &self.body["message"]
            }
            _ => return None,
        };
        field
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    }
}
fn string(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().map(str::to_owned)
}
fn field(id: String, label: String, kind: FieldKind) -> QuestionField {
    QuestionField {
        id,
        label,
        kind,
        header: None,
        description: None,
        options: vec![],
        required: true,
        allow_other: false,
        secret: false,
        default: None,
        schema: None,
    }
}
fn options(entries: &Value, key: &str) -> Option<Vec<QuestionOption>> {
    entries
        .as_array()?
        .iter()
        .map(|v| {
            let value = v.as_str().or_else(|| v[key].as_str())?.to_owned();
            let label = v["title"].as_str().unwrap_or(&value).to_owned();
            Some(QuestionOption {
                value,
                label,
                description: string(v, "description"),
            })
        })
        .collect()
}
fn model_form(questions: &Value, claude: bool) -> Option<QuestionForm> {
    let mut fields = Vec::new();
    let mut ids = HashSet::new();
    let mut labels = HashSet::new();
    for (i, q) in questions.as_array()?.iter().enumerate() {
        let label = string(q, "question")?;
        let id = if claude {
            if !labels.insert(label.clone()) {
                return None;
            }
            i.to_string()
        } else {
            string(q, "id")?
        };
        if !ids.insert(id.clone()) {
            return None;
        }
        let choices = if q["options"].is_null() {
            vec![]
        } else {
            options(&q["options"], "label")?
        };
        let kind = if claude && q["multiSelect"] == true {
            FieldKind::MultiSelect
        } else if choices.is_empty() {
            FieldKind::Text
        } else {
            FieldKind::Select
        };
        let mut f = field(id, label, kind);
        f.options = choices;
        f.header = string(q, "header");
        f.secret = q["isSecret"] == true;
        f.allow_other = claude || q["isOther"] == true;
        fields.push(f);
    }
    (!fields.is_empty()).then_some(QuestionForm {
        fields,
        ..Default::default()
    })
}
fn mcp_form(body: &Value) -> Option<QuestionForm> {
    let message = string(body, "message");
    if body["mode"] == "url" {
        return Some(QuestionForm {
            message,
            url: Some(string(body, "url")?),
            fields: vec![],
        });
    }
    if body.get("mode").is_some_and(|mode| mode != "form") {
        return None;
    }
    let schema = body
        .get("requestedSchema")
        .or_else(|| body.get("requested_schema"))?;
    // Composite/root-dependent schemas need a vendor-specific form.
    if schema["type"] != "object"
        || [
            "oneOf",
            "anyOf",
            "allOf",
            "not",
            "if",
            "$ref",
            "dependentRequired",
            "dependentSchemas",
        ]
        .iter()
        .any(|key| schema.get(*key).is_some())
    {
        return None;
    }
    let required = schema["required"].as_array();
    let mut fields = vec![];
    for (key, prop) in schema["properties"].as_object()? {
        let mut kind = match prop["type"].as_str()? {
            "string" => FieldKind::Text,
            "integer" => FieldKind::Integer,
            "number" => FieldKind::Number,
            "boolean" => FieldKind::Boolean,
            "array" if prop["items"]["type"] == "string" || prop["items"]["anyOf"].is_array() => {
                FieldKind::MultiSelect
            }
            _ => return None,
        };
        let item = if kind == FieldKind::MultiSelect {
            &prop["items"]
        } else {
            prop
        };
        let mut choices = if let Some(v) = item.get("enum") {
            options(v, "const")?
        } else if let Some(v) = item.get("oneOf").or(item.get("anyOf")) {
            options(v, "const")?
        } else {
            vec![]
        };
        if let Some(names) = item["enumNames"].as_array() {
            for (choice, name) in choices.iter_mut().zip(names) {
                if let Some(s) = name.as_str() {
                    choice.label = s.into();
                }
            }
        }
        if !choices.is_empty() && kind == FieldKind::Text {
            kind = FieldKind::Select;
        }
        let mut f = field(
            key.to_owned(),
            string(prop, "title").unwrap_or_else(|| key.clone()),
            kind,
        );
        f.secret = prop["format"] == "password"
            || prop["writeOnly"] == true
            || crate::interaction::sensitive_field(key);
        f.default = prop.get("default").filter(|v| !v.is_null()).cloned();
        f.description = string(prop, "description");
        f.options = choices;
        f.required = required.is_some_and(|keys| keys.iter().any(|v| v.as_str() == Some(key)));
        f.schema = Some(prop.clone());
        fields.push(f);
    }
    Some(QuestionForm {
        message,
        fields,
        url: None,
    })
}
fn form(method: &str, b: &Value) -> Option<QuestionForm> {
    match method {
        "userInput/requested" => {
            let mut form = model_form(&b["questions"], false)?;
            for (field, q) in form.fields.iter_mut().zip(b["questions"].as_array()?) {
                field.allow_other = true;
                if q["selection"]["mode"] == "multiple" {
                    field.kind = FieldKind::MultiSelect;
                    field.schema = Some(serde_json::json!({"uniqueItems":true}));
                }
            }
            Some(form)
        }
        "cursor/ask_question" => cursor::questionnaire(b),
        "droid.ask_user" => factory::questionnaire(b),
        "droid.pick_options" => factory::picker(b),
        "droid.request_permission" => factory::spec_editor(b),
        "dsh/question" => {
            let questions = b["questions"].as_array()?;
            let mut form = model_form(&b["questions"], false)?;
            for (f, q) in form.fields.iter_mut().zip(questions) {
                f.description = string(q, "detail");
                f.allow_other = q["intent"]["kind"] != "approval";
                if q["multiSelect"] == true {
                    f.kind = FieldKind::MultiSelect;
                    f.schema = Some(serde_json::json!({"uniqueItems":true,"minItems":1}));
                }
            }
            Some(form)
        }
        "kimi/question" => {
            let mut fields = vec![];
            let mut ids = HashSet::new();
            for q in b.pointer("/request/questions")?.as_array()? {
                let id = string(q, "id")?;
                if !ids.insert(id.clone()) {
                    return None;
                }
                let multi = q["multi_select"] == true;
                let mut f = field(
                    id,
                    string(q, "question")?,
                    if multi {
                        FieldKind::MultiSelect
                    } else {
                        FieldKind::Select
                    },
                );
                f.header = string(q, "header");
                f.description = string(q, "body");
                f.allow_other = q["allow_other"] == true;
                f.required = false;
                if multi {
                    f.schema = Some(serde_json::json!({"uniqueItems":true}));
                }
                for o in q["options"].as_array()? {
                    f.options.push(QuestionOption {
                        value: string(o, "id")?,
                        label: string(o, "label")?,
                        description: string(o, "description"),
                    });
                }
                fields.push(f);
            }
            Some(QuestionForm {
                fields,
                ..Default::default()
            })
        }
        "item/tool/requestUserInput" => model_form(&b["questions"], false),
        "can_use_tool" if b["tool_name"] == "AskUserQuestion" => {
            model_form(&b["input"]["questions"], true)
        }
        "mcpServer/elicitation/request" | "elicitation" | "elicitation/create" => mcp_form(b),
        "input" | "editor" | "select" | "confirm" => {
            let kind = match method {
                "editor" => FieldKind::Editor,
                "select" => FieldKind::Select,
                "confirm" => FieldKind::Boolean,
                _ => FieldKind::Text,
            };
            let mut f = field("value".into(), string(b, "title")?, kind);
            f.description = string(b, "message").or_else(|| string(b, "placeholder"));
            f.default = b.get("prefill").filter(|v| v.is_string()).cloned();
            if method == "select" {
                f.options = options(&b["options"], "label")?;
            }
            Some(QuestionForm {
                fields: vec![f],
                ..Default::default()
            })
        }
        _ => None,
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn valid(field: &QuestionField, v: &Value) -> bool {
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
pub(crate) fn answer(method: &str, body: &Value, reply: QuestionReply) -> Result<Value> {
    if method == "cursor/ask_question" {
        match reply {
            QuestionReply::Decline => return Ok(json!({"outcome":{"outcome":"skipped"}})),
            QuestionReply::Cancel => return Ok(json!({"outcome":{"outcome":"cancelled"}})),
            QuestionReply::Submit { .. } => {}
        }
    }
    let form = form(method, body)
        .ok_or_else(|| Error::Unsupported("this request has no supported question form".into()))?;
    let mcp = matches!(
        method,
        "mcpServer/elicitation/request" | "elicitation" | "elicitation/create"
    );
    let QuestionReply::Submit { values } = reply else {
        return Ok(match method {
            "droid.ask_user" => json!({"cancelled":true,"answers":[]}),
            "droid.pick_options" => json!({"cancelled":true,"selectedIds":[]}),
            "droid.request_permission" => crate::interaction::decision_payload(
                method,
                body,
                crate::PermissionDecision::Cancel,
            )
            .ok_or_else(|| Error::Refused("Factory did not offer cancellation".into()))?,
            "kimi/question" => json!({"kind":"question","dismissed":true}),
            "item/tool/requestUserInput" => json!({"answers":{}}),
            "can_use_tool" => json!({"behavior":"deny","message":"User declined to answer"}),
            _ if mcp => json!({"action":if reply==QuestionReply::Cancel {"cancel"}else{"decline"}}),
            _ => json!({"cancelled":true}),
        });
    };
    if values
        .keys()
        .any(|key| !form.fields.iter().any(|f| f.id == *key))
    {
        return Err(Error::Refused("answer contains an unknown field".into()));
    }
    for f in &form.fields {
        if !values.get(&f.id).map_or(!f.required, |v| valid(f, v)) {
            return Err(Error::Refused(format!(
                "missing or invalid answer for field {}",
                f.id
            )));
        }
    }
    Ok(match method {
        "userInput/requested" => {
            let mut answers = Vec::new();
            for (field, q) in form.fields.iter().zip(body["questions"].as_array().expect("parsed Muse questions")) {
                let inputs = &values[&field.id];
                let inputs = inputs.as_array().map_or_else(|| std::slice::from_ref(inputs), Vec::as_slice);
                let (selected, other): (Vec<_>,Vec<_>) = inputs.iter().filter_map(Value::as_str)
                    .partition(|s|field.options.iter().any(|o|o.value == *s));
                let custom = other.join("\n");
                let n = selected.len() as u64;
                if custom.encode_utf16().count()>500 || (n==0 && custom.is_empty())
                    || (n!=0 && (q["selection"]["minSelections"].as_u64().is_some_and(|min|n<min)
                        || q["selection"]["maxSelections"].as_u64().is_some_and(|max|n>max))) {
                    return Err(Error::Refused("invalid Muse question selection or custom answer length".into()));
                }
                let mut value = json!({"questionId":field.id});
                if n>0 {
                    if q["selection"]["mode"]=="single" { value["selectedLabel"]=selected[0].into(); }
                    else { value["selectedLabels"]=json!(selected); }
                }
                if !custom.is_empty() { value[if n>0 {"note"} else {"freeText"}] = custom.into(); }
                answers.push(value);
            }
            json!({"answers":answers})
        }
        "cursor/ask_question" => cursor::answers(&form, &values),
        "droid.ask_user" => factory::answers(body, &values)?,
        "droid.request_permission" => {
            json!({"selectedOption":"proceed_edit","editedSpecContent":values["editedSpecContent"]})
        }
        "droid.pick_options" => {
            let ids = match values.get("selectedIds") {
                Some(Value::String(id)) => vec![Value::String(id.clone())],
                Some(Value::Array(ids)) => ids.clone(),
                _ => vec![],
            };
            json!({"cancelled":false,"selectedIds":ids})
        }
        "dsh/question" => {
            let mut answers = Vec::new();
            for f in form.fields {
                let value = &values[&f.id];
                let choices: Vec<_> = match value.as_array() {
                    Some(items) => items.iter().filter_map(Value::as_str).collect(),
                    None => value.as_str().into_iter().collect(),
                };
                let (selected, other): (Vec<_>, Vec<_>) = choices
                    .into_iter()
                    .partition(|s| f.options.iter().any(|o| o.value == *s));
                let mut entry = json!({"id":f.id,"selected":selected});
                match other.as_slice() {
                    [] => {}
                    [text] if !text.trim().is_empty() => entry["custom"] = (*text).into(),
                    _ => {
                        return Err(Error::Refused(
                            "DSH accepts one nonempty custom answer per question".into(),
                        ));
                    }
                }
                answers.push(entry);
            }
            json!({"answers":answers})
        }
        "kimi/question" => {
            let mut answers = Map::new();
            for f in form.fields {
                let choices: Vec<_> = values
                    .get(&f.id)
                    .into_iter()
                    .flat_map(|v| match v.as_array() {
                        Some(a) => a.iter().collect::<Vec<_>>(),
                        None => vec![v],
                    })
                    .filter_map(Value::as_str)
                    .collect();
                let (ids, other): (Vec<_>, Vec<_>) = choices
                    .into_iter()
                    .partition(|v| f.options.iter().any(|o| &o.value == v));
                let answer = match (ids.as_slice(), other.as_slice()) {
                    ([], []) => json!({"kind":"skipped"}),
                    ([], [text]) => json!({"kind":"other","text":text}),
                    ([_], []) if f.kind == FieldKind::Select => {
                        json!({"kind":"single","option_id":ids[0]})
                    }
                    (_, []) => json!({"kind":"multi","option_ids":ids}),
                    (_, [text]) => {
                        json!({"kind":"multi_with_other","option_ids":ids,"other_text":text})
                    }
                    _ => {
                        return Err(Error::Refused(
                            "Kimi accepts one free-text alternative per question".into(),
                        ));
                    }
                };
                answers.insert(f.id, answer);
            }
            json!({"kind":"question","answers":answers})
        }
        "item/tool/requestUserInput" => {
            json!({"answers":values.into_iter().map(|(k,v)|(k,json!({"answers":[v]}))).collect::<Map<_,_>>()})
        }
        "can_use_tool" => {
            let mut input = body["input"].clone();
            let mut answers = Map::new();
            for f in form.fields {
                let v = &values[&f.id];
                let text = match v.as_array() {
                    Some(a) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    None => v.as_str().unwrap_or("").to_owned(),
                };
                answers.insert(f.label, text.into());
            }
            input["answers"] = answers.into();
            json!({"behavior":"allow","updatedInput":input})
        }
        _ if mcp && form.url.is_some() => json!({"action":"accept"}),
        _ if mcp => json!({"action":"accept","content":values}),
        "confirm" => json!({"confirmed":values["value"]}),
        _ => json!({"value":values["value"]}),
    })
}

/// File references have already been resolved within the requesting chat. The actor
/// validates the answer against the live provider schema before it writes the native reply.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn answer_with_files(
    method: &str,
    body: &Value,
    reply: QuestionReply,
    files: std::collections::BTreeMap<String, String>,
) -> Result<Value> {
    if files.is_empty() {
        return answer(method, body, reply);
    }
    let QuestionReply::Submit { mut values } = reply else {
        return Err(Error::Refused(
            "attachments require a submitted answer".into(),
        ));
    };
    let form =
        form(method, body).ok_or_else(|| Error::Unsupported("request has no questions".into()))?;
    let mut choice_notes = Vec::new();
    for (id, note) in files {
        let field = form
            .fields
            .iter()
            .find(|f| f.id == id)
            .ok_or_else(|| Error::Refused("unknown attachment question".into()))?;
        if field.secret {
            return Err(Error::Refused(
                "attachments cannot be added to a secret answer".into(),
            ));
        }
        match field.kind {
            FieldKind::Text | FieldKind::Editor => {
                let value = values
                    .entry(&id)
                    .or_insert_with(|| Value::String(String::new()));
                let Value::String(s) = value else {
                    return Err(Error::Refused("attachments require a text answer".into()));
                };
                if !s.is_empty() {
                    s.push_str("\n\n");
                }
                s.push_str(&note);
            }
            FieldKind::Select | FieldKind::MultiSelect
                if matches!(method, "item/tool/requestUserInput" | "can_use_tool") =>
            {
                if values.get(&id).is_none() && field.allow_other {
                    values.insert(
                        id,
                        if field.kind == FieldKind::MultiSelect {
                            json!([note])
                        } else {
                            Value::String(note)
                        },
                    );
                } else {
                    choice_notes.push((id, field.label.clone(), note));
                }
            }
            _ => {
                return Err(Error::Refused(
                    "this question cannot carry file references".into(),
                ));
            }
        }
    }
    let mut payload = answer(method, body, QuestionReply::Submit { values })?;
    for (id, label, note) in choice_notes {
        if method == "item/tool/requestUserInput" {
            let notes = payload["answers"][id]["answers"]
                .as_array_mut()
                .ok_or_else(|| Error::Refused("expected a choice answer".into()))?;
            notes.push(Value::String(note));
        } else {
            let value = &mut payload["updatedInput"]["answers"][label];
            let Value::String(text) = value else {
                return Err(Error::Refused("expected a choice answer".into()));
            };
            text.push_str("\n\n");
            text.push_str(&note);
        }
    }
    Ok(payload)
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn redact_answer(method: &str, body: &Value, payload: &mut Value) {
    if method == "item/tool/requestUserInput" {
        for q in body["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|q| q["isSecret"] == true)
        {
            if let Some(values) = q["id"]
                .as_str()
                .and_then(|id| payload.get_mut("answers")?.get_mut(id)?.get_mut("answers"))
                .and_then(Value::as_array_mut)
            {
                for v in values {
                    *v = "[redacted]".into();
                }
            }
        }
    } else if matches!(
        method,
        "elicitation" | "elicitation/create" | "mcpServer/elicitation/request"
    ) {
        let props = body
            .get("requestedSchema")
            .or(body.get("requested_schema"))
            .and_then(|s| s["properties"].as_object());
        for (key, property) in props.into_iter().flatten() {
            if (property["format"] == "password"
                || property["writeOnly"] == true
                || crate::interaction::sensitive_field(key))
                && let Some(v) = payload
                    .get_mut("content")
                    .and_then(|content| content.get_mut(key))
            {
                *v = "[redacted]".into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn submit(v: Value) -> QuestionReply {
        QuestionReply::Submit {
            values: v.as_object().unwrap().clone(),
        }
    }
    #[test]
    fn dsh_questions_preserve_selected_labels_and_approval_constraints() {
        let method = "dsh/question";
        let fields = json!({"questions":[
            {"id":"scope","question":"Which files?","detail":"Select all that apply","multiSelect":true,"options":[{"label":"A"},{"label":"B"}]},
            {"id":"ok","question":"Proceed?","intent":{"kind":"approval"},"options":[{"label":"Yes"},{"label":"No"}]}
        ]});
        let form = form(method, &fields).unwrap();
        assert_eq!(
            form.fields[0].description.as_deref(),
            Some("Select all that apply")
        );
        assert!(!form.fields[1].allow_other);
        let data = |choices: Value, approval: &str| submit(json!({"scope":choices,"ok":approval}));
        assert_eq!(
            answer(method, &fields, data(json!(["B", "custom"]), "Yes")).unwrap(),
            json!({"answers":[{"id":"scope","selected":["B"],"custom":"custom"},{"id":"ok","selected":["Yes"]}]})
        );
        assert!(answer(method, &fields, data(json!(["A", "A"]), "Yes")).is_err());
        assert!(answer(method, &fields, data(json!(["B"]), "sometimes")).is_err());
        assert!(answer(method, &fields, data(json!(["one", "two"]), "No")).is_err());
        assert_eq!(
            answer(method, &fields, QuestionReply::Cancel).unwrap(),
            json!({"cancelled":true})
        );
    }
    #[test]
    fn kimi_forms_preserve_ids_labels_and_a_free_text_alternative() {
        let q = json!({"request":{"questions":[{"id":"files","question":"Which files?","multi_select":true,"allow_other":true,"options":[{"id":"op-a","label":"A"},{"id":"op-b","label":"B"}]}]}});
        let field = &form("kimi/question", &q).unwrap().fields[0];
        assert_eq!(field.options[0].label, "A");
        let values = serde_json::from_value(json!({"files":["op-b","C"]})).unwrap();
        assert_eq!(
            answer("kimi/question", &q, QuestionReply::Submit { values }).unwrap(),
            json!({"kind":"question","answers":{"files":{"kind":"multi_with_other","option_ids":["op-b"],"other_text":"C"}}})
        );
        assert_eq!(
            answer("kimi/question", &q, QuestionReply::Cancel).unwrap(),
            json!({"kind":"question","dismissed":true})
        );
    }
    #[test]
    fn mcp_choices_use_protocol_values_and_secret_fields_keep_their_schema() {
        let mut body = json!({"mode":"form","message":"Configure", "requestedSchema":{"type":"object","required":["mode","tags","password"],"properties":{
            "mode":{"type":"string","oneOf":[{"const":"fast","title":"Quick"}]},
            "tags":{"type":"array","minItems":1,"uniqueItems":true,"items":{"anyOf":[{"const":"r","title":"Read"}]}},
            "password":{"type":"string","format":"password","default":"private"},
            "limit":{"type":"integer","minimum":1,"maximum":5}
        }}});
        crate::transport::redact(&mut body, &[]);
        let method = "mcpServer/elicitation/request";
        let form = form(method, &body).unwrap();
        let mode = form.fields.iter().find(|f| f.id == "mode").unwrap();
        assert_eq!(
            (&*mode.options[0].label, &*mode.options[0].value),
            ("Quick", "fast")
        );
        let secret = form.fields.iter().find(|f| f.id == "password").unwrap();
        assert!(secret.secret && secret.default.is_none());
        let mut payload = answer(
            method,
            &body,
            submit(json!({"mode":"fast","tags":["r"],"password":"new"})),
        )
        .unwrap();
        assert_eq!(payload["content"]["password"], "new");
        redact_answer(method, &body, &mut payload);
        assert_eq!(payload["content"]["password"], "[redacted]");
        for v in [
            json!({"mode":"Quick","tags":["r"],"password":"new"}),
            json!({"mode":"fast","tags":[],"password":"new"}),
            json!({"mode":"fast","tags":["r","r"],"password":"new"}),
            json!({"mode":"fast","tags":["r"]}),
            json!({"mode":"fast","tags":["r"],"password":"new","limit":6}),
        ] {
            assert!(answer(method, &body, submit(v)).is_err());
        }
        let mut extended = body.clone();
        extended["requestedSchema"]["oneOf"] = json!([{"required":["mode"]}]);
        assert!(mcp_form(&extended).is_none());
        let mut extended = body.clone();
        extended["mode"] = json!("vendor");
        assert!(mcp_form(&extended).is_none());
        let url = json!({"mode":"url","url":"https://example.invalid/action","message":"Open"});
        assert_eq!(
            answer(method, &url, submit(json!({}))).unwrap(),
            json!({"action":"accept"})
        );
        assert_eq!(
            answer(method, &url, QuestionReply::Cancel).unwrap(),
            json!({"action":"cancel"})
        );
    }
}
