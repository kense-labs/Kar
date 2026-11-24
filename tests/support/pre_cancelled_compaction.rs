use kar::{CompactionOptions, CompactionRejection, CompactionStatus, Direction, Session};

pub async fn check(session: &Session) {
    assert!(session.capabilities().manual_compaction);
    let options = CompactionOptions::default();
    options.signal.cancel();
    let before = session.records().len();
    let report = session.compact_with(&options).await.unwrap();
    assert!(matches!(
        report.status,
        CompactionStatus::Rejected {
            code: CompactionRejection::Aborted,
            ..
        }
    ));
    let rows = session.records();
    let outbound: Vec<_> = rows[before..]
        .iter()
        .filter(|r| r.direction == Direction::ToRuntime)
        .collect();
    assert_eq!(
        outbound.len(),
        1,
        "unexpected native call after pre-cancellation"
    );
    let request = outbound[0];
    assert_eq!(request.cursor(), report.request);
    assert_eq!(request.native["type"], "compact");
    let pair = session
        .control_receipt(request.control_request_id().unwrap())
        .unwrap();
    assert_eq!(Some(pair.response.cursor()), report.response);
    assert_eq!(pair.response.native["code"], "aborted");
    assert!(!session.status().is_running());
}
