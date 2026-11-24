use super::*;
mod codex;

fn string(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0).min(i64::MAX as u64)
}
fn millis(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_str()?
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|v| v.as_millisecond())
    })
}
fn entry(provider: &str, session: &str, model: &str, timestamp: i64, tokens: Totals) -> Entry {
    Entry {
        provider: provider.into(),
        session: session.into(),
        model: model.into(),
        timestamp,
        tokens,
        ..Default::default()
    }
}
fn inclusive(input: u64, read: u64, write: u64, output: u64, reason: u64) -> Totals {
    let read = read.min(input);
    let write = write.min(input - read);
    Totals::from_array([
        input - read - write,
        read,
        write,
        output,
        reason.min(output),
    ])
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Parser {
    tier: Option<String>,
    pub tiers: BTreeMap<String, Option<String>>,
    session: String,
    model: String,
    provider: String,
    clock: i64,
    streams: BTreeMap<String, (u64, Totals)>,
    codex: codex::State,
}
impl Parser {
    pub fn malformed(&self) -> u64 {
        self.codex.malformed
    }
    pub fn parse(&mut self, format: &str, v: &Value) -> Vec<Entry> {
        match format {
            "kar" => self.kar(v),
            "claude" => claude(v).unwrap_or_default(),
            "codex" => self.codex.parse(v),
            "pi" => self.pi(v).into_iter().collect(),
            "muse" => muse(v).into_iter().collect(),
            "grok" => grok(v).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
    fn kar(&mut self, v: &Value) -> Vec<Entry> {
        let kind = string(&v["kind"]);
        if v["format"] == "kar-chat/1" || kind == "handoff_ready" {
            let chat = if kind == "handoff_ready" {
                &v["target"]
            } else {
                &v["chat"]
            };
            self.provider = string(&chat["runtime"]).into();
            self.tier = match chat["settings"]["service_tier"].as_str() {
                None | Some("standard" | "default") => Some("standard".into()),
                Some("fast" | "priority") => Some("fast".into()),
                Some("ultrafast") => Some("ultrafast".into()),
                _ => None,
            };
            self.session = string(&chat["native_id"]).into();
            self.model = chat["model"].as_str().unwrap_or("unknown").into();
            if let Some(t) = millis(&chat["created_ms"]) {
                self.clock = t;
            }
        }
        if let Some(t) = millis(&v["at"]) {
            self.clock = t;
        }
        if kind == "handoff_target_reopened" {
            self.session = string(&v["native_id"]).into();
        }
        let turn = match kind {
            "native_begin" => v["native_turn"].as_str(),
            "frame" | "reconciled"
                if v["frame"]["source"]["session_id"]
                    .as_str()
                    .is_none_or(|s| s == self.session) =>
            {
                v["frame"]["native_turn"].as_str()
            }
            _ => None,
        };
        if self.provider == "codex"
            && self.model != "unknown"
            && !self.session.is_empty()
            && let (Some(turn), Some(tier)) = (turn, &self.tier)
        {
            let key = json!([self.session, turn, self.model]).to_string();
            self.tiers
                .entry(key)
                .and_modify(|v| {
                    if v.as_deref() != Some(tier) {
                        *v = None;
                    }
                })
                .or_insert_with(|| Some(tier.clone()));
        }
        if kind != "usage_changed" || self.provider.is_empty() {
            return Vec::new();
        }
        let reading = &v["reading"];
        let Some(stream) = reading["cursor"]["stream_id"].as_str() else {
            return Vec::new();
        };
        let Some(seq) = reading["cursor"]["after_seq"].as_u64() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let own = std::iter::once((self.session.as_str(), &reading["usage"]["total"]));
        let children = reading["children"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(id, u)| (id.as_str(), &u["total"]));
        for (session, v) in own.chain(children) {
            if !v.is_object() {
                continue;
            }
            let current = inclusive(
                n(&v["input"]),
                n(&v["cache_read"]),
                n(&v["cache_write"]),
                n(&v["output"]),
                n(&v["reasoning"]),
            );
            let state = self
                .streams
                .entry(format!("{stream}/{session}"))
                .or_insert((0, Totals::default()));
            if seq <= state.0 {
                continue;
            }
            let delta = current.zip(state.1, u64::saturating_sub);
            *state = (seq, current);
            if delta.sum() > 0 {
                let mut r = entry(&self.provider, session, &self.model, self.clock, delta);
                if session == self.session {
                    r.speed = self.tier.clone().unwrap_or_default();
                }
                r.key = Some(format!("stream:{stream}/{session}/{seq}"));
                rows.push(r);
            }
        }
        rows
    }
    fn pi(&mut self, v: &Value) -> Option<Entry> {
        let kind = v["type"].as_str()?;
        if kind == "session" {
            self.session = string(&v["id"]).into();
            return None;
        }
        let src = match kind {
            "message"
                if matches!(
                    v["message"]["role"].as_str(),
                    Some("assistant" | "toolResult")
                ) =>
            {
                &v["message"]
            }
            "usage" | "compaction" | "branch_summary" => v,
            _ => return None,
        };
        let usage = src.get("usage")?;
        let rate = src["responseModel"].as_str().or(src["model"].as_str());
        let model = match (src["provider"].as_str(), rate) {
            (Some(p), Some(m)) => format!("{p}/{m}"),
            _ => "Tools/summaries".into(),
        };
        let output = n(&usage["output"]);
        let mut row = entry(
            "pi",
            &self.session,
            &model,
            millis(&v["timestamp"])?,
            Totals::from_array([
                n(&usage["input"]),
                n(&usage["cacheRead"]),
                n(&usage["cacheWrite"]),
                output,
                n(&usage["reasoning"]).min(output),
            ]),
        );
        row.rate_model = rate.map(str::to_string);
        row.cost = usage["cost"]["total"].as_f64().filter(|v| *v > 0.);
        row.key = Some(json!([v["id"].as_str()?, row.timestamp, model]).to_string());
        (row.cost.is_some() || row.tokens.sum() > 0).then_some(row)
    }
}
fn claude(v: &Value) -> Option<Vec<Entry>> {
    if v["type"] != "assistant" {
        return None;
    }
    let m = &v["message"];
    let u = m.get("usage")?;
    let session = string(&v["sessionId"]);
    let time = millis(&v["timestamp"])?;
    let make = |model: &str, usage: &Value| {
        let mut r = entry(
            "claude",
            session,
            model,
            time,
            Totals::from_array([
                n(&usage["input_tokens"]),
                n(&usage["cache_read_input_tokens"]),
                n(&usage["cache_creation_input_tokens"]),
                n(&usage["output_tokens"]),
                0,
            ]),
        );
        r.speed = string(&u["speed"]).into();
        r.one_hour = n(&usage["cache_creation"]["ephemeral_1h_input_tokens"]);
        r
    };
    let model = m["model"].as_str()?;
    if model == "<synthetic>" || model.is_empty() {
        return None;
    }
    let key = (v["requestId"].is_string() || m["id"].is_string())
        .then(|| json!([m["id"], v["requestId"]]).to_string());
    let mut row = make(model, u);
    row.key.clone_from(&key);
    row.cost = v["costUSD"].as_f64();
    let mut out = vec![row];
    if let Some(parts) = u["iterations"].as_array() {
        for (i, a) in parts.iter().enumerate() {
            if a["type"] == "advisor_message"
                && let Some(model) = a["model"]
                    .as_str()
                    .filter(|m| !m.is_empty() && *m != "<synthetic>")
            {
                let mut r = make(model, a);
                r.key = key.as_deref().map(|k| format!("{k}:advisor:{i}"));
                out.push(r);
            }
        }
    }
    Some(out)
}
fn muse(v: &Value) -> Option<Entry> {
    let e = &v["payload"]["event"];
    if v["schema_version"] != 1
        || v["record_type"] != "event"
        || v["payload_type"] != "runtime.session"
        || !matches!(v["payload_schema_version"].as_u64(), Some(1 | 3))
        || v["stream"]["kind"] != "session"
        || !matches!(
            e["kind"].as_str(),
            Some("model_completed" | "automated_review_completed")
        )
    {
        return None;
    }
    let u = e.get("usage")?;
    let t = inclusive(
        n(&u["input_tokens"]),
        n(u.get("cache_read_tokens")
            .or(u.get("cached_input_tokens"))
            .unwrap_or(&u["cached_tokens"])),
        n(&u["cache_write_tokens"]),
        n(&u["output_tokens"]),
        n(&u["reasoning_tokens"]),
    );
    let mut r = entry(
        "muse",
        v["stream"]["id"].as_str()?,
        e["model"].as_str().or(e["model"]["model_id"].as_str())?,
        v["recorded_at"].as_i64()? / 1000,
        t,
    );
    r.key = Some(
        v["payload"]["source_run_record_id"]
            .as_str()
            .or(v["id"].as_str())?
            .into(),
    );
    (t.sum() > 0).then_some(r)
}
fn grok(v: &Value) -> Option<Vec<Entry>> {
    let p = &v["params"];
    let update = &p["update"];
    if update["sessionUpdate"] != "turn_completed" {
        return None;
    }
    let usage = update.get("usage")?;
    let time = p["_meta"]["agentTimestampMs"].as_i64().or_else(|| {
        let t = v["timestamp"].as_f64()?;
        Some(if t > 1e12 { t } else { t * 1000. } as i64)
    })?;
    let ticks = |u: &Value| {
        u["costUsdTicks"]
            .as_f64()
            .filter(|t| *t >= 0.)
            .map(|t| t / 1e10)
    };
    let session = string(&p["sessionId"]);
    let make = |name: &str, u: &Value| {
        let t = inclusive(
            n(&u["inputTokens"]),
            n(&u["cachedReadTokens"]),
            n(&u["cacheCreationTokens"]),
            n(&u["outputTokens"]),
            n(&u["reasoningTokens"]),
        );
        let mut r = entry("grok", session, name, time, t);
        r.cost = ticks(u);
        r.key = update["prompt_id"]
            .as_str()
            .map(|id| json!([session, id, time, name]).to_string());
        r
    };
    let mut rows: Vec<_> = usage["modelUsage"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, u)| !name.is_empty() && u.is_object())
        .map(|(name, u)| make(name, u))
        .collect();
    if rows.is_empty() {
        rows.push(make("grok", usage));
    }
    rows.retain(|r| r.tokens.sum() > 0);
    if let Some(cost) = ticks(usage) {
        let remaining = (cost - rows.iter().filter_map(|r| r.cost).sum::<f64>()).max(0.);
        let denominator = rows
            .iter()
            .filter(|r| r.cost.is_none())
            .map(|r| r.tokens.sum() as f64)
            .sum::<f64>();
        for r in rows.iter_mut().filter(|r| r.cost.is_none()) {
            r.cost = Some(remaining * r.tokens.sum() as f64 / denominator);
        }
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kar_usage_is_scoped_by_open_child_and_handoff() {
        let mut p = Parser::default();
        p.parse("kar",&json!({"format":"kar-chat/1","chat":{"runtime":"codex","native_id":"original","model":"model1","created_ms":1000}}));
        let reading = |stream: &str, seq: u64, own: u64, child: u64| json!({"kind":"usage_changed","at":1100+seq,"reading":{"cursor":{"stream_id":stream,"after_seq":seq},"usage":{"total":{"input":own,"cache_read":2,"output":3},"with_children":{"input":own+child,"output":6}},"children":{"child":{"total":{"input":child,"output":3}}}}});
        let a = p.parse("kar", &reading("open-a", 1, 11, 5));
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].tokens.uncached_input, 9);
        assert_eq!(a[1].session, "child");
        assert!(p.parse("kar", &reading("open-a", 1, 11, 5)).is_empty());
        let a = p.parse("kar", &reading("open-a", 2, 15, 5));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].tokens.uncached_input, 4);
        assert_eq!(a[0].timestamp, 1102);
        p.parse("kar",&json!({"kind":"handoff_ready","target":{"runtime":"pi","native_id":"new","model":"model2"}}));
        let a = p.parse("kar", &reading("open-b", 1, 4, 0));
        assert_eq!(a[0].provider, "pi");
        assert_eq!(a[0].model, "model2");
        assert_eq!(a[0].session, "new");
    }
    #[test]
    fn codex_tiers_require_the_same_model_session_and_native_turn() {
        let mut kar = Parser::default();
        kar.parse("kar",&json!({"format":"kar-chat/1","chat":{"native_id":"n","runtime":"codex","model":"m","settings":{"service_tier":"priority"}}}));
        kar.parse("kar",&json!({"kind":"frame","frame":{"source":{"session_id":"child"},"native_turn":"wrong"}}));
        kar.parse(
            "kar",
            &json!({"kind":"frame","frame":{"source":{"session_id":"n"},"native_turn":"t"}}),
        );
        let mut native = Parser::default();
        native.parse(
            "codex",
            &json!({"type":"session_meta","payload":{"id":"n","model_provider":"openai"}}),
        );
        native.parse(
            "codex",
            &json!({"type":"turn_context","payload":{"model":"m","turn_id":"t"}}),
        );
        let raw = json!({"timestamp":"2025-05-01T01:00:00Z","type":"token_usage_record","payload":{"response_id":"r","usage":{"input_tokens":4,"output_tokens":1}}});
        let r = native.parse("codex", &raw).remove(0);
        let key = json!([r.session, r.tier_turn, r.model]).to_string();
        assert_eq!(kar.tiers.get(&key).and_then(|v| v.as_deref()), Some("fast"));
        assert_eq!(kar.tiers.len(), 1);
        native.parse("codex",&json!({"payload":{"type":"thread_settings_applied","thread_settings":{"service_tier":"ultrafast"}}}));
        let resolved = native.parse("codex", &raw).remove(0);
        assert_eq!(resolved.speed, "ultrafast");
        assert!(resolved.tier_turn.is_none());
    }
}
