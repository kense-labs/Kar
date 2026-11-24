use super::*;
use serde_json::{Value, json};

struct Stream {
    id: Uuid,
    next: u64,
}
impl Stream {
    fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            next: 0,
        }
    }
    fn record(&mut self, session: &str, agent: &[&str], mut raw: Value) -> Arc<Record> {
        let seq = self.next;
        self.next += 1;
        raw["seq"] = seq.into();
        raw["receivedAt"] = seq.into();
        raw["sessionId"] = session.into();
        raw["agentPath"] = json!(agent);
        Arc::new(crate::voyage::convert(&raw, self.id, seq).unwrap())
    }
}
fn request(id: &str, op: &str, mut body: Value) -> Value {
    body["kind"] = op.into();
    json!({"kind":"request","direction":"toRuntime","id":id,"body":body})
}
fn decision(id: &str, body: Value) -> Value {
    json!({"kind":"response","requestId":id,"body":body})
}
fn frame(events: Value) -> Value {
    json!({"kind":"frame","body":{"type":"native","events":events}})
}
fn ack(id: &str) -> Value {
    decision(id, json!({"kind":"accepted"}))
}
fn echo(id: Option<&str>, msg: &str) -> Value {
    json!({"kind":"user_message","input":"same","inputId":id,"nativeMessageId":msg,"evidence":"turn_item","turnId":"turn-B"})
}

#[test]
fn queue_withdraw_retry_and_atomic_carries_keep_every_attempt_and_its_own_receipt() {
    let mut stream = Stream::new();
    let mut conversation = Conversation::default();
    let entries = [
        request(
            "q",
            "queue",
            json!({"inputId":"one","input":"same","origin":{"kind":"user"},"images":[{"path":"a.png"}]}),
        ),
        decision("q", json!({"kind":"accepted","queued":"adapter"})),
        request("w", "withdraw", json!({"inputId":"one"})),
        ack("w"),
        request(
            "bad",
            "prompt",
            json!({"input":"invalid aggregate","carries":["one","missing"]}),
        ),
        decision("bad", json!({"kind":"rejected","reason":"invalid carry"})),
        request(
            "retry",
            "queue",
            json!({"inputId":"one","input":"same","origin":{"kind":"notification","source":"job"}}),
        ),
        decision("retry", json!({"kind":"accepted","queued":"adapter"})),
        request(
            "deliver",
            "prompt",
            json!({"inputId":"bundle","input":"one combined","carries":["one","one"]}),
        ),
        ack("deliver"),
        frame(
            json!([{"kind":"input_queued","inputId":"bundle"},{"kind":"turn_active","inputId":"bundle"}]),
        ),
        frame(json!([{"kind":"input_handled","inputId":"bundle","requestId":"deliver"}])),
        frame(
            json!([{"kind":"input_dropped","inputId":"one","requestId":"q","reason":"runtime_refused"}]),
        ),
        frame(json!([
            echo(Some("bundle"), "native-message"),
            echo(Some("bundle"), "native-message"),
            echo(None, "other")
        ])),
    ];
    for (seq, raw) in entries.into_iter().enumerate() {
        let r = stream.record("root", &[], raw);
        conversation.apply(&r);
        conversation.apply(&r);
        let target = &conversation.inputs()[0];
        match seq {
            0 => assert_eq!(target.images[0].path.as_deref(), Some("a.png")),
            3 => assert_eq!(target.state, DeliveryState::Withdrawn),
            5 => assert_eq!(target.attempts.len(), 2),
            6 => {
                assert!(target.images.is_empty());
                assert_eq!(target.state, DeliveryState::Pending);
            }
            10 => assert_eq!(target.turn.as_ref().unwrap().phase, TurnPhase::Active),
            _ => {}
        }
    }
    assert_eq!(conversation.inputs().len(), 3);
    let target = &conversation.inputs()[0];
    assert_eq!(target.state, DeliveryState::Accepted);
    assert_eq!(target.reason, None);
    assert_eq!(
        target.origin.as_ref().unwrap().source.as_deref(),
        Some("job")
    );
    assert_eq!(
        target.attempts.iter().map(|a| a.kind).collect::<Vec<_>>(),
        [
            InputAttemptKind::Queue,
            InputAttemptKind::Withdraw,
            InputAttemptKind::Queue,
            InputAttemptKind::Prompt
        ]
    );
    let carrier = &conversation.inputs()[2];
    assert_eq!(carrier.carries, ["one", "one"]);
    assert_eq!(
        target.attempts[3].response.as_ref().unwrap().cursor(),
        carrier.attempts[0].response.as_ref().unwrap().cursor()
    );
    assert_eq!(target.handled, carrier.handled);
    assert_eq!(target.attempts[3].handled.as_ref().unwrap().after_seq, 11);
    assert!(target.observations.is_empty());
    assert_eq!(carrier.observations.len(), 1);
    assert_eq!(conversation.unlinked().len(), 1);
}

#[test]
fn resumed_echo_repairs_only_exit_drops_and_never_borrows_foreign_attempts_or_echoes() {
    let (mut old, mut new) = (Stream::new(), Stream::new());
    let mut state = Conversation::default();
    let mut fold = |stream: &mut Stream, session: &str, path: &[&str], raw| {
        state.apply(&stream.record(session, path, raw))
    };
    for (session, path) in [("s", vec![]), ("s", vec!["child"]), ("other", vec![])] {
        fold(
            &mut old,
            session,
            &path,
            request(
                "request",
                "queue",
                json!({"input":"same","inputId":"stable"}),
            ),
        );
        fold(&mut old, session, &path, ack("request"));
        fold(
            &mut old,
            session,
            &path,
            frame(json!([{"kind":"input_queued","inputId":"stable"}])),
        );
    }
    fold(
        &mut old,
        "s",
        &[],
        request("local", "prompt", json!({"input":"same"})),
    );
    fold(
        &mut old,
        "s",
        &[],
        decision("exit", json!({"kind":"exited"})),
    );
    fold(
        &mut new,
        "s",
        &[],
        request("local", "prompt", json!({"input":"same"})),
    );
    fold(
        &mut new,
        "s",
        &[],
        decision(
            "request",
            json!({"kind":"rejected","reason":"reused identifier in another stream"}),
        ),
    );
    fold(
        &mut new,
        "s",
        &[],
        frame(json!([
            echo(Some("stable"), "n"),
            echo(Some("stable"), "n")
        ])),
    );
    fold(&mut new, "s", &[], ack("local"));
    assert_eq!(state.inputs.len(), 5);
    let root = &state.inputs[0];
    assert_eq!(root.state, DeliveryState::Accepted);
    assert_eq!(root.observations.len(), 1);
    assert_eq!(root.attempts.len(), 1);
    assert_eq!(
        root.attempts[0].response.as_ref().unwrap().stream_id,
        old.id
    );
    assert_eq!(state.inputs[1].reason, Some(DropReason::RuntimeExited));
    assert_eq!(
        state.inputs[2].turn.as_ref().unwrap().phase,
        TurnPhase::Queued
    );
    assert_eq!(state.inputs[3].key.stream_id, Some(old.id));
    assert_eq!(state.inputs[3].state, DeliveryState::Pending);
    assert_eq!(state.inputs[4].key.stream_id, Some(new.id));
    assert_eq!(state.inputs[4].state, DeliveryState::Accepted);
    state.apply(&new.record(
        "s",
        &[],
        request(
            "next",
            "queue",
            json!({"input":"changed","inputId":"stable"}),
        ),
    ));
    state.apply(&new.record("s", &[], ack("next")));
    state.apply(&new.record("s",&[],frame(json!([{"kind":"input_dropped","requestId":"next","reason":"turn_interrupted"},echo(Some("stable"),"later")]))));
    assert_eq!(state.inputs[0].reason, Some(DropReason::TurnInterrupted));
}

#[test]
fn kar_wire_echo_elision_and_late_attempt_receipts_rebuild_from_serialized_records() {
    let (mut stream, mut records) = (Stream::new(), Vec::new());
    let input = Uuid::new_v4().to_string();
    let mut add = |kind: RecordKind,
                   direction: Direction,
                   native: Value,
                   events: Vec<Event>,
                   request: Option<&str>| {
        let mut r = (*stream.record("s", &[], frame(json!([])))).clone();
        r.upstream = None;
        r.kind = kind;
        r.direction = direction;
        r.native = native;
        r.events = events;
        r.response_to = request.map(str::to_owned);
        records.push(Arc::new(r));
    };
    add(
        RecordKind::Request,
        Direction::ToRuntime,
        json!({"type":"steer","id":"old","inputId":input,"input":"same"}),
        vec![],
        None,
    );
    add(
        RecordKind::Request,
        Direction::ToRuntime,
        json!({"type":"steer","id":"wire-id","message":"same"}),
        vec![],
        None,
    );
    add(
        RecordKind::Response,
        Direction::Host,
        json!({"type":"accepted","requestId":"old"}),
        vec![],
        Some("old"),
    );
    add(
        RecordKind::Request,
        Direction::ToRuntime,
        json!({"type":"queue","id":"retry","inputId":input,"text":"same"}),
        vec![],
        None,
    );
    add(
        RecordKind::Frame,
        Direction::FromRuntime,
        Value::Null,
        vec![Event::NativeInput {
            input_id: input.parse().unwrap(),
            receipt: crate::InputReceipt::Handled,
        }],
        Some("old"),
    );
    add(
        RecordKind::Frame,
        Direction::FromRuntime,
        json!({"method":"item/started","params":{"turnId":"turn","item":{"type":"userMessage","clientId":input,"id":"native-item","content":[{"type":"text","text":"same"}]}}}),
        vec![],
        None,
    );
    add(
        RecordKind::Frame,
        Direction::FromRuntime,
        json!({"method":"item/completed","params":{"turnId":"turn","item":{"type":"userMessage","clientId":input,"id":"native-item","content":[{"type":"text","text":"same"}]}}}),
        vec![],
        None,
    );
    let bytes = serde_json::to_vec(&records).unwrap();
    let replayed: Vec<Arc<Record>> = serde_json::from_slice(&bytes).unwrap();
    let view = conversation_of(&replayed);
    assert_eq!(view.inputs.len(), 1);
    let item = &view.inputs[0];
    assert_eq!(item.attempts.len(), 2);
    assert_eq!(item.attempts[0].handled.as_ref().unwrap().after_seq, 4);
    assert!(item.handled.is_none());
    assert_eq!(item.observations.len(), 1);
    assert_eq!(item.observations[0].evidence, EchoEvidence::TurnItem);
    assert_eq!(item.observations[0].turn_id.as_deref(), Some("turn"));
    let mut elided = replayed;
    for r in &mut elided[5..] {
        *r = Arc::new(r.elided_copy());
    }
    assert_eq!(
        conversation_of(&elided).inputs[0].observations,
        item.observations
    );
}
