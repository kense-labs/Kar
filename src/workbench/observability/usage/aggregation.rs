use super::*;
#[derive(Deserialize)]
pub(in crate::workbench::observability) struct Request {
    since_day: String,
    until_day: String,
    time_zone: String,
    resolution: Option<String>,
    since_time: Option<String>,
    until_time: Option<String>,
    #[serde(default)]
    pub await_refresh: bool,
    #[serde(default)]
    pub provisional: bool,
    model_filter: Option<Filter>,
}
#[derive(Deserialize, Serialize)]
struct Filter {
    include: Option<Vec<Selector>>,
    exclude: Option<Vec<Selector>>,
}
#[derive(Deserialize, Serialize)]
struct Selector {
    provider: Option<String>,
    model: String,
}
impl Selector {
    fn matches(&self, r: &Entry) -> bool {
        self.provider.as_deref().is_none_or(|p| p == r.provider) && self.model == r.model
    }
}
pub(super) struct Window {
    zone: jiff::tz::TimeZone,
    bounds: Option<(i64, i64)>,
    hour: bool,
}
impl Request {
    pub(super) fn account_bounds(&self, w: &Window) -> Result<(i64, i64)> {
        if let Some((a, b)) = w.bounds {
            return Ok((a, b - 1));
        }
        let bounds = || -> std::result::Result<_, jiff::Error> {
            let a = self
                .since_day
                .parse::<jiff::civil::Date>()?
                .to_zoned(w.zone.clone())?;
            let b = self
                .until_day
                .parse::<jiff::civil::Date>()?
                .tomorrow()?
                .to_zoned(w.zone.clone())?;
            Ok((
                a.timestamp().as_millisecond(),
                b.timestamp().as_millisecond() - 1,
            ))
        };
        bounds().map_err(|_| Error::Refused("invalid usage date range".into()))
    }
    pub(super) fn window(&self) -> Result<Window> {
        let invalid = || Error::Refused("invalid usage date or time window".into());
        let date = |s: &str| {
            s.parse::<jiff::civil::Date>()
                .ok()
                .filter(|v| v.to_string() == s)
                .ok_or_else(invalid)
        };
        if date(&self.since_day)? > date(&self.until_day)? {
            return Err(invalid());
        }
        let zone = jiff::tz::TimeZone::get(&self.time_zone)
            .map_err(|_| Error::Refused("invalid usage time zone".into()))?;
        let hour = match self.resolution.as_deref() {
            None | Some("day") => false,
            Some("hour") => true,
            _ => return Err(invalid()),
        };
        let instant = |s: &str| {
            s.parse::<jiff::Timestamp>()
                .map(|v| v.as_millisecond())
                .map_err(|_| invalid())
        };
        let bounds = if !hour {
            None
        } else {
            match (&self.since_time, &self.until_time) {
                (Some(a), Some(b)) => {
                    let a = instant(a)?;
                    let b = instant(b)?;
                    if b <= a || (hour && b - a > 86_400_000) {
                        return Err(invalid());
                    }
                    Some((a, b))
                }
                _ => return Err(invalid()),
            }
        };
        Ok(Window { zone, bounds, hour })
    }
}
#[derive(Default)]
struct Bucket {
    totals: Totals,
    records: u64,
    unpriced: u64,
    reported: u64,
    unpriced_tokens: u64,
    sessions: BTreeSet<String>,
    cost: f64,
    savings: f64,
    parts: Option<[f64; 4]>,
    fast: f64,
    ultra: f64,
    premium: f64,
}
impl Bucket {
    fn add(&mut self, r: &Entry, p: pricing::Price) {
        self.totals = self.totals.zip(r.tokens, u64::saturating_add);
        self.records += 1;
        if !r.session.is_empty() {
            self.sessions.insert(r.session.clone());
        }
        self.cost += p.total;
        self.savings += p.savings;
        self.premium += p.premium;
        match r.speed.as_str() {
            "fast" => self.fast += p.total,
            "ultrafast" => self.ultra += p.total,
            _ => {}
        }
        match p.source {
            "unpriced" => {
                self.unpriced += 1;
                self.unpriced_tokens = self.unpriced_tokens.saturating_add(r.tokens.sum());
            }
            "provider_reported" => self.reported += 1,
            _ => {}
        }
        if let Some(parts) = p.parts {
            let sum = self.parts.get_or_insert([0.; 4]);
            for (a, b) in sum.iter_mut().zip(parts) {
                *a += b;
            }
        }
    }
}
pub(super) fn build(
    s: &scans::Snapshot,
    rates: &pricing::Rates,
    url: &str,
    q: &Request,
    w: Window,
    refreshing: bool,
    version: &settings::Version,
) -> Value {
    let aliases = version.settings.aliases();
    let tiers = s.codex_tiers();
    let mut native = BTreeSet::new();
    for f in s.files.values().filter(|f| f.format != "kar") {
        for e in &f.entries {
            if !e.session.is_empty() {
                native.insert((&e.provider, &e.session));
            }
        }
    }
    type AggregatedEntry<'a> = (usize, &'a Entry, Totals, Option<f64>, u64);
    let mut records: BTreeMap<(String, String), AggregatedEntry<'_>> = BTreeMap::new();
    let ag = databases::antigravity::merged(&s.files);
    for (path, f) in &s.files {
        let mut occurrences = BTreeMap::new();
        let entries = if f.format == "antigravity" {
            ag.get(path).map(Vec::as_slice).unwrap_or_default()
        } else {
            &f.entries
        };
        for (i, e) in entries.iter().enumerate() {
            if (f.format == "kar" && native.contains(&(&e.provider, &e.session)))
                || q.model_filter.as_ref().is_some_and(|f| {
                    f.include
                        .as_ref()
                        .is_some_and(|v| !v.iter().any(|x| x.matches(e)))
                        || f.exclude
                            .as_ref()
                            .is_some_and(|v| v.iter().any(|x| x.matches(e)))
                })
            {
                continue;
            }
            let key = e.key.clone().unwrap_or_else(|| {
                if e.provider == "codex" && f.format == "codex" && !e.session.is_empty() {
                    let id = json!([e.session, e.timestamp, e.model, e.tokens]).to_string();
                    let sequence = occurrences.entry(id.clone()).or_insert(0usize);
                    *sequence += 1;
                    format!("snapshot:{id}:{sequence}")
                } else {
                    format!("file:{}:{i}", path.display())
                }
            });
            let Some(source) = s.source_id(&f.root, &e.provider) else {
                continue;
            };
            records
                .entry((e.provider.clone(), key))
                .and_modify(|p| {
                    if e.provider == "grok" {
                        p.2 = p.2.zip(e.tokens, u64::min);
                        p.3 = p.3.into_iter().chain(e.cost).reduce(f64::min);
                    } else {
                        p.2 = p.2.zip(e.tokens, u64::max);
                        p.3 = p.3.into_iter().chain(e.cost).reduce(f64::max);
                        p.4 = p.4.max(e.one_hour);
                    }
                })
                .or_insert((source, e, e.tokens, e.cost, e.one_hour));
        }
    }
    let mut buckets = BTreeMap::new();
    let mut sessions: Vec<BTreeSet<&str>> =
        (0..s.sources.len()).map(|_| Default::default()).collect();
    for (_, (source, e, tokens, cost, one_hour)) in records {
        let Ok(time) = jiff::Timestamp::from_millisecond(e.timestamp) else {
            continue;
        };
        let day = time.to_zoned(w.zone.clone()).date().to_string();
        if (!w.hour && (day < q.since_day || day > q.until_day))
            || w.bounds
                .is_some_and(|(a, b)| e.timestamp < a || e.timestamp >= b)
        {
            continue;
        }
        let hour = w
            .bounds
            .filter(|_| w.hour)
            .map(|(a, _)| a + (e.timestamp - a) / 3_600_000 * 3_600_000);
        let mut entry = Entry {
            tokens,
            cost,
            one_hour,
            ..e.clone()
        };
        if let Some(turn) = &entry.tier_turn
            && let Some(Some(speed)) = tiers.get(
                json!([entry.session, turn, entry.model])
                    .to_string()
                    .as_str(),
            )
        {
            entry.speed = (*speed).into();
        }
        let model = aliases.get(e.model.as_str()).copied().unwrap_or(&e.model);
        if model != e.model {
            entry.model = model.into();
            entry.rate_model = if rates.listed(model) {
                None
            } else {
                match e.provider.as_str() {
                    "cursor" => Some(cursor::rate_model(model)),
                    "pi" => model.split_once('/').map(|(_, n)| n.into()),
                    "antigravity" => Some(databases::antigravity::rate(model)),
                    _ => None,
                }
            };
        }
        if !e.session.is_empty() {
            sessions[source].insert(&e.session);
        }
        buckets
            .entry((day, hour, &e.provider, model, source))
            .or_insert_with(Bucket::default)
            .add(
                &entry,
                rates.price(&entry, version.settings.price_overrides.get(model)),
            );
    }
    let round = |x: f64| (x * 1e6).round() / 1e6;
    let buckets=buckets.into_iter().map(|((day,hour,provider,model,src),b)|{let mut row=json!({"day":day,"provider":provider,"model":model,"source_path":s.sources[src].root,"totals":b.totals,"cost_usd":round(b.cost),"cache_savings_usd":b.savings,"records":b.records,"sessions":b.sessions.len(),"unpriced_records":b.unpriced,"unpriced_tokens":b.unpriced_tokens,"cost_source":if b.unpriced==b.records{"unpriced"}else if b.reported==b.records{"provider_reported"}else{"model_priced"}});
        if let Some(h)=hour{row["hour_start"]=json!(stamp(h));}
        if let Some([input,read,write,output])=b.parts{row["category_cost_usd"]=json!({"input":round(input),"cache_read":round(read),"cache_write":round(write),"output":round(output)});}
        for(k,v)in [("fast_cost_usd",b.fast),("ultrafast_cost_usd",b.ultra),("speed_premium_usd",b.premium)]{if round(v)!=0.{row[k]=json!(round(v));}}
        row}).collect::<Vec<_>>();
    let sources = s
        .sources
        .iter()
        .zip(sessions)
        .map(|(src, ids)| {
            let mut v = serde_json::to_value(src).expect("usage source");
            v["distinct_sessions"] = json!(ids.len());
            if refreshing {
                v["refreshing"] = json!(true);
            }
            v
        })
        .collect::<Vec<_>>();
    let mut out = json!({"contract_version":6,"settings_revision":version.revision,"read_at":stamp(s.at),"since_day":q.since_day,"until_day":q.until_day,"time_zone":q.time_zone,"scan_duration_ms":s.duration,"buckets":buckets,"sources":sources,"pricing":rates.view(url)});
    if let Some(f) = &q.model_filter {
        out["model_filter"] = json!(f);
    }
    out
}
