use super::*;
pub(super) async fn call(
    app: &Workbench,
    c: &Caller,
    p: Uuid,
    t: Uuid,
    name: &str,
    input: Value,
) -> Result<Value> {
    match name {
        "kar_thread_configure" => {
            let i = app.binding(p, t)?;
            Ok(
                json!({"runtime":i.runtime,"model":i.model,"settings":i.settings,"approvals":i.approvals}),
            )
        }
        "kar_thread_search" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                query: String,
                cursor: Option<usize>,
                limit: Option<usize>,
            }
            let a: Args = parse(input)?;
            let q = a.query.trim().to_lowercase();
            if q.is_empty() || q.len() > 2048 {
                return Err(Error::Refused("invalid search query".into()));
            }
            let mut rows = vec![];
            for chat in app
                .chats(p, false)?
                .into_iter()
                .filter(|ch| visible(&app.0.catalog.lock().unwrap(), c, p, ch.info.id))
            {
                let title = chat.metadata.title.as_deref().unwrap_or("");
                let h = app.0.chats.history(chat.info.id)?;
                let first = h.turns.first().and_then(|t| t.prompt()).unwrap_or("");
                let branch = crate::workbench_git::Repository::open(&chat.info.workspace)
                    .ok()
                    .and_then(|r| r.status().ok()?.branch);
                let hit=if title.to_lowercase().contains(&q) || first.to_lowercase().contains(&q){"title"}else if branch.as_deref().is_some_and(|b|b.to_lowercase().contains(&q)){"branch"}else if h.turns.iter().flat_map(|t|t.items.iter()).any(|i|matches!(&i.item,crate::chat::Item::Message{text,..} if text.to_lowercase().contains(&q))){"message"}else{continue};
                rows.push(json!({"thread_id":chat.info.id,"project_id":p,"matched":hit,"title":title,"branch":branch}));
            }
            toolkit::page(rows, "threads", a.cursor.unwrap_or(0), a.limit)
        }
        "kar_thread_wait" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                after: Option<Value>,
                timeout_ms: Option<u64>,
            }
            let a: Args = parse(input)?;
            let ms = a.timeout_ms.unwrap_or(10000);
            if ms > 30000 {
                return Err(Error::Refused("timeout exceeds 30 seconds".into()));
            }
            let stop = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
            loop {
                target(app, c, Some(p), Some(t), false)?;
                let snapshot = app.0.chats.feed_snapshot(t)?;
                let h = snapshot.history;
                let cursor = json!(snapshot.cursor);
                let changed = a.after.as_ref() != Some(&cursor);
                if changed || tokio::time::Instant::now() >= stop {
                    return Ok(
                        json!({"thread_id":t,"cursor":cursor,"changed":changed,"latest_turn":h.turns.last().map(|r|json!({"id":r.id,"status":r.status}))}),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        _ => {
            let questions = app
                .active_control(p, t)?
                .map(|s| s.snapshot().pending_requests)
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.kind == crate::RequestKind::Question)
                .collect::<Vec<_>>();
            if input["action"] == "list" {
                return Ok(json!({"requests":questions}));
            }
            questions
                .into_iter()
                .find(|r| json!(r.cursor) == input["cursor"] && r.id == input["id"])
                .map(|r| json!({"request":r}))
                .ok_or_else(|| Error::Refused("question occurrence is no longer pending".into()))
        }
    }
}
