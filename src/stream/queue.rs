use super::{Direction, Event, Journal, RecordKind};
use crate::{Error, Result, TurnReport, input::Input, transport::redact};
use serde_json::{Value, json};
use std::collections::VecDeque;
use tokio::sync::oneshot;
use uuid::Uuid;

impl Journal {
    pub(crate) fn queue_request(&self, id: Uuid, input: &Input, secrets: &[String]) {
        let mut body = json!({"type":"queue","id":input.request_id,"inputId":id,"text":input.text});
        redact(&mut body, secrets);
        self.push(
            Direction::ToRuntime,
            RecordKind::Request,
            None,
            vec![],
            body,
            vec![],
        );
    }

    pub(crate) fn queue_accepted(
        &self,
        request: Uuid,
        id: Uuid,
        adapter: bool,
        native: Option<Value>,
    ) {
        let mut response = json!({"type":"accepted","requestId":request});
        if adapter {
            response["queued"] = "adapter".into();
        }
        if let Some(native) = native {
            response["native"] = native;
        }
        self.push(
            Direction::Host,
            RecordKind::Response,
            None,
            vec![],
            response,
            vec![Event::InputQueued {
                input_id: id.to_string(),
            }],
        );
    }

    pub(crate) fn withdraw_request(&self, id: Uuid) -> Uuid {
        self.control_request(json!({"type":"withdraw","inputId":id}))
    }

    pub(crate) fn withdraw_held(
        &self,
        queue: &mut VecDeque<(Uuid, String, oneshot::Sender<Result<TurnReport>>)>,
        id: Uuid,
    ) -> Result<()> {
        let request = self.withdraw_request(id);
        let mut removed = false;
        while let Some(index) = queue.iter().position(|entry| entry.0 == id) {
            let (_, _, done) = queue.remove(index).expect("queued entry was present");
            let _ = done.send(Err(Error::Refused("input withdrawn".into())));
            removed = true;
        }
        if !removed {
            self.control_decision(request, Some(("not_queued", "input is not queued")));
            return Err(Error::Refused("input is not queued".into()));
        }
        self.control_outcome(
            request,
            json!({"type":"accepted"}),
            Event::InputRemoved {
                input_id: id.to_string(),
            },
        );
        Ok(())
    }
}
