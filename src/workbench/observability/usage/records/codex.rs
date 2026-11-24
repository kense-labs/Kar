use super::*;
// Keep native cumulative and total fields intact for reconciliation, including context-only markers.
type Counter = [u64; 6];
fn read(v: &Value) -> Option<Counter> {
    let input = v["input_tokens"].as_u64()?;
    let output = v["output_tokens"].as_u64()?;
    let field = |k: &str| v.get(k).map_or(Some(0), Value::as_u64);
    let c = [
        input,
        field("cached_input_tokens")?,
        field("cache_write_input_tokens")?,
        output,
        field("reasoning_output_tokens")?,
        v.get("total_tokens")
            .map_or(input.checked_add(output), Value::as_u64)?,
    ];
    valid(c).then_some(c)
}
fn valid(c: Counter) -> bool {
    c[1].checked_add(c[2]).is_some_and(|s| s <= c[0])
        && c[4] <= c[3]
        && c[0].checked_add(c[3]).is_some_and(|x| x <= c[5])
}
fn plus(a: Counter, b: Counter) -> Counter {
    std::array::from_fn(|i| a[i].saturating_add(b[i]))
}
fn minus(a: Counter, b: Counter) -> Option<Counter> {
    let mut c = [0; 6];
    for i in 0..6 {
        c[i] = a[i].checked_sub(b[i])?;
    }
    valid(c).then_some(c)
}
fn totals(c: Counter) -> Totals {
    inclusive(c[0], c[1], c[2], c[3], c[4])
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct State {
    model_provider: String,
    turn: String,
    saw_settings: bool,
    session: String,
    model: String,
    speed: String,
    meta: bool,
    ledger: bool,
    fork_at: Option<i64>,
    baseline: Option<Counter>,
    baseline_time: Option<i64>,
    last: Option<Counter>,
    start: Option<(i64, Counter)>,
    pending: Vec<(Entry, Counter)>,
    pub malformed: u64,
}
impl State {
    fn swallowed(&mut self, t: i64) -> bool {
        if self.fork_at.is_some_and(|f| t.saturating_sub(f) < 1000) {
            self.fork_at = Some(t);
            return true;
        }
        if self.fork_at.take().is_some() {
            self.last = None;
        }
        false
    }
    fn record(&self, t: i64, c: Counter) -> Entry {
        let mut r = entry("codex", &self.session, &self.model, t, totals(c));
        r.speed.clone_from(&self.speed);
        if !self.saw_settings
            && self.model_provider == "openai"
            && !self.session.is_empty()
            && !self.turn.is_empty()
        {
            r.tier_turn = Some(self.turn.clone());
        }
        r
    }
    pub fn parse(&mut self, v: &Value) -> Vec<Entry> {
        let p = &v["payload"];
        let kind = string(&v["type"]);
        let time = millis(&v["timestamp"]);
        match kind {
            "session_meta" if !self.meta => {
                self.meta = true;
                self.model_provider = string(&p["model_provider"]).into();
                self.session = p["id"]
                    .as_str()
                    .or(p["session_id"].as_str())
                    .unwrap_or("")
                    .into();
                if p["forked_from_id"].is_string()
                    || p.pointer("/source/subagent/thread_spawn/parent_thread_id")
                        .is_some_and(Value::is_string)
                {
                    self.fork_at = time;
                }
                return vec![];
            }
            "turn_context" => {
                self.ledger = false;
                self.turn = if p["model"].as_str().is_some_and(|s| !s.is_empty()) {
                    string(&p["turn_id"]).into()
                } else {
                    String::new()
                };
                if let Some(model) = p["model"].as_str() {
                    self.model = model.into();
                }
                return vec![];
            }
            _ => {}
        }
        if p["type"] == "thread_settings_applied" {
            self.saw_settings = true;
            self.speed = match p["thread_settings"]["service_tier"].as_str() {
                Some("priority" | "fast") => "fast",
                Some("ultrafast") => "ultrafast",
                _ => "standard",
            }
            .into();
            return vec![];
        }
        let Some(t) = time.filter(|_| !self.model.is_empty()) else {
            return vec![];
        };
        if kind == "token_usage_record" {
            let Some(c) = read(&p["usage"]) else {
                return vec![];
            };
            if self.swallowed(t) {
                return vec![];
            }
            let mut r = self.record(t, c);
            let Some(id) = p["response_id"].as_str().filter(|s| !s.is_empty()) else {
                self.ledger = false;
                self.pending.push((r, c));
                return vec![];
            };
            self.ledger = true;
            let mut rows = Vec::new();
            let mut base = self.baseline.unwrap_or_default();
            for (row, item) in self.pending.drain(..) {
                rows.push(row);
                base = plus(base, item);
            }
            self.baseline = Some(plus(base, c));
            self.baseline_time = Some(self.baseline_time.unwrap_or(t).max(t));
            r.key = Some(json!([self.session, id]).to_string());
            rows.push(r);
            return rows;
        }
        if p["type"] != "token_count" || !p["info"].is_object() {
            return vec![];
        }
        let info = &p["info"];
        let last = read(&info["last_token_usage"]);
        let raw = &info["total_token_usage"];
        let total = read(raw);
        let old = self.baseline;
        let old_time = self.baseline_time;
        let copied = self.swallowed(t);
        if let Some(sum) = total {
            self.baseline = Some(sum);
            self.baseline_time = Some(old_time.unwrap_or(t).max(t));
            if sum[0] == 0 && sum[3] == 0 {
                self.start = None;
            }
        }
        if copied {
            if total.is_none()
                && raw.is_null()
                && last != self.last
                && let Some(c) = last
            {
                self.baseline = Some(plus(old.unwrap_or_default(), c));
            }
            self.last = last;
            self.pending.clear();
            return vec![];
        }
        if self.ledger {
            self.pending.clear();
            return vec![];
        }
        let mut pending = std::mem::take(&mut self.pending);
        let Some(last) = last.filter(|_| raw.is_null() || total.is_some()) else {
            self.malformed += 1;
            return vec![];
        };
        if last[0] == 0 && last[3] == 0 {
            self.last = None;
            return vec![];
        }
        if total.is_some() && total == old {
            return vec![];
        }
        if last[0].checked_add(last[3]) != Some(last[5]) {
            self.malformed += 1;
            return vec![];
        }
        let mut result = Vec::new();
        if let Some(sum) = total {
            let restarted = sum == last;
            if restarted && self.start == Some((t, last)) {
                self.baseline = old;
                self.baseline_time = old_time;
                return vec![];
            }
            let lowered = old.is_some_and(|a| a.iter().zip(sum).any(|(x, y)| *x > y));
            let rewound = lowered && old_time.is_some_and(|p| t > p);
            let reconciled = if old.is_none() || rewound {
                minus(sum, last).is_some()
            } else {
                minus(sum, old.unwrap()) == Some(last)
            };
            if !restarted && !reconciled {
                let bridge = pending
                    .last()
                    .and_then(|(_, c)| minus(*c, last))
                    .filter(|_| {
                        old.is_some()
                            && minus(sum, old.unwrap())
                                == Some(pending.iter().map(|(_, c)| *c).fold([0; 6], plus))
                    });
                if let Some(remainder) = bridge {
                    if let Some((r, _)) = pending.last_mut() {
                        r.tokens = totals(remainder);
                    }
                    result.extend(
                        pending
                            .into_iter()
                            .map(|(r, _)| r)
                            .filter(|r| r.tokens.sum() > 0),
                    );
                } else {
                    if old_time.is_some_and(|p| t < p || (t == p && lowered)) {
                        self.baseline = old;
                        self.baseline_time = old_time;
                        self.pending = pending;
                    }
                    self.malformed += 1;
                    return vec![];
                }
            } else if rewound && !restarted {
                self.malformed += pending
                    .len()
                    .saturating_sub(usize::from(pending.last().is_some_and(|(_, c)| *c == last)))
                    as u64;
            }
            if restarted {
                self.start = Some((t, last));
            }
        } else {
            if self.last == Some(last) {
                return vec![];
            }
            self.baseline = Some(plus(old.unwrap_or_default(), last));
            self.baseline_time = Some(old_time.unwrap_or(t).max(t));
        }
        self.last = Some(last);
        result.push(self.record(t, last));
        result
    }
}
