use super::*;
fn parse(
    v: &Value,
    id: Option<&str>,
    session: Option<&str>,
    created: Option<i64>,
) -> Option<Entry> {
    if v["role"].as_str().is_some_and(|s| s != "assistant") {
        return None;
    }
    let model = v["model"]["id"]
        .as_str()
        .or(v["model"]["modelID"].as_str())
        .or(v["modelID"].as_str())?
        .trim();
    if model.is_empty() {
        return None;
    }
    let provider = v["model"]["providerID"]
        .as_str()
        .or(v["providerID"].as_str())
        .unwrap_or("");
    let u = &v["tokens"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let r = n(&u["reasoning"]);
    let tokens = Totals::from_array([
        n(&u["input"]),
        n(&u["cache"]["read"]),
        n(&u["cache"]["write"]),
        n(&u["output"]).saturating_add(r),
        r,
    ]);
    if tokens.sum() == 0 {
        return None;
    }
    let free = matches!(provider, "opencode" | "opencode-go") && model.ends_with("-free");
    Some(Entry {
        timestamp: v["time"]["created"].as_i64().or(created)?,
        provider: "opencode".into(),
        model: model.into(),
        session: session.or(v["sessionID"].as_str()).unwrap_or("").into(),
        tokens,
        cost: v["cost"].as_f64().filter(|v| *v > 0. || (*v == 0. && free)),
        rate_model: (provider == "opencode-go" && !model.contains('/'))
            .then(|| format!("opencode_go/{model}")),
        key: id
            .or(v["id"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        ..Default::default()
    })
}
pub(in super::super) fn legacy(path: &Path) -> io::Result<Vec<Entry>> {
    let value = serde_json::from_slice(&bounded(path, 32 << 20)?)?;
    Ok(parse(&value, None, None, None).into_iter().collect())
}
#[derive(Clone)]
struct Fork {
    parent: String,
    through: bool,
    boundary: String,
    created: i64,
}
pub(super) fn rows(db: &Tables) -> io::Result<Vec<Entry>> {
    if !db.tables.contains_key("message") && !db.tables.contains_key("session_message") {
        return Err(invalid("OpenCode database has no supported message table"));
    }
    let mut forks: BTreeMap<String, Fork> = BTreeMap::new();
    db.read("session_v2", |c, row| {
        if let (Some(id), Some(parent), Some(raw), Some(created)) = (
            c.text(&row, "id"),
            c.text(&row, "fork_session_id"),
            c.text(&row, "fork_boundary"),
            c.int(&row, "time_created"),
        ) && let Ok(b) = serde_json::from_str::<Value>(raw)
            && let Some(kind @ ("through" | "before")) = b["type"].as_str()
            && let Some(boundary) = b["messageID"].as_str()
        {
            forks.insert(
                id.into(),
                Fork {
                    parent: parent.into(),
                    through: kind == "through",
                    boundary: boundary.into(),
                    created,
                },
            );
        }
        Ok(())
    })?;
    let mut bounds = BTreeMap::new();
    let mut entries = Vec::new();
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in forks.values() {
        owners
            .entry(f.parent.clone())
            .or_default()
            .insert(f.boundary.clone());
    }
    for table in ["message", "session_message"] {
        db.read(table, |c, row| {
            let id = c.text(&row, "id");
            let session = c.text(&row, "session_id");
            let seq = c.int(&row, "seq");
            if table == "session_message"
                && let (Some(id), Some(session), Some(seq)) = (id, session, seq)
                && owners.get(session).is_some_and(|m| m.contains(id))
            {
                bounds.insert((session.to_owned(), id.to_owned()), seq);
            }
            if table == "session_message"
                && !matches!(c.text(&row, "type"), Some("assistant" | "compaction"))
            {
                return Ok(());
            }
            if let Some(data) = c.text(&row, "data")
                && let Ok(value) = serde_json::from_str::<Value>(data)
                && let Some(r) = parse(&value, id, session, c.int(&row, "time_created"))
            {
                entries.push((r, seq));
            }
            Ok(())
        })?;
    }
    // v2 fork copies have new message IDs. Preserve the native ID for duplicate v1/legacy
    // stores, and retain a single owner for a lineage only when its boundary still exists.
    let mut seen = BTreeSet::new();
    let mut chosen = BTreeSet::new();
    let mut result = Vec::new();
    entries.sort_by_key(|(r, _)| forks.contains_key(&r.session));
    for (e, seq) in entries {
        if let Some(seq) = seq {
            let mut session = &e.session;
            let mut ancestors = BTreeSet::new();
            while let Some(f) = forks.get(session) {
                if !ancestors.insert(session) || e.timestamp >= f.created {
                    break;
                }
                let Some(boundary) = bounds.get(&(f.parent.clone(), f.boundary.clone())) else {
                    break;
                };
                if seq > *boundary || (seq == *boundary && !f.through) {
                    break;
                }
                session = &f.parent;
            }
            if !seen.insert((session.clone(), seq)) {
                continue;
            }
        }
        if e.key.as_ref().is_none_or(|k| chosen.insert(k.clone())) {
            result.push(e);
        }
    }
    Ok(result)
}
