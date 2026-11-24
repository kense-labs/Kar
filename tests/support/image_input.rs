use kar::{Direction, Error, Event, RecordKind, Session};
use serde_json::{Value, json};
use std::path::PathBuf;

pub(crate) const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAKElEQVR4nO3NsQ0AAAzCMP5/un0CNkuZ41wybXsHAAAAAAAAAAAAxR4yw/wuPL6QkAAAAABJRU5ErkJggg==";

fn rejection(
    session: &Session,
    start: usize,
    control: &str,
    text: &str,
    paths: &[PathBuf],
    code: &str,
) -> String {
    let all = session.records();
    let since = &all[start..];
    assert!(
        !since
            .iter()
            .any(|r| r.events.contains(&Event::PromptDispatched))
    );
    let mut attempts = since.iter().filter(|r| {
        r.kind == RecordKind::Request
            && r.direction == Direction::ToRuntime
            && (r.native["control"] == control || r.native["type"] == control)
    });
    let attempt = attempts.next().unwrap();
    assert!(attempts.next().is_none());
    let request = attempt.native["id"].as_str().unwrap();
    assert_ne!(request, attempt.native["inputId"].as_str().unwrap());
    assert_eq!(attempt.native["input"], text);
    assert_eq!(
        attempt.native["images"],
        Value::Array(paths.iter().map(|p| json!({"path":p})).collect())
    );
    let mut replies = since
        .iter()
        .filter(|r| r.kind == RecordKind::Response && r.native["requestId"] == request);
    let reply = replies.next().unwrap();
    assert!(replies.next().is_none() && reply.seq > attempt.seq);
    assert_eq!(reply.native["type"], "rejected");
    assert_eq!(reply.native["code"], code);
    assert!(matches!(
        session.status(),
        kar::observe::AgentStatus::Idle {
            released: false,
            ..
        }
    ));
    reply.native["reason"].as_str().unwrap().to_owned()
}

pub(crate) async fn empty_controls(session: &Session) {
    for control in ["prompt", "steer", "queue"] {
        let start = session.records().len();
        let result = match control {
            "prompt" => session.prompt("").await.map(drop),
            "steer" => session.steer("").await,
            _ => session.queue("").await.map(drop),
        };
        assert!(
            matches!(result, Err(Error::Unsupported(ref reason)) if reason == "empty input: give text or images")
        );
        assert_eq!(
            rejection(session, start, control, "", &[], "unsupported"),
            "empty input: give text or images"
        );
    }
}

pub(crate) async fn refused_files(session: &Session, text: &str, paths: &[PathBuf], code: &str) {
    let start = session.records().len();
    let result = session.prompt_image_paths(text, paths).await;
    match code {
        "error" => assert!(matches!(result, Err(Error::Io(_)))),
        _ => assert!(matches!(result, Err(Error::Unsupported(_)))),
    }
    assert!(!rejection(session, start, "prompt", text, paths, code).is_empty());
}
