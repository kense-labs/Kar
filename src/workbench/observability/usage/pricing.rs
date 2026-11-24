use super::*;
use std::path::Path;

const RAW: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const API: &str = "https://api.github.com/repos/BerriAI/litellm/contents/model_prices_and_context_window.json?ref=main";
const FLOOR: i64 = 60_000;
const TTL: i64 = 24 * 60 * FLOOR;

#[derive(Clone, PartialEq)]
struct SpeedRates {
    standard: [f64; 5],
    fast: Option<[f64; 5]>,
    ultrafast: Option<[f64; 5]>,
}
#[derive(Clone, PartialEq)]
struct Model {
    base: SpeedRates,
    tiers: BTreeMap<u64, SpeedRates>,
}
#[derive(Default, Clone)]
pub(super) struct Rates {
    models: BTreeMap<String, Model>,
    pub fetched: Option<i64>,
    status: &'static str,
}
fn channel(v: &Value, key: &str, suffix: &str) -> Option<f64> {
    v.get(format!("{key}{suffix}"))?
        .as_f64()
        .filter(|x| x.is_finite() && *x >= 0.)
}
fn rate(v: &Value, s: &str, standard: Option<[f64; 5]>, output: Option<f64>) -> Option<[f64; 5]> {
    let input = channel(v, "input_cost_per_token", s)?;
    let out = channel(v, "output_cost_per_token", s).or(output)?;
    let cache = |k, i, fallback| {
        channel(v, k, s)
            .or_else(|| standard.and_then(|b| (b[0] > 0.).then(|| b[i] / b[0] * input)))
            .unwrap_or(fallback)
    };
    let read = cache("cache_read_input_token_cost", 1, input);
    let write = cache("cache_creation_input_token_cost", 2, input);
    let hour = cache("cache_creation_input_token_cost_above_1hr", 3, write);
    Some([input, read, write, hour, out])
}
fn speed(v: &Value, s: &str, base: [f64; 5]) -> SpeedRates {
    let fast = v
        .pointer("/provider_specific_entry/fast")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.)
        .map(|m| base.map(|r| r * m))
        .or_else(|| rate(v, &format!("{s}_priority"), Some(base), None));
    SpeedRates {
        standard: base,
        fast,
        ultrafast: rate(v, &format!("{s}_ultrafast"), Some(base), None),
    }
}
fn combine_speed(a: &mut SpeedRates, b: &SpeedRates) -> bool {
    if a.standard != b.standard {
        return false;
    }
    for (left, right) in [(&mut a.fast, b.fast), (&mut a.ultrafast, b.ultrafast)] {
        match (*left, right) {
            (Some(x), Some(y)) if x != y => return false,
            (None, Some(y)) => *left = Some(y),
            _ => {}
        }
    }
    true
}
fn combine(a: &mut Model, b: &Model) -> bool {
    if !combine_speed(&mut a.base, &b.base) {
        return false;
    }
    for (k, v) in &b.tiers {
        if let Some(t) = a.tiers.get_mut(k) {
            if !combine_speed(t, v) {
                return false;
            }
        } else {
            a.tiers.insert(*k, v.clone());
        }
    }
    true
}
impl Rates {
    fn parse(document: &Value, fetched: i64, status: &'static str) -> Option<Self> {
        let mut models = BTreeMap::new();
        for (name, entry) in document.as_object()? {
            let Some(base) = rate(entry, "", None, None) else {
                continue;
            };
            let key = name.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let mut tiers = BTreeMap::new();
            for k in entry.as_object()?.keys() {
                let Some(n) = k
                    .strip_prefix("input_cost_per_token_above_")
                    .and_then(|s| s.split_once("k_tokens"))
                    .and_then(|(n, t)| matches!(t, "" | "_priority" | "_ultrafast").then_some(n))
                    .and_then(|n| n.parse::<u64>().ok())
                    .filter(|n| *n <= 9_999_999)
                else {
                    continue;
                };
                let suffix = format!("_above_{n}k_tokens");
                let tier = rate(entry, &suffix, Some(base), Some(base[4])).unwrap_or(base);
                tiers.insert(n * 1000, speed(entry, &suffix, tier));
            }
            models.insert(
                key,
                Model {
                    base: speed(entry, "", base),
                    tiers,
                },
            );
        }
        let mut aliases: BTreeMap<String, (bool, Option<Model>)> = BTreeMap::new();
        for (k, v) in &models {
            let (prefix, name) = if let Some((p, _)) = k.split_once('/') {
                (p, k.rsplit('/').next()?)
            } else if let Some(t) = k.strip_prefix("anthropic.") {
                ("", t)
            } else {
                continue;
            };
            if models.contains_key(name) || name.is_empty() {
                continue;
            }
            let primary = matches!(
                prefix,
                "anthropic" | "deepseek" | "gemini" | "meta" | "mistral" | "openai" | "xai"
            );
            match aliases.entry(name.into()) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert((primary, Some(v.clone())));
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let (priority, current) = e.get_mut();
                    if primary && !*priority {
                        *priority = true;
                        *current = Some(v.clone());
                    } else if primary == *priority
                        && let Some(m) = current
                        && !combine(m, v)
                    {
                        *current = None;
                    }
                }
            }
        }
        models.extend(aliases.into_iter().filter_map(|(n, (_, m))| Some((n, m?))));
        (!models.is_empty()).then_some(Self {
            models,
            fetched: Some(fetched),
            status,
        })
    }
    pub fn view(&self, url: &str) -> Value {
        json!({"status":if self.models.is_empty(){"unavailable"}else{self.status},"source":url,"fetched_at":self.fetched.map(stamp),"known_models":self.models.len()})
    }
    fn lookup(&self, name: &str) -> Option<&Model> {
        let key = name.trim().to_lowercase();
        let key = key.split('[').next()?;
        if matches!(
            key.rsplit('/').next()?,
            "" | "synthetic" | "<synthetic>" | "opus" | "sonnet" | "haiku" | "fable"
        ) {
            return None;
        }
        self.models.get(key)
    }
    pub fn listed(&self, model: &str) -> bool {
        self.lookup(model).is_some()
    }
    pub fn eligible(&self, model: &str) -> bool {
        let base = model
            .split('[')
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        !matches!(
            base.as_str(),
            "" | "<synthetic>" | "synthetic" | "opus" | "sonnet" | "haiku" | "fable" | "unknown"
        ) && !self.listed(model)
    }
    pub fn price(&self, r: &Entry, local: Option<&settings::Override>) -> Price {
        let override_model = local.map(|p| Model {
            base: SpeedRates {
                standard: p.rates(),
                fast: None,
                ultrafast: None,
            },
            tiers: BTreeMap::new(),
        });
        let model = r
            .rate_model
            .as_deref()
            .and_then(|n| self.lookup(n))
            .or_else(|| self.lookup(&r.model));
        let model = override_model.as_ref().or(model);
        let reported = r
            .cost
            .filter(|c| local.is_none() && c.is_finite() && *c >= 0.);
        let Some(m) = model else {
            return Price {
                total: reported.unwrap_or(0.),
                source: if reported.is_some() {
                    "provider_reported"
                } else {
                    "unpriced"
                },
                ..Default::default()
            };
        };
        let prompt = r
            .tokens
            .uncached_input
            .saturating_add(r.tokens.cached_input)
            .saturating_add(r.tokens.cache_creation);
        let tier = m.tiers.range(..prompt).next_back().map(|(_, t)| t);
        let standard = tier.unwrap_or(&m.base).standard;
        let actual = match r.speed.as_str() {
            "fast" => tier
                .and_then(|s| s.fast)
                .or(m.base.fast)
                .unwrap_or(standard),
            "ultrafast" => tier
                .and_then(|s| s.ultrafast)
                .or(m.base.ultrafast)
                .unwrap_or(standard),
            _ => standard,
        };
        let by = |a: [f64; 5]| {
            let hour = r.one_hour.min(r.tokens.cache_creation);
            [
                r.tokens.uncached_input as f64 * a[0],
                r.tokens.cached_input as f64 * a[1],
                (r.tokens.cache_creation - hour) as f64 * a[2] + hour as f64 * a[3],
                r.tokens.output as f64 * a[4],
            ]
        };
        let costs = by(actual);
        let sum = costs.iter().sum::<f64>();
        let total = reported.unwrap_or(sum);
        let scale = if sum > 0. { total / sum } else { 1. };
        Price {
            total,
            source: if reported.is_some() {
                "provider_reported"
            } else {
                "model_priced"
            },
            parts: (sum > 0. || reported.is_none()).then(|| costs.map(|v| v * scale)),
            premium: (sum - by(standard).iter().sum::<f64>()) * scale,
            savings: r.tokens.cached_input as f64 * (actual[0] - actual[1]),
        }
    }
}
#[derive(Default)]
pub(super) struct Price {
    pub total: f64,
    pub source: &'static str,
    pub parts: Option<[f64; 4]>,
    pub premium: f64,
    pub savings: f64,
}
pub(super) struct Cache {
    file: PathBuf,
    pub url: String,
    fallback: bool,
    current: Mutex<Arc<Rates>>,
    lock: tokio::sync::Mutex<()>,
    last_attempt: Mutex<Option<i64>>,
}
impl Cache {
    pub fn new(state: &Path) -> Self {
        let file = state.join("usage-rates.json");
        let url = std::env::var("KAR_USAGE_RATES_URL").unwrap_or_else(|_| RAW.into());
        let fallback = url == RAW;
        let cached = std::fs::File::open(&file)
            .ok()
            .and_then(|f| serde_json::from_reader::<_, Value>(f).ok())
            .filter(|v| v["source"] == url)
            .and_then(|v| Rates::parse(&v["document"], v["fetched_at_ms"].as_i64()?, "cached"))
            .unwrap_or_default();
        Self {
            file,
            url,
            fallback,
            current: Mutex::new(Arc::new(cached)),
            lock: Default::default(),
            last_attempt: Default::default(),
        }
    }
    pub fn latest(&self) -> Arc<Rates> {
        self.current.lock().unwrap().clone()
    }
    pub async fn get(&self, force: bool) -> Arc<Rates> {
        let _guard = self.lock.lock().await;
        let current = self.latest();
        let now = now();
        if current
            .fetched
            .is_some_and(|t| now - t < if force { FLOOR } else { TTL })
            || self
                .last_attempt
                .lock()
                .unwrap()
                .is_some_and(|t| now - t < FLOOR)
        {
            return current;
        }
        *self.last_attempt.lock().unwrap() = Some(now);
        for url in std::iter::once(self.url.as_str()).chain(self.fallback.then_some(API)) {
            if let Some((doc, rates)) = fetch(url).await {
                let data = json!({"document":doc,"fetched_at_ms":rates.fetched,"source":self.url});
                let _ = super::super::triage::atomic(
                    &self.file,
                    &serde_json::to_vec(&data).expect("price table"),
                );
                let next = Arc::new(rates);
                *self.current.lock().unwrap() = next.clone();
                return next;
            }
        }
        let mut cached = (*current).clone();
        if !cached.models.is_empty() {
            cached.status = "cached";
        }
        let next = Arc::new(cached);
        *self.current.lock().unwrap() = next.clone();
        next
    }
}
async fn fetch(url: &str) -> Option<(Value, Rates)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .ok()?;
    let mut res = client
        .get(url)
        .header("user-agent", "Kar usage pricing")
        .header(
            "accept",
            if url == API {
                "application/vnd.github.raw+json"
            } else {
                "application/json"
            },
        )
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let mut buf = Vec::new();
    while let Some(chunk) = res.chunk().await.ok()? {
        if buf.len() + chunk.len() > 32 << 20 {
            return None;
        }
        buf.extend_from_slice(&chunk);
    }
    let v = serde_json::from_slice(&buf).ok()?;
    let rates = Rates::parse(&v, now(), "fresh")?;
    Some((v, rates))
}
