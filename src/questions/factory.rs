use super::*;
use serde_json::json;

pub(super) fn spec_editor(body: &Value) -> Option<QuestionForm> {
    let mut edits = body["options"]
        .as_array()?
        .iter()
        .filter(|v| v["value"] == "proceed_edit");
    let label = string(edits.next()?, "label")?;
    if edits.next().is_some() {
        return None;
    }
    let mut specs = body["toolUses"]
        .as_array()?
        .iter()
        .map(|t| &t["details"])
        .filter(|d| d["type"] == "exit_spec_mode");
    let spec = specs.next()?;
    if specs.next().is_some() {
        return None;
    }
    let mut input = field("editedSpecContent".into(), label, FieldKind::Editor);
    input.default = Some(string(spec, "plan")?.into());
    Some(QuestionForm {
        fields: vec![input],
        message: string(spec, "title"),
        url: None,
    })
}

pub(super) fn questionnaire(body: &Value) -> Option<QuestionForm> {
    let mut fields = Vec::new();
    let mut indices = HashSet::new();
    for q in body["questions"].as_array()? {
        let index = q["index"].as_u64()?;
        if !indices.insert(index) {
            return None;
        }
        let multi = q["multiSelect"] == true;
        let mut f = field(
            index.to_string(),
            string(q, "question")?,
            if multi {
                FieldKind::MultiSelect
            } else {
                FieldKind::Select
            },
        );
        f.header = string(q, "topic");
        f.description = string(q, "context");
        f.allow_other = true;
        f.options = q["options"]
            .as_array()?
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let text = option.as_str()?.to_owned();
                Some(QuestionOption {
                    value: text.clone(),
                    label: text,
                    description: q["optionDescriptions"]
                        .get(i)
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            })
            .collect::<Option<_>>()?;
        if multi {
            f.schema = Some(json!({"uniqueItems":true,"minItems":1}));
        }
        fields.push(f);
    }
    (!fields.is_empty()).then_some(QuestionForm {
        fields,
        ..Default::default()
    })
}

pub(super) fn picker(body: &Value) -> Option<QuestionForm> {
    let multi = body["multi"].as_bool()?;
    let mut f = field(
        "selectedIds".into(),
        string(body, "title")?,
        if multi {
            FieldKind::MultiSelect
        } else {
            FieldKind::Select
        },
    );
    let mut ids = HashSet::new();
    for row in body["rows"].as_array()? {
        let id = row["id"].as_str()?;
        if !ids.insert(id) {
            return None;
        }
        f.options.push(QuestionOption {
            value: id.into(),
            label: string(row, "label")?,
            description: string(row, "detail"),
        });
    }
    let selected: Vec<_> = body["preselectedIds"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| ids.contains(id))
        .collect();
    f.default = if multi {
        Some(json!(selected))
    } else {
        selected.first().map(|id| json!(id))
    };
    f.description = string(body, "hint");
    f.required = false;
    if multi {
        f.schema = Some(json!({"uniqueItems":true,"maxItems":ids.len()}));
    }
    Some(QuestionForm {
        fields: vec![f],
        ..Default::default()
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn answers(body: &Value, values: &Map<String, Value>) -> Result<Value> {
    let mut answers = Vec::new();
    for q in body["questions"].as_array().into_iter().flatten() {
        let value = &values[&q["index"].to_string()];
        let choices: Vec<_> = match value.as_array() {
            Some(items) => items.iter().filter_map(Value::as_str).collect(),
            None => value.as_str().into_iter().collect(),
        };
        if choices.iter().any(|s| s.trim().is_empty()) {
            return Err(Error::Refused("empty answer to a Factory question".into()));
        }
        answers
            .push(json!({"index":q["index"],"question":q["question"],"answer":choices.join(", ")}));
    }
    Ok(json!({"cancelled":false,"answers":answers}))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    #[test]
    fn edited_spec_requires_an_offered_option_and_an_unambiguous_plan() {
        let method = "droid.request_permission";
        let mut body = json!({"options":[{"value":"proceed_edit","label":"Edit proposal"}],
            "toolUses":[{"details":{"type":"exit_spec_mode","title":"A plan","plan":"Original text"}}]});
        let form = spec_editor(&body).unwrap();
        assert_eq!(form.fields[0].default, Some(json!("Original text")));
        let submit = QuestionReply::Submit {
            values: json!({"editedSpecContent":"Revised plan"})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert_eq!(
            super::super::answer(method, &body, submit.clone()).unwrap(),
            json!({"selectedOption":"proceed_edit","editedSpecContent":"Revised plan"})
        );
        assert!(super::super::answer(method, &body, QuestionReply::Cancel).is_err());
        body["options"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value":"cancel"}));
        assert_eq!(
            super::super::answer(method, &body, QuestionReply::Cancel).unwrap(),
            json!({"selectedOption":"cancel"})
        );
        let duplicate = body["toolUses"][0].clone();
        body["toolUses"].as_array_mut().unwrap().push(duplicate);
        assert!(super::super::answer(method, &body, submit).is_err());
    }
}
