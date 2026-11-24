use crate::{
    ChangeKind, Direction, Event, OutputPart, Protocol, RecordKind, ToolStatus,
    observe::SessionView, stream::Journal,
};
use serde_json::{Value, json};

fn fold(protocol: Protocol, frames: Vec<Value>) -> SessionView {
    let journal = Journal::new(None).unwrap();
    journal.bind_root("root");
    for v in frames {
        let events = super::project(protocol, &v, false);
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            v,
            events,
        );
    }
    let mut view = SessionView::default();
    for r in journal.records() {
        assert!(!r.elided);
        let decoded = serde_json::from_slice(&serde_json::to_vec(&*r).unwrap()).unwrap();
        view.apply(&decoded).unwrap();
    }
    view
}
#[test]
fn codex_keeps_deltas_status_paths_and_latest_turn_patch_on_replay() {
    let item =
        |method: &str, item: Value| json!({"method":method,"params":{"turnId":"turn","item":item}});
    let delta = |text: &str| json!({"method":"item/commandExecution/outputDelta","params":{"itemId":"cmd","delta":text}});
    let patch =
        |diff: &str| json!({"method":"turn/diff/updated","params":{"turnId":"turn","diff":diff}});
    let files = json!([{"path":"a","kind":{"type":"update","move_path":"b"}},{"path":"c","kind":{"type":"add"}},{"path":"d","kind":{"type":"delete"}}]);
    let view = fold(
        Protocol::Codex,
        vec![
            item(
                "item/started",
                json!({"type":"commandExecution","id":"cmd","command":"cargo test"}),
            ),
            delta("one"),
            delta(" two"),
            item(
                "item/completed",
                json!({"type":"commandExecution","id":"cmd","status":"failed","exitCode":7,"aggregatedOutput":"native final"}),
            ),
            item(
                "item/completed",
                json!({"type":"fileChange","id":"failed","status":"failed","changes":files}),
            ),
            item(
                "item/completed",
                json!({"type":"fileChange","id":"edited","status":"completed","changes":files}),
            ),
            patch("partial"),
            patch("full patch"),
        ],
    );
    let agent = view.agents.values().next().unwrap();
    let tool = &agent.tools["cmd"];
    assert_eq!(tool.preview, "one two");
    assert_eq!(tool.input, Some(json!("cargo test")));
    let result = tool.result.as_ref().unwrap();
    assert_eq!(result.status, Some(ToolStatus::Failed));
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(
        result.content.as_ref().unwrap(),
        &[OutputPart::Text {
            text: "native final".into()
        }]
    );
    assert!(
        agent.tools["failed"]
            .result
            .as_ref()
            .unwrap()
            .file_changes
            .is_empty()
    );
    let files = &agent.tools["edited"].result.as_ref().unwrap().file_changes;
    assert_eq!(files[0].move_to.as_deref(), Some("b"));
    assert_eq!(files[1].kind, ChangeKind::Added);
    assert_eq!(files[2].kind, ChangeKind::Deleted);
    assert_eq!(agent.turn_diffs["turn"], "full patch");
}
#[test]
fn acp_replaces_previews_and_preserves_unknown_result_parts() {
    let frame = |mut update: Value| {
        update["toolCallId"] = "call".into();
        json!({"method":"session/update","params":{"update":update}})
    };
    let view = fold(
        Protocol::Acp,
        vec![
            frame(json!({"sessionUpdate":"tool_call","title":"run","rawInput":{}})),
            frame(
                json!({"sessionUpdate":"tool_call_update","rawInput":{"command":"ls"},"rawOutput":"one"}),
            ),
            frame(json!({"sessionUpdate":"tool_call_update","rawOutput":"two"})),
            frame(
                json!({"sessionUpdate":"tool_call_update","status":"cancelled","content":[{"type":"terminal","terminalId":"t"}]}),
            ),
        ],
    );
    let tool = &view.agents.values().next().unwrap().tools["call"];
    assert_eq!(tool.input, Some(json!({"command":"ls"})));
    assert_eq!(tool.preview, "two");
    assert!(tool.finished);
    let result = tool.result.as_ref().unwrap();
    assert_eq!(result.status, Some(ToolStatus::Cancelled));
    assert!(
        matches!(&result.content.as_ref().unwrap()[0],OutputPart::Other{value} if value["terminalId"]=="t")
    );
}
#[test]
fn stream_json_and_pi_keep_native_input_and_result() {
    for p in [Protocol::Claude, Protocol::Qwen] {
        let view = fold(
            p,
            vec![
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"Read","input":{"file_path":"a"}}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x","is_error":true,"content":"missing"}]}}),
            ],
        );
        let tool = &view.agents.values().next().unwrap().tools["x"];
        assert_eq!(tool.input, Some(json!({"file_path":"a"})));
        assert_eq!(
            tool.result.as_ref().unwrap().status,
            Some(ToolStatus::Failed)
        );
    }
    let pi = |name: &str, text: &str| json!({"type":name,"toolCallId":"x","partialResult":{"content":[{"type":"text","text":text}]}});
    let view = fold(
        Protocol::Pi,
        vec![
            json!({"type":"tool_execution_start","toolName":"read","toolCallId":"x","args":{"path":"a"}}),
            pi("tool_execution_update", "a"),
            pi("tool_execution_update", "ab"),
            json!({"type":"tool_execution_end","toolCallId":"x","isError":false,"result":{"content":[{"type":"image","mimeType":"image/png","data":"AA=="}]}}),
        ],
    );
    let tool = &view.agents.values().next().unwrap().tools["x"];
    assert_eq!(tool.preview, "ab");
    assert!(
        matches!(&tool.result.as_ref().unwrap().content.as_ref().unwrap()[0],OutputPart::Image{media_type,..} if media_type=="image/png")
    );
}
#[test]
fn tool_updates_without_a_call_id_never_attach_to_another_call() {
    let events = super::project(
        Protocol::Codex,
        &json!({"method":"item/commandExecution/outputDelta","params":{"delta":"outside"}}),
        false,
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ToolProgress { .. }))
    );
}
