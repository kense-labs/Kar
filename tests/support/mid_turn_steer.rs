use kar::{Direction, RecordKind, Session};
use uuid::Uuid;

/// The fixture holds the active native model open and checks its terminal outcome afterward.
pub async fn check(session: &Session, text: &str) {
    let from = session.records().len();
    let input = Uuid::new_v4();
    let kar::observe::AgentStatus::Running {
        input_id: Some(owner),
        request_id: Some(attempt),
        ..
    } = session.status()
    else {
        panic!("expected a running root prompt with a confirmed identity");
    };
    session
        .steer_turn(
            owner.parse().unwrap(),
            Some(attempt.parse().unwrap()),
            input,
            text,
            vec![],
        )
        .await
        .unwrap();
    let records = session.records();
    let rows = &records[from..];
    let request = rows
        .iter()
        .find(|r| {
            r.kind == RecordKind::Request
                && r.direction == Direction::ToRuntime
                && r.native["type"] == "steer"
                && r.native["inputId"] == input.to_string()
        })
        .unwrap();
    assert_eq!(request.native["input"], text);
    assert_eq!(request.native["targetInputId"], owner);
    assert_eq!(request.native["targetRequestId"], attempt);
    let replies = rows
        .iter()
        .filter(|r| {
            r.kind == RecordKind::Response
                && r.direction == Direction::Host
                && r.native["requestId"] == request.native["id"]
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].native["type"], "accepted");
    assert!(replies[0].seq > request.seq);
    println!(
        "public mid-turn steer accepted on {:?} with request and response seq {}..{}",
        session.protocol(),
        request.seq,
        replies[0].seq
    );
}
