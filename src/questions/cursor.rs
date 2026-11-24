use super::*;

pub(super) fn questionnaire(body: &Value) -> Option<QuestionForm> {
    let mut fields = Vec::new();
    let mut question_ids = HashSet::new();
    for question in body["questions"].as_array()? {
        let id = question["id"].as_str()?;
        if id.is_empty() || !question_ids.insert(id) {
            return None;
        }
        let kind = match question.get("allowMultiple") {
            None | Some(Value::Bool(false)) => FieldKind::Select,
            Some(Value::Bool(true)) => FieldKind::MultiSelect,
            _ => return None,
        };
        let mut f = field(id.into(), string(question, "prompt")?, kind);
        let mut option_ids = HashSet::new();
        for option in question["options"].as_array()? {
            let id = option["id"].as_str()?;
            if id.is_empty() || !option_ids.insert(id) {
                return None;
            }
            f.options.push(QuestionOption {
                value: id.into(),
                label: string(option, "label")?,
                description: None,
            });
        }
        if f.options.is_empty() {
            return None;
        }
        if f.kind == FieldKind::MultiSelect {
            f.schema = Some(serde_json::json!({"minItems":1,"uniqueItems":true}));
        }
        fields.push(f);
    }
    (!fields.is_empty()).then(|| QuestionForm {
        fields,
        message: string(body, "title").filter(|s| !s.trim().is_empty()),
        url: None,
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn answers(form: &QuestionForm, values: &Map<String, Value>) -> Value {
    let answers: Vec<_> = form
        .fields
        .iter()
        .map(|f| {
            let value = &values[&f.id];
            let ids = value
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(value));
            json!({"questionId":f.id,"selectedOptionIds":ids})
        })
        .collect();
    json!({"outcome":{"outcome":"answered","answers":answers}})
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_forms_can_still_be_dismissed() {
        let question = json!({"id":"q","prompt":"Choose","options":[{"id":"a","label":"A"}]});
        let mut duplicate = question.clone();
        duplicate["options"] = json!([{"id":"a","label":"A"},{"id":"a","label":"B"}]);
        let mut empty = question.clone();
        empty["options"] = json!([]);
        for body in [
            json!({"questions":[question,question]}),
            json!({"questions":[duplicate]}),
            json!({"questions":[empty]}),
            json!({"questions":[]}),
        ] {
            assert!(questionnaire(&body).is_none());
            assert!(matches!(
                answer(
                    "cursor/ask_question",
                    &body,
                    QuestionReply::Submit { values: Map::new() }
                ),
                Err(Error::Unsupported(_))
            ));
            assert_eq!(
                answer("cursor/ask_question", &body, QuestionReply::Cancel).unwrap(),
                json!({"outcome":{"outcome":"cancelled"}})
            );
        }
    }
}
