use crate::{
    Direction, Error, ImageSubmission, Protocol,
    stream::{Journal, RecordStamp},
    transport::{Command, redact},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

impl Command {
    pub(crate) fn reject_unreachable(
        &self,
        journal: &Journal,
        protocol: Protocol,
        secrets: &[String],
    ) -> Option<Error> {
        journal.reject_unreachable(|| {
            let mut stamp = RecordStamp::from(Direction::ToRuntime);
            let (request, mut body) = match self {
                Self::Prompt(id, input, ..)
                | Self::Queue(id, input, ..)
                | Self::Steer(id, input, ..) => {
                    let request = input.request_id;
                    let submission = ImageSubmission::loaded(&input.text, &input.images, secrets);
                    let mut body = json!({"id":request, "inputId":id, "input":input.text});
                    if let Some(submission) = &submission {
                        body["images"] = json!(submission.images);
                    }
                    match self {
                        Self::Prompt(..) => {
                            body["control"] = "prompt".into();
                            stamp = stamp.with_prompt_request(
                                id,
                                protocol == Protocol::Pi,
                                request,
                                submission,
                            );
                        }
                        Self::Queue(..) => body["type"] = "queue".into(),
                        _ => body["type"] = "steer".into(),
                    }
                    (request, body)
                }
                Self::Withdraw(id, _) => (Uuid::new_v4(), json!({"type":"withdraw","inputId":id})),
                Self::Abort(_) => (Uuid::new_v4(), json!({"type":"abort"})),
                Self::StopTask(id, _) => (Uuid::new_v4(), json!({"type":"stop_task","taskId":id})),
                Self::Compact(request, ..) => (*request, json!({"type":"compact"})),
                Self::Rpc(method, params, ..)
                    if matches!(
                        method.as_str(),
                        "goals/get"
                            | "goals/set"
                            | "goals/clear"
                            | "thread/goal/get"
                            | "thread/goal/set"
                            | "thread/goal/clear"
                    ) =>
                {
                    (Uuid::new_v4(), json!({"type":method,"params":params}))
                }
                Self::Close(_) | Self::Send(..) | Self::Respond(..) | Self::Rpc(..) => return None,
            };
            body["id"] = Value::String(request.to_string());
            redact(&mut body, secrets);
            Some((stamp, body, request))
        })
    }
}

pub(crate) async fn reject_pending(
    inbox: &mut mpsc::Receiver<Command>,
    journal: &Journal,
    protocol: Protocol,
    secrets: &[String],
) {
    inbox.close();
    while let Some(command) = inbox.recv().await {
        command.reject_unreachable(journal, protocol, secrets);
    }
}
