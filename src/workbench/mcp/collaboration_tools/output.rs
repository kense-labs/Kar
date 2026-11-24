use super::*;
use crate::Event;
use std::{
    fs::{self, File},
    io,
};

fn validator(schema: &Value) -> Result<jsonschema::Validator> {
    jsonschema::validator_for(schema)
        .map_err(|e| Error::Refused(format!("invalid requested output schema: {e}")))
}
impl Workbench {
    pub(in crate::workbench) fn register_run_schema(
        &self,
        project: Uuid,
        chat: Uuid,
        turn: Uuid,
        schema: Value,
    ) -> Result<()> {
        self.binding(project, chat)?;
        if !schema.is_object() || serde_json::to_string(&schema)?.chars().count() > 32_000 {
            return Err(Error::Refused("invalid requested output schema".into()));
        }
        validator(&schema)?;
        let file = self.0.chats.run_schema_path(chat, turn);
        let dir = file.parent().unwrap();
        fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut tmp, &schema)?;
        tmp.as_file().sync_all()?;
        match tmp.persist_noclobber(&file) {
            Ok(_) => {
                #[cfg(unix)]
                File::open(dir)?.sync_all()?;
                Ok(())
            }
            Err(e)
                if e.error.kind() == io::ErrorKind::AlreadyExists
                    && self.0.chats.run_schema(chat, turn)?.as_ref() == Some(&schema) =>
            {
                Ok(())
            }
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Err(Error::Refused(
                "a different output schema was already requested for this turn".into(),
            )),
            Err(e) => Err(e.error.into()),
        }
    }
}
pub(super) fn call(app: &Workbench, grant: &Grant, name: &str, input: Value) -> Result<Value> {
    let control = app
        .active_control(grant.project, grant.chat)?
        .ok_or_else(denied)?;
    let Some(turn) = control.model_caller_turn() else {
        return Err(Error::Refused("this chat has no active turn".into()));
    };
    let (event, result) = if name == "kar_send_to_user" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Message {
            message: String,
        }
        let m: Message = serde_json::from_value(input)?;
        if m.message.trim().is_empty() || m.message.len() > 32000 {
            return Err(Error::Refused("invalid Markdown message".into()));
        }
        (
            Event::Message {
                text: m.message.clone(),
            },
            json!({"sent_message":m.message,"turn":turn}),
        )
    } else {
        let schema = app
            .0
            .chats
            .run_schema(grant.chat, turn)?
            .or_else(|| {
                app.0
                    .chats
                    .saved_info(grant.chat)
                    .ok()?
                    .settings
                    .output_schema
                    .map(Value::Object)
            })
            .ok_or_else(|| {
                Error::Refused("no structured output requested for the current turn".into())
            })?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Output {
            json: String,
        }
        let m: Output = serde_json::from_value(input)?;
        if m.json.len() > 200000 {
            return Err(Error::Refused(
                "output exceeds the requested size limit".into(),
            ));
        }
        let value: Value = serde_json::from_str(&m.json)?;
        if let Err(e) = validator(&schema)?.validate(&value) {
            return Err(Error::Refused(format!(
                "output does not fit schema: {}",
                e.to_string().chars().take(2000).collect::<String>()
            )));
        }
        (Event::StructuredOutput { value }, json!({"turn":turn}))
    };
    control.publish_model_tool_result(turn, name, event)?;
    Ok(result)
}
