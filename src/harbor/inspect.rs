//! Read-only channel, status, journal watch, findings, and voyage trace helpers for Harbor.

use super::store::{HarborMessage, HarborStore, JournalEntry, format_channel_message};
use crate::{Error, Result, voyage::import_records_or_voyage};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

/// Summary row returned by `GET /status` (`kar harbor status`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentStatusRow {
    pub name: String,
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: String,
    pub opens: u64,
    pub turns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_tokens: Option<u64>,
    pub pending: usize,
    pub awaiting_reply: bool,
}

/// Read up to `limit` channel messages strictly after `after_seq`.
pub fn read_messages(
    store: &HarborStore,
    after_seq: u64,
    limit: usize,
) -> Result<Vec<HarborMessage>> {
    let all: Vec<HarborMessage> = store
        .messages()?
        .into_iter()
        .filter(|m| m.seq > after_seq)
        .collect();
    if limit == 0 || all.len() <= limit {
        return Ok(all);
    }
    if after_seq == 0 {
        Ok(all[all.len() - limit..].to_vec())
    } else {
        Ok(all.into_iter().take(limit).collect())
    }
}

pub fn format_messages(messages: &[HarborMessage]) -> String {
    messages
        .iter()
        .map(format_channel_message)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_status_table(rows: &[AgentStatusRow]) -> String {
    if rows.is_empty() {
        return "(no agents registered)".into();
    }
    let mut lines = vec![format!(
        "{:<16} {:<16} {:<14} {:>6} {:>7} {:>10} {:>10} {:>7}",
        "AGENT", "RUNTIME", "STATE", "OPENS", "TURNS", "CONTEXT", "DAY_TOK", "PENDING"
    )];
    for row in rows {
        let rt = match &row.model {
            Some(m) => format!("{}:{m}", row.runtime),
            None => row.runtime.clone(),
        };
        let state = match row.active_turn_ms {
            Some(ms) => format!("{}({}s)", row.state, ms / 1000),
            None => row.state.clone(),
        };
        let ctx = row
            .context_tokens
            .map_or_else(|| "-".into(), |n| n.to_string());
        let day = row.day_tokens.map_or_else(|| "-".into(), |n| n.to_string());
        lines.push(format!(
            "{:<16} {:<16} {:<14} {:>6} {:>7} {:>10} {:>10} {:>7}",
            row.name, rt, state, row.opens, row.turns, ctx, day, row.pending
        ));
    }
    lines.join("\n")
}

pub fn format_journal_entry(entry: &JournalEntry) -> String {
    let agent = entry.agent.as_deref().unwrap_or("-");
    let details = serde_json::to_string(&entry.details).unwrap_or_else(|_| "{}".into());
    format!(
        "[#{} ts={} agent={} kind={}] {}",
        entry.seq, entry.timestamp_ms, agent, entry.kind, details
    )
}

/// Filter recorded findings from `journal.jsonl` (optionally scoped to one `agent`).
pub fn findings_for(store: &HarborStore, agent: Option<&str>) -> Result<Vec<JournalEntry>> {
    Ok(store
        .journal_entries()?
        .into_iter()
        .filter(|e| e.kind == "finding" && agent.is_none_or(|a| e.agent.as_deref() == Some(a)))
        .collect())
}

/// Follow `journal.jsonl` (either findings only or `--all` entries) until interrupted.
pub async fn watch_journal(store: &HarborStore, include_all: bool) -> Result<()> {
    let mut seen_seq = 0u64;
    loop {
        for entry in store.journal_entries()? {
            if entry.seq <= seen_seq {
                continue;
            }
            seen_seq = entry.seq;
            if include_all || entry.kind == "finding" {
                println!("{}", format_journal_entry(&entry));
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
    }
    Ok(())
}

/// Options for `kar harbor trace`.
#[derive(Clone, Debug, Default)]
pub struct TraceOptions {
    pub agent: Option<String>,
    pub session: Option<u64>,
    pub around_seq: Option<u64>,
    pub turn: Option<usize>,
    pub voyage_path: Option<std::path::PathBuf>,
    pub raw: bool,
}

/// Inspect records from an agent's `.voyage.jsonl` recording around a sequence or turn.
pub fn trace_voyage(store: &HarborStore, opts: &TraceOptions) -> Result<String> {
    let target_path = if let Some(explicit) = &opts.voyage_path {
        if explicit.is_absolute() {
            explicit.clone()
        } else {
            store.dir().join(explicit)
        }
    } else {
        let agent = opts
            .agent
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Refused("specify `<agent>` or `--voyage <path>`".into()))?;
        let state = store.load_state()?;
        let session_num = opts
            .session
            .or_else(|| state.agents.get(agent).map(|a| a.opens))
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                Error::Refused(format!("no recorded sessions found for agent {agent:?}"))
            })?;
        store.voyage_path(agent, session_num)
    };

    render_voyage_trace(&target_path, opts)
}

fn render_voyage_trace(path: &Path, opts: &TraceOptions) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    let summary = import_records_or_voyage(std::io::Cursor::new(content.as_bytes()), |rec| {
        records.push(rec);
        Ok(())
    })?;
    let session_id = summary
        .header
        .get("sessionId")
        .and_then(|v| v.as_str())
        .or_else(|| records.iter().find_map(|r| r.session_id.as_deref()));

    let selected: Vec<_> = if let Some(center) = opts.around_seq {
        let lo = center.saturating_sub(10);
        let hi = center.saturating_add(10);
        records
            .iter()
            .filter(|r| r.seq >= lo && r.seq <= hi)
            .collect()
    } else if let Some(turn_idx) = opts.turn {
        let mut current_turn = 0usize;
        let mut slice = Vec::new();
        for rec in &records {
            if rec.events.iter().any(|e| {
                matches!(
                    e,
                    crate::Event::PromptDispatched | crate::Event::TurnStarted
                )
            }) && (slice.is_empty() || current_turn != turn_idx)
            {
                current_turn += 1;
            }
            if current_turn == turn_idx {
                slice.push(rec);
            }
        }
        slice
    } else {
        let skip = records.len().saturating_sub(25);
        records.iter().skip(skip).collect()
    };

    let mut out = vec![format!(
        "voyage={} session={:?} total_records={} showing={}",
        path.display(),
        session_id,
        records.len(),
        selected.len()
    )];
    for rec in selected {
        if opts.raw {
            out.push(serde_json::to_string(rec)?);
        } else {
            let ev_summary: Vec<String> = rec
                .events
                .iter()
                .map(|ev| {
                    serde_json::to_value(ev)
                        .ok()
                        .and_then(|v| v["kind"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| format!("{ev:?}"))
                })
                .collect();
            out.push(format!(
                "  seq={} dir={:?} kind={:?} frame={:?} events=[{}]",
                rec.seq,
                rec.direction,
                rec.kind,
                rec.frame_type(),
                ev_summary.join(", ")
            ));
        }
    }
    Ok(out.join("\n"))
}
