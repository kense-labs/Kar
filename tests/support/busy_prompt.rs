use kar::{Direction, Error, Event, RecordKind, Session};
use uuid::Uuid;

/// Called while the fixture has a known slow or gated native turn.
pub(crate) async fn check(session: &Session, active: Uuid) {
    assert!(session.status().is_running());
    let before = session.records();
    let opener = before
        .iter()
        .find_map(|r| {
            r.prompt
                .as_ref()
                .filter(|p| p.input_id == active.to_string())
        })
        .unwrap();
    let first = opener.request_id.as_deref().unwrap();
    assert!(
        before
            .iter()
            .any(|r| r.native["type"] == "accepted" && r.native["requestId"] == first)
    );
    let rejected = Uuid::new_v4();
    for _ in 0..2 {
        assert!(matches!(
            session
                .prompt_with_id(rejected, "busy prompt must stay local")
                .await,
            Err(Error::Busy)
        ));
    }
    let records = session.records();
    let rows = &records[before.len()..];
    let mut seen = std::collections::HashSet::new();
    for r in rows.iter().filter(|r| {
        r.prompt
            .as_ref()
            .is_some_and(|p| p.input_id == rejected.to_string())
    }) {
        assert_eq!(
            (r.direction, r.kind),
            (Direction::ToRuntime, RecordKind::Request)
        );
        assert!(!r.events.contains(&Event::PromptDispatched));
        let id = r.prompt.as_ref().unwrap().request_id.as_deref().unwrap();
        assert_ne!(id, first);
        assert!(seen.insert(id));
        let mut replies = rows.iter().filter(|a| {
            a.direction == Direction::Host
                && a.kind == RecordKind::Response
                && a.native["requestId"] == id
        });
        let answer = replies.next().unwrap();
        assert_eq!(
            answer.native,
            serde_json::json!({"type":"rejected","code":"busy","reason":"busy","requestId":id})
        );
        assert!(answer.seq > r.seq && replies.next().is_none());
    }
    assert_eq!(seen.len(), 2);
    assert!(session.status().is_running());
}
