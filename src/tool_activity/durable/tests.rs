use super::*;
use crate::{OutputPart, observe::Tool};
use serde_json::json;

fn view(tools: Value, nested: Value, entries: Value) -> Value {
    json!({"docs":{"pi.live":{"run":{"taskId":9,"first":5},"tools":tools,"nestedTools":nested}},"entries":entries})
}

fn cards(events: &[Event]) -> HashMap<String, Tool> {
    let mut cards = HashMap::<String, Tool>::new();
    for event in events {
        if let Some(id) = Tool::id(event) {
            cards.entry(id.to_owned()).or_default().apply(event, false);
        }
    }
    cards
}

#[test]
fn attach_to_running_tools_and_replay_updates() {
    let assistant = json!({"id":6,"model":[{"role":"assistant","content":[
        {"type":"toolCall","id":"outer","arguments":{"path":"new.txt"}}
    ]}]});
    let older = json!({"id":1,"model":[{"role":"assistant","content":[
        {"type":"toolCall","id":"outer","arguments":{"path":"old.txt"}}
    ]}]});
    let mut running = view(
        json!([{"callId":"outer","name":"read","taskId":10,"status":"running"}]),
        json!([{"callId":"outer/child","name":"inspect","taskId":11,"arguments":{"part":1},"status":"running","output":"abc"}]),
        json!([older, assistant]),
    );
    let (_, mut events) = changes(&Value::Null, &running, &[], true);
    let attached = cards(&events);
    assert_eq!(attached["outer"].input, Some(json!({"path":"new.txt"})));
    assert_eq!(attached["outer/child"].preview, "abc");
    assert_eq!(attached["outer/child"].input, Some(json!({"part":1})));
    let original = running.clone();
    running["docs"]["pi.live"]["nestedTools"][0]["output"] = "bcde".into();
    running["docs"]["pi.live"]["nestedTools"][0]["arguments"] = json!({"part":2});
    let (_, next) = changes(&original, &running, &[], false);
    events.extend(next);
    assert_eq!(cards(&events)["outer/child"].preview, "bcde");
    assert_eq!(cards(&events)["outer/child"].input, Some(json!({"part":2})));
    assert_eq!(changes(&running, &running, &[], false), (vec![], vec![]));
    let result = json!({"id":19,"byTaskId":10,"model":[
        {"role":"toolResult","toolCallId":"elsewhere","isError":true},
        {"role":"toolResult","toolCallId":"outer","isError":false,"content":[
             {"type":"text","text":"contents"},{"type":"image","mimeType":"image/png","data":"AQ=="},
             {"type":"blob","key":"raw"}]}
    ]});
    let mut done = running.clone();
    done["entries"] = json!([]);
    let slots = &mut done["docs"]["pi.live"];
    slots["tools"][0]["status"] = "done".into();
    slots["tools"][0]["entry"] = 19.into();
    slots["nestedTools"][0]["status"] = "done".into();
    slots["nestedTools"][0]["summary"] = json!({"isError":true});
    let (removed, ended) = changes(&running, &done, &[&result], false);
    assert!(removed.is_empty());
    events.extend(ended);
    let cards = cards(&events);
    let outer = cards["outer"].result.as_ref().unwrap();
    assert_eq!(outer.status, Some(ToolStatus::Succeeded));
    assert_eq!(
        outer.content,
        Some(vec![
            OutputPart::Text {
                text: "contents".into()
            },
            OutputPart::Image {
                media_type: "image/png".into(),
                data: "AQ==".into()
            },
            OutputPart::Other {
                value: json!({"type":"blob","key":"raw"})
            },
        ])
    );
    assert_eq!(
        cards["outer/child"].result,
        Some(ToolResult {
            status: Some(ToolStatus::Failed),
            ..Default::default()
        })
    );
    assert_eq!(
        changes(&done, &running, &[], true).1,
        changes(&Value::Null, &running, &[], true).1
    );
    assert_eq!(changes(&Value::Null, &done, &[], true), (vec![], vec![]));
    assert_eq!(changes(&done, &done, &[&result], false), (vec![], vec![]));
}

#[test]
fn removed_slots_use_their_own_result_and_pending_slots_only_end_with_evidence() {
    let old = view(
        json!([
            {"callId":"same","taskId":22,"status":"running","name":"old"},
            {"callId":"unstarted","status":"pending","name":"read"},
            {"callId":"aborted","status":"pending","name":"read"},
            {"callId":"finished","status":"done","taskId":8}
        ]),
        json!([]),
        json!([]),
    );
    let new = view(
        json!([{"callId":"same","taskId":23,"status":"running","name":"new"}]),
        json!([]),
        json!([]),
    );
    let unrelated = json!({"id":30,"byTaskId":23,"model":[{"role":"toolResult","toolCallId":"same","isError":false}]});
    let aborted = json!({"id":31,"model":[{"role":"toolResult","toolCallId":"aborted","isError":true,"content":"stopped"}]});
    let (removed, started) = changes(&old, &new, &[&unrelated, &aborted], false);
    assert!(matches!(&removed[0], Event::ToolEnded { id } if id == "same"));
    let mut lifecycle = changes(&Value::Null, &old, &[], true).1;
    lifecycle.push(Event::ToolProgress {
        id: "same".into(),
        output: Some("previous output".into()),
        delta: None,
    });
    lifecycle.extend(removed.iter().chain(&started).cloned());
    let latest = &cards(&lifecycle)["same"];
    assert_eq!(latest.name.as_deref(), Some("new"));
    assert!(!latest.finished && latest.preview.is_empty() && latest.result.is_none());
    let removed_cards = cards(&removed);
    assert!(removed_cards["same"].result.is_none());
    assert!(!removed_cards.contains_key("finished") && !removed_cards.contains_key("unstarted"));
    assert_eq!(
        removed_cards["aborted"]
            .result
            .as_ref()
            .unwrap()
            .text()
            .as_deref(),
        Some("stopped")
    );
    assert_eq!(
        removed_cards["aborted"].result.as_ref().unwrap().status,
        Some(ToolStatus::Failed)
    );
    assert!(matches!(&started[..], [Event::ToolStarted {id, name}] if id=="same" && name=="new"));
}

#[test]
fn pending_call_settled_in_one_publication_has_one_result() {
    let old = view(
        json!([{"callId":"call","name":"inspect","status":"pending"}]),
        json!([]),
        json!([]),
    );
    let call = json!({"id":10,"model":[{"role":"assistant","content":[{"type":"toolCall","id":"call","arguments":{"mode":"x"}}]}]});
    let result = json!({"id":11,"byTaskId":7,"model":[{"role":"toolResult","toolCallId":"call","isError":false,"content":[]}]});
    let new = view(
        json!([{"callId":"call","taskId":7,"name":"inspect","status":"done","entry":11}]),
        json!([]),
        json!([]),
    );
    let (removed, events) = changes(&old, &new, &[&call, &result], false);
    assert!(removed.is_empty());
    assert!(matches!(
        &events[..],
        [
            Event::ToolStarted { .. },
            Event::ToolInput { .. },
            Event::ToolResult { .. },
            Event::ToolEnded { .. }
        ]
    ));
    let activity = &cards(&events)["call"];
    assert_eq!(activity.input, Some(json!({"mode":"x"})));
    assert_eq!(activity.result.as_ref().unwrap().content, Some(vec![]));
    let mut anonymous = result;
    anonymous.as_object_mut().unwrap().remove("byTaskId");
    assert_eq!(
        changes(&old, &new, &[&call, &anonymous], false),
        (removed, events)
    );
}
