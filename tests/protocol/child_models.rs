use kar::{ApprovalMode, Event, Outcome, Protocol, observe::SessionView};
use serde_json::{Value, json};

pub(super) fn rpc_frames(protocol: &str) {
    for (id, model) in [("child", Some("initial")), ("sibling", None)] {
        if protocol == "codex" {
            super::emit(
                json!({"method":"item/completed","params":{"threadId":"session-1","item":{
                "type":"subAgentActivity","id":format!("spawn-{id}"),"kind":"started","agentThreadId":id,"model":model}}}),
            );
        } else {
            super::emit(
                json!({"method":"_x.ai/session_notification","params":{"sessionId":"session-1",
                "update":{"sessionUpdate":"subagent_spawned","subagent_id":id,"child_session_id":id,"model":model}}}),
            );
        }
    }
    if protocol == "codex" {
        for thread in [
            json!({"id":"session-1","model":"ignore-root-start"}),
            json!({"id":"child","model":"selected","reasoningEffort":"medium"}),
            json!({"id":"sibling","modelProvider":"unresolved"}),
        ] {
            super::emit(
                json!({"method":"thread/started","params":{"threadId":"session-1","thread":thread}}),
            );
        }
        super::emit(
            json!({"method":"thread/settings/updated","params":{"threadId":"child","threadSettings":{"model":"later"}}}),
        );
    }
}

pub(super) fn claude_frames(session: &str) {
    let assistant = |parent: Option<&str>, model: Option<&str>, content: Value| {
        super::emit(
            json!({"type":"assistant","session_id":session,"parent_tool_use_id":parent,
            "message":{"role":"assistant","model":model,"content":content}}),
        );
    };
    assistant(None, Some("unconfirmed-root"), json!([]));
    assistant(Some("first"), None, json!([]));
    assistant(
        Some("first"),
        Some("small"),
        json!([{"type":"tool_use","id":"nested","name":"Task","input":{}}]),
    );
    super::emit(
        json!({"type":"system","session_id":session,"subtype":"init","model":"root","tools":[]}),
    );
    for (parent, model) in [
        ("first", "small"),
        ("second", "small"),
        ("nested", "small"),
        ("first", "<synthetic>"),
        ("first", "large"),
        ("second", "small"),
    ] {
        assistant(Some(parent), Some(model), json!([]));
    }
}

pub(super) async fn exercise() -> kar::Result<()> {
    for protocol in [Protocol::Codex, Protocol::Acp, Protocol::Claude] {
        let (_dir, mut runtime, opts) = super::rig(protocol, ApprovalMode::Deny);
        if protocol == Protocol::Acp {
            runtime.id = "grok".into();
        }
        let session = runtime.open(opts).await?;
        let root = session.model();
        let cursor = session.records().last().unwrap().cursor();
        assert_eq!(
            session.prompt("child-selections").await?.wait().await?,
            Outcome::Completed
        );
        let mut view = SessionView::default();
        let mut models = Vec::new();
        for r in session.records() {
            view.apply(&r)?;
            if r.seq <= cursor.after_seq {
                continue;
            }
            for event in &r.events {
                if let Event::Model { model } = event {
                    models.push((r.session_id.clone(), r.agent_path.clone(), model.clone()));
                }
            }
        }
        if protocol == Protocol::Claude {
            let expected = [
                (vec!["first"], "small"),
                (vec![], "root"),
                (vec!["second"], "small"),
                (vec!["first", "nested"], "small"),
                (vec!["first"], "large"),
            ];
            assert_eq!(
                models,
                expected.map(|(path, model)| (
                    Some(session.id().to_owned()),
                    path.into_iter().map(str::to_owned).collect(),
                    model.to_owned()
                ))
            );
            assert_eq!(session.model().as_deref(), Some("root"));
        } else {
            let tasks = session.tasks();
            assert_eq!(tasks.len(), 2);
            for t in &tasks {
                assert_eq!(
                    t.details.model.as_deref(),
                    match t.id.as_str() {
                        "child" => Some("initial"),
                        "sibling" => None,
                        id => panic!("unexpected child {id}"),
                    }
                );
            }
            assert_eq!(session.model(), root);
            if protocol == Protocol::Codex {
                assert_eq!(
                    models,
                    ["selected", "later"].map(|m| (Some("child".into()), vec![], m.into()))
                );
                let child = &view.agents[&kar::observe::Agent {
                    session_id: Some("child".into()),
                    path: vec![],
                }];
                assert_eq!(child.model.as_deref(), Some("later"));
                assert_eq!(child.effort.as_deref(), Some("medium"));
            } else {
                assert!(models.is_empty());
            }
        }
        session.close().await?;
    }
    Ok(())
}
