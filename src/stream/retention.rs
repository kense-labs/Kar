//! In-memory replay can shed native/tool payloads after subscribers and durable storage see them.
use super::{Event, Record, RecordKind, State};
use crate::ToolResult;
use serde_json::Value;
use std::{
    io::{self, Write},
    sync::Arc,
};

#[derive(Default)]
struct Bytes(usize);

impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Record {
    pub(super) fn payload_bytes(&self) -> usize {
        let mut size = Bytes::default();
        serde_json::to_writer(&mut size, &self.native).expect("native frame is JSON");
        let mut tools = 0;
        for event in &self.events {
            if matches!(
                event,
                Event::ToolInput { .. }
                    | Event::ToolInputDelta { .. }
                    | Event::ToolProgress { .. }
                    | Event::ToolResult { .. }
            ) {
                serde_json::to_writer(&mut size, event).expect("projected tool event is JSON");
                tools += 1;
            }
        }
        // JSON array brackets and commas, only when tool payloads are present.
        size.0
            .saturating_add(if tools == 0 { 0 } else { tools + 1 })
    }

    pub(super) fn elided_copy(&self) -> Self {
        let events = self
            .events
            .iter()
            .map(|event| match event {
                Event::ToolInput { id, .. } => Event::ToolInput {
                    id: id.clone(),
                    input: Value::String(String::new()),
                },
                Event::ToolInputDelta { id, .. } => Event::ToolInputDelta {
                    id: id.clone(),
                    delta: String::new(),
                },
                Event::ToolProgress { id, .. } => Event::ToolProgress {
                    id: id.clone(),
                    output: None,
                    delta: None,
                },
                Event::ToolResult { id, result } => Event::ToolResult {
                    id: id.clone(),
                    result: ToolResult {
                        status: result.status.clone(),
                        exit_code: result.exit_code,
                        content: None,
                        file_changes: result.file_changes.clone(),
                    },
                },
                other => other.clone(),
            })
            .collect();
        Self {
            stream_id: self.stream_id,
            seq: self.seq,
            timestamp_ms: self.timestamp_ms,
            direction: self.direction,
            kind: self.kind,
            prompt: self.prompt.clone(),
            response_to: self.response_to.clone(),
            opening: self.opening.clone(),
            origin: self.origin.clone(),
            carries: self.carries.clone(),
            frame_type: self.frame_type.clone(),
            session_id: self.session_id.clone(),
            span_id: self.span_id.clone(),
            agent_path: self.agent_path.clone(),
            elided: true,
            native: Value::Null,
            upstream: self.upstream.clone(),
            events,
        }
    }
}

pub(crate) fn check_limit(limit: Option<usize>) -> crate::Result<()> {
    if limit == Some(0) {
        return Err(crate::Error::Unsupported(
            "retained_bytes must be positive".into(),
        ));
    }
    Ok(())
}

impl State {
    pub(super) fn trim_history(&mut self) {
        let Some(limit) = self.retained_bytes else {
            return;
        };
        for index in self.measured..self.records.len() {
            let record = &self.records[index];
            if record.kind != RecordKind::Frame {
                continue;
            }
            let size = record.payload_bytes();
            self.bytes = self.bytes.saturating_add(size);
            self.sizes.push_back((index, size));
            while self.bytes > limit && self.sizes.len() > 1 {
                let (old, size) = self.sizes.pop_front().expect("oldest measured frame");
                self.bytes -= size;
                self.records[old] = Arc::new(self.records[old].elided_copy());
            }
        }
        self.measured = self.records.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ChangeKind, Direction, FileChange, OutputPart, ToolStatus, observe::SessionView,
        stream::Journal,
    };
    use serde_json::json;

    #[tokio::test]
    async fn counts_projected_tool_payloads_and_keeps_live_delivery_and_replay_positions() {
        assert!(Journal::new(Some(0)).is_err());
        let journal = Journal::new(Some(512)).unwrap();
        journal.bind_root("root");
        let mut early = journal.subscribe(None).unwrap();
        let id = "read".to_owned();
        let content = "读取".repeat(256);
        let file = FileChange {
            path: "sample".into(),
            kind: ChangeKind::Modified,
            move_to: None,
        };
        let events = vec![
            Event::ToolStarted {
                id: id.clone(),
                name: "read".into(),
            },
            Event::ToolInputDelta {
                id: id.clone(),
                delta: content.clone(),
            },
            Event::ToolInput {
                id: id.clone(),
                input: json!({"data":content}),
            },
            Event::ToolProgress {
                id: id.clone(),
                output: Some(content.clone()),
                delta: Some(content.clone()),
            },
            Event::ToolResult {
                id: id.clone(),
                result: ToolResult {
                    status: Some(ToolStatus::Failed),
                    exit_code: Some(7),
                    content: Some(vec![OutputPart::Text {
                        text: content.clone(),
                    }]),
                    file_changes: vec![file.clone()],
                },
            },
            Event::TextDelta {
                text: "回答".into(),
            },
        ];
        let native = json!({"n":"一"});
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            native.clone(),
            events.clone(),
        );
        journal.push(
            Direction::FromRuntime,
            RecordKind::Response,
            None,
            vec![],
            json!("response"),
            vec![],
        );
        let full = early.next().await.unwrap();
        assert_eq!(
            full.payload_bytes(),
            native.to_string().len() + serde_json::to_vec(&events[1..=4]).unwrap().len()
        );
        assert!(!journal.records()[0].elided);
        let mut subscribed = journal.events(None).unwrap();
        let at = subscribed.next().await.unwrap().cursor;
        journal.host_event(Event::TurnEnded {
            outcome: crate::Outcome::Completed,
        });
        journal.close();
        assert!(!subscribed.next().await.unwrap().elided);
        let mut late = journal.events(Some(&at)).unwrap();
        for index in 1..events.len() {
            let next = late.next().await.unwrap();
            assert_eq!(
                (next.cursor.seq, next.cursor.index, next.elided),
                (0, index, true)
            );
        }
        let end = late.next().await.unwrap();
        assert_eq!(
            (end.cursor.seq, end.cursor.index, end.elided),
            (2, 0, false)
        );
        assert!(late.next().await.is_none());
        assert_eq!(full.events, events);
        let records = journal.records();
        assert!(records[0].native.is_null());
        assert!(
            !serde_json::to_string(&records[0].events)
                .unwrap()
                .contains(&content)
        );
        assert_eq!(records[1].native, json!("response"));
        let mut replay = SessionView::default();
        for record in records {
            replay.apply(&record).unwrap();
        }
        let agent = replay.agents.values().next().unwrap();
        assert_eq!(agent.messages, ["回答"]);
        let tool = &agent.tools[&id];
        assert!(tool.elided && tool.finished && tool.preview.is_empty());
        assert_eq!(tool.name.as_deref(), Some("read"));
        assert_eq!(tool.input, Some(json!("")));
        let result = tool.result.as_ref().unwrap();
        assert_eq!(result.status, Some(ToolStatus::Failed));
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.file_changes, [file]);
        assert!(result.content.is_none());
    }
}
