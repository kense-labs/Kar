//! Public queue and withdrawal cases on stdio adapters that actually hold inputs locally.
use kar::{Error, Event, Outcome, Record, RecordKind, Session};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

fn control<'a>(rows: &'a [Arc<Record>], kind: &str, id: Uuid) -> (&'a Record, &'a Record) {
    let request = rows
        .iter()
        .rev()
        .find(|r| {
            r.kind == RecordKind::Request
                && r.native["type"] == kind
                && r.native["inputId"] == id.to_string()
        })
        .unwrap();
    assert!(request.native["id"].as_str().is_some());
    let decisions: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == RecordKind::Response && r.native["requestId"] == request.native["id"])
        .collect();
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].seq > request.seq);
    (request, decisions[0])
}

fn ends(rows: &[Arc<Record>]) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.agent_path.is_empty())
        .flat_map(|r| {
            r.events.iter().filter_map(move |e| match e {
                Event::TurnEnded { outcome } => {
                    assert_eq!(*outcome, Outcome::Completed);
                    Some(r.seq)
                }
                _ => None,
            })
        })
        .collect()
}

pub(crate) async fn check(s: &Session, slow: &str, hold: impl Fn(), release: impl Fn()) {
    hold();
    let from = s.records().len();
    let first = s.prompt(slow).await.unwrap();
    let (id, queued) = s.queue("queue then run").await.unwrap();
    let rows = s.records();
    let (_, accepted) = control(&rows, "queue", id);
    assert_eq!(accepted.native["type"], "accepted");
    assert_eq!(accepted.native["queued"], "adapter");
    assert_eq!(
        queued.admission().unwrap().response.cursor(),
        accepted.cursor()
    );
    assert!(ends(&rows[from..]).is_empty());
    release();
    assert_eq!(first.wait().await.unwrap(), Outcome::Completed);
    assert_eq!(queued.wait().await.unwrap(), Outcome::Completed);
    let rows = s.records();
    let completed = ends(&rows[from..]);
    let dispatch = rows
        .iter()
        .find(|r| {
            r.prompt
                .as_ref()
                .is_some_and(|p| p.input_id == id.to_string())
        })
        .unwrap();
    assert_eq!(completed.len(), 2);
    assert_eq!(dispatch.carries, [id.to_string()]);
    assert!(
        accepted.seq < completed[0] && completed[0] < dispatch.seq && dispatch.seq < completed[1]
    );
    assert!(
        rows[from..]
            .iter()
            .filter(|r| r
                .events
                .iter()
                .any(|e| matches!(e, Event::TurnEnded { .. })))
            .all(|r| matches!(r.events.last(), Some(Event::TurnEnded { .. })))
    );
    hold();
    let from = s.records().len();
    let first = s.prompt(slow).await.unwrap();
    let (removed, withdrawn_turn) = s.queue("withdraw this input").await.unwrap();
    let retry = s
        .queue_with_id(removed, "withdraw this input")
        .await
        .unwrap();
    assert_ne!(
        withdrawn_turn
            .admission()
            .unwrap()
            .request
            .control_request_id(),
        retry.admission().unwrap().request.control_request_id()
    );
    let rows = s.records();
    let (before, accepted) = control(&rows, "queue", removed);
    assert_eq!(accepted.native["type"], "accepted");
    s.withdraw(removed).await.unwrap();
    let after = s.records();
    assert!(Arc::ptr_eq(
        &after[before.seq as usize],
        &rows[before.seq as usize]
    ));
    assert!(Arc::ptr_eq(
        &after[accepted.seq as usize],
        &rows[accepted.seq as usize]
    ));
    let (request, decision) = control(&after, "withdraw", removed);
    assert_eq!(decision.native["type"], "accepted");
    assert_eq!(withdrawn_turn.wait().await.unwrap(), Outcome::Aborted);
    assert_eq!(retry.wait().await.unwrap(), Outcome::Aborted);
    for gone in [removed, Uuid::new_v4()] {
        assert!(matches!(s.withdraw(gone).await, Err(Error::Refused(_))));
        let after = s.records();
        let (attempt, failure) = control(&after, "withdraw", gone);
        assert_ne!(attempt.native["id"], request.native["id"]);
        assert_eq!(failure.native["type"], "rejected");
        assert_eq!(failure.native["code"], "not_queued");
    }
    release();
    assert_eq!(first.wait().await.unwrap(), Outcome::Completed);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let rows = s.records();
    assert_eq!(ends(&rows[from..]).len(), 1);
    assert!(
        !s.status().is_running(),
        "withdrawn input must leave the session idle"
    );
    assert!(!rows[from..].iter().any(|r| {
        r.prompt
            .as_ref()
            .is_some_and(|p| p.input_id == removed.to_string())
            || r.events.contains(&Event::UserMessage {
                text: "withdraw this input".into(),
            })
    }));
    s.close().await.unwrap();
    assert!(matches!(s.withdraw(removed).await, Err(Error::Closed(_))));
    let records = s.records();
    let (_, terminal) = control(&records, "withdraw", removed);
    assert_eq!(terminal.native["type"], "rejected");
    assert!(matches!(
        terminal.native["code"].as_str(),
        Some("disposed" | "runtime_exited")
    ));
}
