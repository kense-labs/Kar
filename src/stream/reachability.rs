use super::{Direction, Event, Journal, RecordKind, RecordStamp, State};
use crate::{Error, Result};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Reachability {
    exited: bool,
    released: bool,
    disposing: bool,
}

impl Reachability {
    pub(super) fn observe(
        &mut self,
        kind: RecordKind,
        direction: Direction,
        native: &Value,
        events: &[Event],
    ) {
        match (kind, direction, native["type"].as_str()) {
            (RecordKind::Request, Direction::ToRuntime, Some("dispose")) => self.disposing = true,
            (RecordKind::Response, Direction::Host, Some("exited")) => self.exited = true,
            (RecordKind::Response, Direction::Host, Some("released")) => self.released = true,
            _ if events.contains(&Event::Released) => self.released = true,
            _ => {}
        }
    }
    fn rejection(&self, closed: bool) -> Option<(&'static str, &'static str)> {
        if self.exited {
            Some(("runtime_exited", "runtime exited"))
        } else if self.disposing {
            Some(("disposed", "session disposed"))
        } else if self.released || closed {
            Some(("session_released", "session released"))
        } else {
            None
        }
    }
}

impl Journal {
    pub(crate) fn reject_unreachable(
        &self,
        control: impl FnOnce() -> Option<(RecordStamp, Value, Uuid)>,
    ) -> Option<Error> {
        let mut state = self.state.lock().unwrap();
        let (code, reason) = state.reachability.rejection(state.closed)?;
        let (stamp, body, id) = control()?;
        self.append_rejection(&mut state, stamp, body, id, code, reason);
        Some(Error::Closed(reason.into()))
    }

    /// Admission checks and their recorded decisions settle together with respect to native exit.
    pub(crate) fn refused_input(
        &self,
        stamp: RecordStamp,
        body: Value,
        id: Uuid,
        code: &str,
        reason: &str,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let stopped = state.reachability.rejection(state.closed);
        let (code, reason) = stopped.unwrap_or((code, reason));
        self.append_rejection(&mut state, stamp, body, id, code, reason);
        if stopped.is_some() {
            Err(Error::Closed(reason.into()))
        } else {
            Ok(())
        }
    }
    fn append_rejection(
        &self,
        state: &mut State,
        stamp: RecordStamp,
        body: Value,
        id: Uuid,
        code: &str,
        reason: &str,
    ) {
        self.append_locked(
            state,
            stamp,
            RecordKind::Request,
            None,
            vec![],
            None,
            body,
            vec![],
        );
        let response = json!({"type":"rejected","requestId":id,"code":code,"reason":reason});
        self.append_locked(
            state,
            Direction::Host.into(),
            RecordKind::Response,
            None,
            vec![],
            None,
            response,
            vec![],
        );
    }
    pub(crate) fn dispose_after_exit(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.reachability.exited || state.reachability.disposing {
            return;
        }
        let id = Uuid::new_v4();
        self.append_locked(
            &mut state,
            Direction::ToRuntime.into(),
            RecordKind::Request,
            None,
            vec![],
            None,
            json!({"type":"dispose","id":id}),
            vec![],
        );
        self.append_locked(
            &mut state,
            Direction::Host.into(),
            RecordKind::Response,
            None,
            vec![],
            None,
            json!({"type":"accepted","requestId":id}),
            vec![],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn root_exit_settles_dispose_once_and_keeps_late_control_replayable() {
        let journal = Journal::new(None).unwrap();
        journal.bind_root("root");
        let mut reader = journal.subscribe(None).unwrap();
        let control = || {
            let id = Uuid::new_v4();
            Some((
                Direction::ToRuntime.into(),
                json!({"type":"abort","id":id}),
                id,
            ))
        };
        journal.push(
            Direction::Host,
            RecordKind::Response,
            Some("child".into()),
            vec!["agent".into()],
            json!({"type":"exited","code":0}),
            vec![],
        );
        assert!(journal.reject_unreachable(control).is_none());
        journal.push(
            Direction::Host,
            RecordKind::Response,
            None,
            vec![],
            json!({"type":"exited","code":9}),
            vec![],
        );
        journal.close();
        journal.dispose_after_exit();
        journal.dispose_after_exit();
        assert!(matches!(
            journal.reject_unreachable(control),
            Some(Error::Closed(_))
        ));
        let rows = journal.records();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[2].native["type"], "dispose");
        assert_eq!(rows[3].control_response_to(), rows[2].control_request_id());
        assert_eq!(rows[5].native["code"], "runtime_exited");
        for row in rows {
            assert_eq!(reader.next().await.unwrap().cursor(), row.cursor());
        }
        assert!(reader.next().await.is_none());
    }
}
