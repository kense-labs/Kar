//! UTC daily token budget gate (`--day-tokens`), idle/context session recycling
//! (`--recycle-idle-min` / `--recycle-context`) with ~10K-token channel handoff,
//! and workspace disk cap (`--max-disk-gb`) for Harbor.

use super::store::{DayTokenCount, HarborMessage, format_channel_message};
use crate::{Result, SessionUsage};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, time::Duration};

const DAY_MS: u64 = 86_400_000;

/// Approximately 10K tokens (~40,000 UTF-8 characters) of recent channel handoff context.
pub const HANDOFF_MAX_CHARS: usize = 40_000;
const TRUNCATED_SUFFIX: &str = " [truncated; `kar harbor read` shows full message]";

/// Policy governing automatic session replacement when an idle session's context grows past threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecyclePolicy {
    pub idle_ms: u64,
    pub context_tokens: u64,
}

impl RecyclePolicy {
    /// Construct from CLI `--recycle-idle-min` (default 60; `0` disables recycling)
    /// and `--recycle-context` (default 128,000 tokens).
    pub fn from_limits(idle_min: u64, context_tokens: u64) -> Option<Self> {
        if idle_min == 0 {
            None
        } else {
            Some(Self {
                idle_ms: idle_min.saturating_mul(60_000),
                context_tokens: context_tokens.max(1),
            })
        }
    }
}

/// Returns `true` when an idle session has exceeded both `policy.idle_ms` and `policy.context_tokens`.
pub fn should_recycle(
    policy: Option<RecyclePolicy>,
    idle_ms: Option<u64>,
    context_tokens: Option<u64>,
) -> bool {
    let (Some(policy), Some(idle), Some(ctx)) = (policy, idle_ms, context_tokens) else {
        return false;
    };
    idle > policy.idle_ms && ctx > policy.context_tokens
}

fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build a bounded (~10K-token / `max_chars`) channel handoff containing the newest messages
/// sent or received by `agent_name` strictly before `before_seq`, ordered oldest-first.
pub fn build_handoff(
    messages: &[HarborMessage],
    agent_name: &str,
    before_seq: u64,
    max_chars: usize,
) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    let mut selected_rev: Vec<String> = Vec::new();
    let mut used = 0usize;

    for msg in messages.iter().rev() {
        if msg.seq >= before_seq {
            continue;
        }
        let relevant = msg.from == agent_name || msg.to.iter().any(|t| t == agent_name);
        if !relevant {
            continue;
        }
        let formatted = format_channel_message(msg);
        let line = if selected_rev.is_empty() && formatted.len() > max_chars {
            let budget = max_chars.saturating_sub(TRUNCATED_SUFFIX.len());
            format!(
                "{}{TRUNCATED_SUFFIX}",
                truncate_utf8_prefix(&formatted, budget)
            )
        } else {
            formatted
        };
        if !selected_rev.is_empty() && used.saturating_add(line.len()) > max_chars {
            break;
        }
        used = used.saturating_add(line.len()).saturating_add(1);
        selected_rev.push(line);
    }

    if selected_rev.is_empty() {
        return None;
    }
    selected_rev.reverse();

    let mut out = String::from(
        "(harbor: your previous session sat idle with a large context, so this is a fresh session. \
These are the latest channel messages you sent or received, oldest first; run `kar harbor read` for \
older history. Files you wrote remain in your working directory.)\n",
    );
    for line in selected_rev {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("(harbor: end of earlier channel handoff; new messages follow.)");
    Some(out)
}

/// Total input + output tokens reported for a session (including linked child sessions when available).
pub fn session_reported_tokens(usage: &SessionUsage) -> u64 {
    let Some(totals) = usage.with_children.or(usage.total) else {
        return 0;
    };
    let sum = totals.input.saturating_add(totals.output).max(0);
    u64::try_from(sum).unwrap_or(0)
}

/// Format `now_ms` (Unix epoch milliseconds) as a canonical `YYYY-MM-DD` UTC calendar date.
pub fn utc_day_string(now_ms: u64) -> String {
    let days = (now_ms / DAY_MS) as i64;
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Milliseconds remaining until `00:00:00.000Z` of the next UTC calendar day.
pub fn ms_until_next_utc_day(now_ms: u64) -> u64 {
    let rem = now_ms % DAY_MS;
    if rem == 0 { DAY_MS } else { DAY_MS - rem }
}

/// Add `delta_tokens` into the current UTC day's token ledger, rolling over cleanly at UTC midnight.
pub fn add_day_tokens(
    current: Option<&DayTokenCount>,
    delta_tokens: u64,
    now_ms: u64,
) -> DayTokenCount {
    let today = utc_day_string(now_ms);
    let base = match current {
        Some(c) if c.day == today => c.tokens,
        _ => 0,
    };
    DayTokenCount {
        day: today,
        tokens: base.saturating_add(delta_tokens),
    }
}

/// Returns `true` when `max_day_tokens` is configured and `current` has reached or exceeded it today (UTC).
pub fn day_exhausted(
    current: Option<&DayTokenCount>,
    max_day_tokens: Option<u64>,
    now_ms: u64,
) -> bool {
    let (Some(c), Some(limit)) = (current, max_day_tokens) else {
        return false;
    };
    c.day == utc_day_string(now_ms) && c.tokens >= limit
}

/// Per-agent runtime tracker for daily token budgeting and idle-context session recycling.
#[derive(Clone, Debug)]
pub struct AgentBudget {
    pub policy: Option<RecyclePolicy>,
    pub day_limit: Option<u64>,
    pub opened_at_ms: u64,
    pub last_turn_ended_ms: Option<u64>,
    pub counted_session_tokens: u64,
}

impl AgentBudget {
    pub fn new(policy: Option<RecyclePolicy>, day_limit: Option<u64>, now_ms: u64) -> Self {
        Self {
            policy,
            day_limit,
            opened_at_ms: now_ms,
            last_turn_ended_ms: None,
            counted_session_tokens: 0,
        }
    }

    pub fn on_session_opened(&mut self, initial_usage: &SessionUsage, now_ms: u64) {
        self.opened_at_ms = now_ms;
        self.last_turn_ended_ms = None;
        self.counted_session_tokens = session_reported_tokens(initial_usage);
    }

    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        let anchor = self.last_turn_ended_ms.unwrap_or(self.opened_at_ms);
        now_ms.saturating_sub(anchor)
    }

    pub fn should_recycle_now(&self, context_tokens: Option<u64>, now_ms: u64) -> bool {
        should_recycle(self.policy, Some(self.idle_ms(now_ms)), context_tokens)
    }

    /// Account any newly reported session tokens into `current_day` and return `(updated, delta)`.
    pub fn record_usage(
        &mut self,
        usage: &SessionUsage,
        current_day: Option<&DayTokenCount>,
        now_ms: u64,
    ) -> (DayTokenCount, u64) {
        let total = session_reported_tokens(usage);
        let delta = total.saturating_sub(self.counted_session_tokens);
        self.counted_session_tokens = self.counted_session_tokens.max(total);
        let updated = add_day_tokens(current_day, delta, now_ms);
        (updated, delta)
    }

    pub fn on_turn_ended(
        &mut self,
        usage: &SessionUsage,
        current_day: Option<&DayTokenCount>,
        now_ms: u64,
    ) -> (DayTokenCount, u64) {
        self.last_turn_ended_ms = Some(now_ms);
        self.record_usage(usage, current_day, now_ms)
    }

    pub fn is_day_paused(
        &self,
        current_day: Option<&DayTokenCount>,
        now_ms: u64,
    ) -> Option<Duration> {
        if day_exhausted(current_day, self.day_limit, now_ms) {
            Some(Duration::from_millis(ms_until_next_utc_day(now_ms)))
        } else {
            None
        }
    }
}

/// Recursively compute total regular-file byte size under `root` (never following symlinks).
pub fn directory_bytes(root: &Path) -> Result<u64> {
    let meta = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Ok(0);
    }
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }

    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file()
                && let Ok(m) = entry.metadata()
            {
                total = total.saturating_add(m.len());
            }
        }
    }
    Ok(total)
}

/// Check whether `root` exceeds `max_disk_bytes`, returning `Some(measured_bytes)` when over budget.
pub fn exceeds_disk_limit(root: &Path, max_disk_bytes: Option<u64>) -> Result<Option<u64>> {
    let Some(limit) = max_disk_bytes else {
        return Ok(None);
    };
    let used = directory_bytes(root)?;
    Ok((used > limit).then_some(used))
}
