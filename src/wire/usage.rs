//! Translate the provider's per-turn or lifetime counts into running totals since open.
use crate::{Event, Protocol, TokenReading, TokenTotals};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) const CLAUDE_BASELINE: &str = "oar/claudeUsageBaseline";

fn count(v: &Value, key: &str) -> Option<i64> {
    v[key].as_i64().filter(|n| *n >= 0)
}
fn totals(v: &Value, input: &str, output: &str, read: &str, write: &str) -> Option<TokenTotals> {
    Some(TokenTotals {
        input: count(v, input)?,
        output: count(v, output)?,
        cache_read: count(v, read),
        cache_write: count(v, write),
        reasoning: None,
    })
}
fn with_cache(mut t: TokenTotals) -> Option<TokenTotals> {
    t.input = t
        .input
        .checked_add(t.cache_read.unwrap_or(0))?
        .checked_add(t.cache_write.unwrap_or(0))?;
    Some(t)
}
pub(crate) fn factory_tokens(v: &Value) -> Option<TokenTotals> {
    with_cache(totals(
        v,
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheCreationTokens",
    )?)
}
type Models = BTreeMap<String, TokenTotals>;
fn models(v: &Value) -> Option<Models> {
    v.as_object()?
        .iter()
        .map(|(name, v)| {
            Some((
                name.clone(),
                with_cache(totals(
                    v,
                    "inputTokens",
                    "outputTokens",
                    "cacheReadInputTokens",
                    "cacheCreationInputTokens",
                )?)?,
            ))
        })
        .collect()
}
fn share(models: &Models, baseline: &Models) -> TokenTotals {
    models
        .iter()
        .fold(TokenTotals::default(), |sum, (name, v)| {
            sum.plus(v.minus(baseline.get(name).copied().unwrap_or_default()))
        })
}
#[derive(Default)]
struct ClaudeUsage {
    cache_write_by_ttl: Option<crate::CacheWriteByTtl>,
    result_ids: HashSet<String>,
    baseline: Option<Models>,
    last: Models,
    carried: TokenTotals,
    agents: HashMap<Vec<String>, TokenTotals>,
}
impl ClaudeUsage {
    fn result(&mut self, v: &Value, path: &[String]) -> Option<TokenReading> {
        if let Some(id) = v["uuid"].as_str().filter(|id| !id.is_empty())
            && !self.result_ids.insert(id.to_owned())
        {
            return None;
        }
        let cache_write_by_ttl = if path.is_empty() {
            self.cache_write_by_ttl.take()
        } else {
            None
        };
        let Some(baseline) = self.baseline.as_ref() else {
            return cache_write_by_ttl.map(|ttl| TokenReading {
                cache_write_by_ttl: Some(ttl),
                ..Default::default()
            });
        };
        let value = &v["usage"];
        let tokens = if value.is_object() {
            let raw = TokenTotals {
                input: count(value, "input_tokens").unwrap_or(0),
                output: count(value, "output_tokens").unwrap_or(0),
                cache_read: count(value, "cache_read_input_tokens"),
                cache_write: count(value, "cache_creation_input_tokens"),
                reasoning: None,
            };
            let t = self.agents.entry(path.to_vec()).or_default();
            *t = t.plus(with_cache(raw)?);
            Some(*t)
        } else {
            None
        };
        let mut total = None;
        if let Some(current) = models(&v["modelUsage"])
            && current.values().any(|t| t.input != 0 || t.output != 0)
        {
            let reset = self.last.iter().any(|(name, old)| {
                current
                    .get(name)
                    .is_none_or(|new| new.input < old.input || new.output < old.output)
            });
            if reset {
                self.carried = self.carried.plus(share(&self.last, baseline));
                self.baseline = Some(Models::new());
            }
            total = Some(
                self.carried
                    .plus(share(&current, self.baseline.as_ref().unwrap())),
            );
            self.last = current;
        }
        (tokens.is_some() || total.is_some() || cache_write_by_ttl.is_some()).then_some(
            TokenReading {
                tokens,
                total,
                cache_write_by_ttl,
            },
        )
    }
}

enum CodexBaseline {
    Awaiting,
    Unknown,
    Known(TokenTotals),
}
pub(crate) struct Projection {
    protocol: Protocol,
    runtime: String,
    resumed: bool,
    codex: CodexBaseline,
    codex_root: Option<String>,
    claude: ClaudeUsage,
    running: TokenTotals,
    opencode_whole_prompt: bool,
    grok_prompts: HashSet<String>,
    grok_events: HashSet<(String, String)>,
}
impl Projection {
    pub fn new(protocol: Protocol, runtime: &str, resume: Option<&str>) -> Self {
        Self {
            protocol,
            runtime: runtime.into(),
            resumed: resume.is_some(),
            codex_root: resume.map(str::to_owned),
            codex: CodexBaseline::Known(TokenTotals::default()),
            claude: ClaudeUsage {
                baseline: Some(Models::new()),
                ..Default::default()
            },
            running: Default::default(),
            opencode_whole_prompt: false,
            grok_prompts: Default::default(),
            grok_events: Default::default(),
        }
    }
    pub fn request(&mut self, method: &str, params: &Value) {
        if self.protocol == Protocol::Codex && matches!(method, "thread/fork" | "thread/resume") {
            self.codex = CodexBaseline::Awaiting;
        }
        if self.protocol == Protocol::Claude && method == CLAUDE_BASELINE {
            self.claude.baseline = None;
        }
        if self.runtime == "grok"
            && method == "session/prompt"
            && let Some(id) = params.pointer("/_meta/promptId").and_then(Value::as_str)
        {
            self.grok_prompts.insert(id.into());
        }
    }
    pub fn claude_baseline(&mut self, v: &Value) {
        self.claude.baseline = models(&v["session"]["model_usage"]);
        if let Some(base) = &self.claude.baseline {
            self.claude.last = base.clone();
        }
    }
    pub fn reply(&mut self, method: &str, v: &Value) -> Option<Event> {
        if self.protocol == Protocol::Codex
            && matches!(method, "thread/start" | "thread/fork" | "thread/resume")
        {
            self.codex_root = v
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if self.runtime == "opencode" && method == "initialize" {
            self.opencode_whole_prompt = v
                .pointer("/agentInfo/version")
                .and_then(Value::as_str)
                .and_then(|s| semver::Version::parse(s.trim_start_matches('v')).ok())
                .is_some_and(|v| v.major == 2 && v >= semver::Version::new(2, 0, 22));
        }
        if method != "session/prompt" {
            return None;
        }
        let t = match self.runtime.as_str() {
            "opencode" if self.opencode_whole_prompt => {
                let u = &v["usage"];
                if ["cachedReadTokens", "cachedWriteTokens", "thoughtTokens"]
                    .iter()
                    .any(|key| u.get(key).is_some() && count(u, key).is_none())
                {
                    return None;
                }
                let mut t = with_cache(totals(
                    u,
                    "inputTokens",
                    "outputTokens",
                    "cachedReadTokens",
                    "cachedWriteTokens",
                )?)?;
                t.reasoning = count(u, "thoughtTokens");
                t.output = t.output.checked_add(t.reasoning.unwrap_or(0))?;
                if t.input.checked_add(t.output)? != count(u, "totalTokens")? {
                    return None;
                }
                t
            }
            "antigravity" => {
                let u = &v["usage"];
                let output = count(u, "outputTokens")?;
                let input = count(u, "totalTokens")?.checked_sub(output)?;
                if input < count(u, "inputTokens")? {
                    return None;
                }
                TokenTotals {
                    input,
                    output,
                    cache_read: count(u, "cachedReadTokens").filter(|n| *n <= input),
                    cache_write: count(u, "cachedWriteTokens").filter(|n| *n == 0),
                    reasoning: count(u, "thoughtTokens").filter(|n| *n <= output),
                }
            }
            "grok" => grok_tokens(v.pointer("/_meta/usage")?)?,
            _ => return None,
        };
        Some(self.accumulate(t))
    }
    fn accumulate(&mut self, one: TokenTotals) -> Event {
        self.running = self.running.plus(one);
        event(Some(self.running), None)
    }
    pub fn notice(&mut self, v: &Value, path: &[String], root: Option<&str>) -> Option<Event> {
        match self.protocol {
            Protocol::Codex => {
                let method = v["method"].as_str()?;
                let reported = v.pointer("/params/threadId").and_then(Value::as_str)?;
                let root = self
                    .codex_root
                    .as_deref()
                    .or(root)
                    .is_none_or(|s| s == reported);
                if method == "turn/started" && root && matches!(self.codex, CodexBaseline::Awaiting)
                {
                    self.codex = CodexBaseline::Unknown;
                }
                if method != "thread/tokenUsage/updated" {
                    return None;
                }
                let u = v.pointer("/params/tokenUsage/total")?;
                let mut t = totals(
                    u,
                    "inputTokens",
                    "outputTokens",
                    "cachedInputTokens",
                    "cacheWriteInputTokens",
                )?;
                t.reasoning = count(u, "reasoningOutputTokens");
                if root {
                    if matches!(self.codex, CodexBaseline::Awaiting) {
                        self.codex = CodexBaseline::Known(t);
                    }
                    let CodexBaseline::Known(base) = self.codex else {
                        return None;
                    };
                    t = t.minus(base);
                }
                Some(event(Some(t), None))
            }
            Protocol::Claude
                if path.is_empty()
                    && v["type"] == "assistant"
                    && v["message"]["model"] != "<synthetic>"
                    && v["message"]["usage"].is_object() =>
            {
                let creation = &v["message"]["usage"]["cache_creation"];
                let five_minutes = count(creation, "ephemeral_5m_input_tokens");
                let one_hour = count(creation, "ephemeral_1h_input_tokens");
                self.claude.cache_write_by_ttl = (five_minutes.is_some() || one_hour.is_some())
                    .then_some(crate::CacheWriteByTtl {
                        five_minutes,
                        one_hour,
                    });
                None
            }
            Protocol::Claude
                if path.is_empty()
                    && v["type"] == "system"
                    && v["subtype"] == "compact_boundary" =>
            {
                self.claude.cache_write_by_ttl = None;
                None
            }
            Protocol::Claude if v["type"] == "result" => self
                .claude
                .result(v, path)
                .map(|reading| Event::TokenUsage { reading }),
            Protocol::Qwen if v["type"] == "result" && !self.resumed && path.is_empty() => {
                Some(event(
                    None,
                    Some(totals(
                        &v["usage"],
                        "input_tokens",
                        "output_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    )?),
                ))
            }
            Protocol::Qoder if v["type"] == "result" && path.is_empty() => {
                let per_query = with_cache(totals(
                    &v["usage"],
                    "input_tokens",
                    "output_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                )?)?;
                self.running = self.running.plus(per_query);
                Some(event(None, Some(self.running)))
            }
            Protocol::Pi if v["type"] == "message_end" && v["message"]["role"] == "assistant" => {
                let u = v.pointer("/message/usage")?.as_object()?;
                let n = |k: &str| u.get(k).and_then(Value::as_i64).filter(|n| *n >= 0);
                let t = with_cache(TokenTotals {
                    input: n("input").unwrap_or(0),
                    output: n("output").unwrap_or(0),
                    cache_read: n("cacheRead"),
                    cache_write: n("cacheWrite"),
                    reasoning: n("reasoning"),
                })?;
                self.running = self.running.plus(t);
                Some(Event::TokenUsage {
                    reading: TokenReading {
                        tokens: Some(self.running),
                        total: None,
                        cache_write_by_ttl: n("cacheWrite1h").map(|one_hour| {
                            crate::CacheWriteByTtl {
                                five_minutes: None,
                                one_hour: Some(one_hour),
                            }
                        }),
                    },
                })
            }
            Protocol::Acp
                if self.runtime == "grok" && v["method"] == "_x.ai/session_notification" =>
            {
                let p = &v["params"];
                let update = &p["update"];
                let prompt = update["prompt_id"].as_str().filter(|id| !id.is_empty())?;
                if update["sessionUpdate"] != "turn_completed" || self.grok_prompts.contains(prompt)
                {
                    return None;
                }
                let source = p["sessionId"].as_str()?;
                let key = p["_meta"]["eventId"].as_str().filter(|id| !id.is_empty())?;
                let t = grok_tokens(&update["usage"])?;
                if !self.grok_events.insert((source.into(), key.into()))
                    || p["_meta"]["isReplay"] == true
                    || Some(source) != root
                {
                    return None;
                }
                Some(self.accumulate(t))
            }
            _ => None,
        }
    }
}
fn grok_tokens(u: &Value) -> Option<TokenTotals> {
    Some(TokenTotals {
        input: count(u, "inputTokens").or_else(|| count(u, "input_tokens"))?,
        output: count(u, "outputTokens").or_else(|| count(u, "output_tokens"))?,
        cache_read: count(u, "cachedReadTokens"),
        cache_write: count(u, "cacheCreationTokens"),
        reasoning: count(u, "reasoningTokens"),
    })
}
fn event(tokens: Option<TokenTotals>, total: Option<TokenTotals>) -> Event {
    Event::TokenUsage {
        reading: TokenReading {
            tokens,
            total,
            cache_write_by_ttl: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reading(event: Option<Event>) -> TokenReading {
        let Some(Event::TokenUsage { reading }) = event else {
            panic!("missing measured tokens")
        };
        reading
    }
    #[test]
    fn claude_separates_main_loop_and_aggregate_through_resume_and_clear() {
        let mut p = Projection::new(Protocol::Claude, "claude", Some("r"));
        p.request(CLAUDE_BASELINE, &Value::Null);
        let result = |n: i64| {
            json!({"type":"result","usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":2},
            "modelUsage":{"main":{"inputTokens":n,"outputTokens":n,"cacheReadInputTokens":4}}})
        };
        assert!(p.notice(&result(13), &[], Some("r")).is_none());
        p.claude_baseline(&json!({"session":{"model_usage":{"main":{"inputTokens":7,"outputTokens":7,"cacheReadInputTokens":2}}}}));
        let sample = reading(p.notice(&result(13), &[], Some("r")));
        assert_eq!(sample.tokens.unwrap().input, 4);
        assert_eq!(
            (sample.total.unwrap().input, sample.total.unwrap().output),
            (8, 6)
        );
        let sample = reading(p.notice(&result(1), &[], Some("r")));
        assert_eq!(
            (sample.total.unwrap().input, sample.total.unwrap().output),
            (13, 7)
        );
        assert_eq!(sample.tokens.unwrap().input, 8);
        let zero = reading(p.notice(
            &json!({"type":"result","usage":{},"modelUsage":{}}),
            &[],
            Some("r"),
        ));
        assert!(zero.total.is_none());
    }
    #[test]
    fn claude_receipts_survive_missing_baseline_and_replayed_result_ids() {
        let mut p = Projection::new(Protocol::Claude, "claude", Some("root"));
        p.request(CLAUDE_BASELINE, &Value::Null);
        let message = json!({"type":"assistant","message":{"model":"main","usage":{
            "cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":8}}}});
        let result =
            |id| json!({"type":"result","uuid":id,"usage":{"input_tokens":1,"output_tokens":2}});
        let r = Some("root");
        p.notice(&message, &[], r);
        let unknown = reading(p.notice(&result("one"), &[], r));
        assert!(unknown.tokens.is_none());
        let ttl = unknown.cache_write_by_ttl.unwrap();
        assert_eq!((ttl.five_minutes, ttl.one_hour), (Some(0), Some(8)));
        p.claude_baseline(&json!({"session":{"model_usage":{}}}));
        p.notice(&message, &[], r);
        assert!(p.notice(&result("one"), &[], r).is_none());
        let next = reading(p.notice(&result("two"), &[], r));
        assert_eq!(next.tokens.unwrap().input, 1);
        assert_eq!(next.cache_write_by_ttl, Some(ttl));
        p.notice(&message, &[], r);
        p.notice(
            &json!({"type":"system","subtype":"compact_boundary"}),
            &[],
            r,
        );
        let after_compaction = reading(p.notice(&result("three"), &[], r));
        assert_eq!(after_compaction.tokens.unwrap().input, 2);
        assert!(after_compaction.cache_write_by_ttl.is_none());
    }
    #[test]
    fn no_codex_resume_baseline_keeps_only_independent_child_usage() {
        let mut p = Projection::new(Protocol::Codex, "codex", Some("root"));
        p.request("thread/resume", &Value::Null);
        let packet = |id: &str| json!({"method":"thread/tokenUsage/updated","params":{"threadId":id,"tokenUsage":{"total":{"inputTokens":18,"outputTokens":5,"reasoningOutputTokens":3}}}});
        p.notice(
            &json!({"method":"turn/started","params":{"threadId":"root"}}),
            &[],
            Some("root"),
        );
        assert!(p.notice(&packet("root"), &[], Some("root")).is_none());
        let t = reading(p.notice(&packet("child"), &[], Some("root")))
            .tokens
            .unwrap();
        assert_eq!((t.input, t.output, t.reasoning), (18, 5, Some(3)));
    }
    #[test]
    fn grok_ignores_own_terminal_notifications_replays_and_overlapping_children() {
        let mut p = Projection::new(Protocol::Acp, "grok", None);
        p.request("session/prompt", &json!({"_meta":{"promptId":"host"}}));
        let report = |source: &str, prompt: &str, id: &str, replay: bool| {
            json!({"method":"_x.ai/session_notification","params":{"sessionId":source,
            "_meta":{"eventId":id,"isReplay":replay},"update":{"sessionUpdate":"turn_completed","prompt_id":prompt,"usage":{"inputTokens":10,"outputTokens":2}}}})
        };
        let r = Some("root");
        assert!(
            p.notice(&report("root", "host", "own", false), &[], r)
                .is_none()
        );
        assert!(
            p.notice(&report("root", "wake", "old", true), &[], r)
                .is_none()
        );
        assert!(
            p.notice(&report("root", "wake", "old", false), &[], r)
                .is_none()
        );
        assert!(
            p.notice(&report("child", "wake", "new", false), &[], r)
                .is_none()
        );
        assert_eq!(
            reading(p.reply(
                "session/prompt",
                &json!({"_meta":{"usage":{"input_tokens":20,"output_tokens":4}}})
            ))
            .tokens
            .unwrap()
            .input,
            20
        );
        assert_eq!(
            reading(p.notice(&report("root", "wake", "new", false), &[], r))
                .tokens
                .unwrap()
                .input,
            30
        );
        assert!(
            p.notice(&report("root", "wake", "new", false), &[], r)
                .is_none()
        );
        assert_eq!(
            reading(p.notice(&report("root", "wake", "later", false), &[], r))
                .tokens
                .unwrap()
                .input,
            40
        );
    }
    #[test]
    fn acp_whole_prompt_meters_only_verified_fields_and_versions() {
        let reply = json!({"usage":{"inputTokens":10,"outputTokens":3,"cachedReadTokens":2,"cachedWriteTokens":1,"thoughtTokens":4,"totalTokens":20}});
        let mut open = Projection::new(Protocol::Acp, "opencode", None);
        assert!(open.reply("session/prompt", &reply).is_none());
        open.reply("initialize", &json!({"agentInfo":{"version":"2.0.22"}}));
        let t = reading(open.reply("session/prompt", &reply))
            .tokens
            .unwrap();
        assert_eq!(
            (t.input, t.output, t.cache_read, t.cache_write, t.reasoning),
            (13, 7, Some(2), Some(1), Some(4))
        );
        let mut inconsistent = reply.clone();
        inconsistent["usage"]["totalTokens"] = 19.into();
        assert!(open.reply("session/prompt", &inconsistent).is_none());
        let mut agy = Projection::new(Protocol::Acp, "antigravity", None);
        let t = reading(agy.reply("session/prompt", &reply)).tokens.unwrap();
        assert_eq!(
            (t.input, t.output, t.cache_read, t.cache_write, t.reasoning),
            (17, 3, Some(2), None, None)
        );
    }
}
