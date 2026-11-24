//! Fetch Cursor billing data from its account RPC. Account fingerprints and cache keys are hashes.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;
const API: &str = "https://api2.cursor.sh";
const TTL: i64 = 60_000;
const HOUR: i64 = 3_600_000;
fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
fn failure(s: &str) -> String {
    s.into()
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct Account {
    pub id: String,
    pub rows: Vec<Entry>,
    ranges: Vec<(i64, i64, i64)>,
}
#[derive(Clone, Default)]
pub(super) struct Read {
    pub account: Option<Arc<Account>>,
    pub message: Option<String>,
    pub refreshing: bool,
}
pub(super) struct Cache {
    dir: PathBuf,
    data: tokio::sync::Mutex<()>,
    last: Mutex<Option<(String, Instant, Read)>>,
}
impl Cache {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            data: Default::default(),
            last: Default::default(),
        }
    }
    pub fn prepared() -> Option<Zeroizing<String>> {
        std::env::var("CURSOR_API_KEY")
            .ok()
            .map(|s| Zeroizing::new(s.trim().to_owned()))
            .filter(|s| !s.is_empty())
    }
    fn cached(&self, key: &str) -> Read {
        self.last
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(k, _, _)| k == key)
            .map(|(_, _, r)| r.clone())
            .unwrap_or_else(|| {
                let source = self.dir.join(format!("usage-cursor-{key}.json"));
                let account = std::fs::File::open(source)
                    .ok()
                    .and_then(|f| {
                        serde_json::from_reader::<_, Account>(std::io::BufReader::new(f)).ok()
                    })
                    .map(Arc::new);
                Read {
                    account,
                    ..Default::default()
                }
            })
    }
    pub fn preview(&self, key: &str) -> Read {
        Read {
            refreshing: true,
            ..self.cached(&hash(key))
        }
    }
    pub async fn get(&self, key: &str, from: i64, to: i64) -> Read {
        let _lock = self.data.lock().await;
        let credential = hash(key);
        let cached = self.cached(&credential);
        let now = now();
        let from = from.max(0);
        let to = to.min(now);
        if from > to {
            return Read::default();
        }
        let fail_recent = self.last.lock().unwrap().as_ref().is_some_and(|(k, t, r)| {
            k == &credential && t.elapsed() < Duration::from_secs(60) && r.message.is_some()
        });
        if fail_recent
            || cached.account.as_ref().is_some_and(|a| {
                a.ranges
                    .iter()
                    .any(|&(a, b, t)| a <= from && b >= to.min(t) && now - t < TTL && b >= from)
            })
        {
            return cached;
        }
        // Late-finalized dashboard events are re-read in the trailing hour of a contiguous interval.
        let since = cached
            .account
            .as_ref()
            .and_then(|a| {
                a.ranges
                    .iter()
                    .filter(|(a, b, _)| *a <= from && *b >= from)
                    .map(|r| r.1)
                    .max()
            })
            .filter(|&b| b < to)
            .map_or(from, |b| from.max(b - HOUR));
        let query = async {
            let (id, rows) = fetch(API, key, since, to).await?;
            if since != from && cached.account.as_ref().is_some_and(|a| a.id != id) {
                return fetch(API, key, from, to)
                    .await
                    .map(|(id, rows)| (id, rows, from));
            }
            Ok((id, rows, since))
        };
        let output = tokio::time::timeout(Duration::from_secs(60), query)
            .await
            .unwrap_or_else(|_| Err(failure("Cursor account usage timed out")));
        let read = match output {
            Ok((id, rows, since)) => {
                let mut account = cached
                    .account
                    .filter(|a| a.id == id)
                    .map(Arc::unwrap_or_clone)
                    .unwrap_or_default();
                account.id = id;
                account
                    .rows
                    .retain(|r| r.timestamp < since || r.timestamp > to);
                account.rows.extend(rows);
                account.ranges.retain(|&(a, b, _)| a < from || b > to);
                account.ranges.push((from, to, now));
                let saved = super::super::triage::atomic(
                    &self.dir.join(format!("usage-cursor-{credential}.json")),
                    &serde_json::to_vec(&account).expect("account usage"),
                );
                Read {
                    account: Some(Arc::new(account)),
                    message: saved
                        .err()
                        .map(|_| "could not persist Cursor account usage".into()),
                    refreshing: false,
                }
            }
            Err(msg) => Read {
                message: Some(msg),
                ..cached
            },
        };
        *self.last.lock().unwrap() = Some((credential, Instant::now(), read.clone()));
        read
    }
}
async fn post(
    client: &reqwest::Client,
    url: String,
    token: &str,
    data: &Value,
) -> std::result::Result<Value, String> {
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("connect-protocol-version", "1")
        .json(data)
        .send()
        .await
        .map_err(|_| failure("Cursor account request failed"))?;
    if matches!(resp.status().as_u16(), 401 | 403) {
        return Err(failure("sign in to Cursor again to read account usage"));
    }
    if !resp.status().is_success() {
        return Err(format!(
            "Cursor account returned HTTP {}",
            resp.status().as_u16()
        ));
    }
    let mut resp = resp;
    let mut bytes = Vec::new();
    while let Some(b) = resp
        .chunk()
        .await
        .map_err(|_| failure("Cursor account response was interrupted"))?
    {
        if bytes.len() + b.len() > 16 << 20 {
            return Err(failure("Cursor account page exceeds 16 MiB"));
        }
        bytes.extend_from_slice(&b);
    }
    let v: Value =
        serde_json::from_slice(&bytes).map_err(|_| failure("invalid Cursor account response"))?;
    if !v.is_object()
        || ["error", "message", "code"]
            .iter()
            .any(|k| v.get(k).is_some())
    {
        return Err(failure("invalid Cursor account response"));
    }
    Ok(v)
}
async fn fetch(
    base: &str,
    key: &str,
    from: i64,
    to: i64,
) -> std::result::Result<(String, Vec<Entry>), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| failure("Cursor account client failed"))?;
    let mut auth = post(
        &client,
        format!("{base}/auth/exchange_user_api_key"),
        key,
        &json!({}),
    )
    .await?;
    let Some(Value::String(access)) = auth.as_object_mut().and_then(|o| o.remove("accessToken"))
    else {
        return Err(failure("invalid Cursor authentication"));
    };
    let access = Zeroizing::new(access);
    let payload = access
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let id = payload
        .as_ref()
        .and_then(|v| v["sub"].as_str())
        .filter(|s| !s.is_empty())
        .map(hash)
        .ok_or_else(|| failure("invalid Cursor account identity"))?;
    let endpoint = format!("{base}/aiserver.v1.DashboardService");
    let empty = json!({});
    let (teams, me) = tokio::try_join!(
        post(&client, format!("{endpoint}/GetTeams"), &access, &empty),
        post(&client, format!("{endpoint}/GetMe"), &access, &empty)
    )?;
    let user = me["userId"]
        .as_u64()
        .filter(|i| *i > 0)
        .ok_or_else(|| failure("invalid Cursor user identity"))?;
    let mut scopes = BTreeSet::from([None]);
    if let Some(v) = teams.get("teams") {
        let list = v
            .as_array()
            .ok_or_else(|| failure("invalid Cursor teams"))?;
        for team in list {
            scopes.insert(Some(
                team["id"]
                    .as_u64()
                    .filter(|i| *i > 0)
                    .ok_or_else(|| failure("invalid Cursor team identity"))?,
            ));
        }
    }
    use futures_util::{StreamExt, TryStreamExt, stream};
    let rows = stream::iter(scopes)
        .map(|team| scope(&client, &endpoint, &access, &id, user, team, from, to))
        .buffer_unordered(2)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect();
    Ok((id, rows))
}
fn rate_model(name: &str) -> String {
    static SUFFIX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?:-thinking)?(?:-(?:none|minimal|low|medium|high|xhigh|max))?(?:-fast)?$",
        )
        .unwrap()
    });
    let s = SUFFIX.replace(name.strip_prefix("cursor-").unwrap_or(name), "");
    if s.starts_with("grok-") {
        format!("xai/{s}")
    } else {
        s.into_owned()
    }
}
fn normalize(
    v: &Value,
    scope: Option<u64>,
    from: i64,
    to: i64,
) -> std::result::Result<Option<Entry>, String> {
    let u = &v["tokenUsage"];
    if u.is_null() {
        return Ok(None);
    }
    if !u.is_object() {
        return Err(failure("invalid Cursor token usage"));
    }
    for k in [
        "inputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "outputTokens",
        "totalCents",
    ] {
        if u.get(k)
            .is_some_and(|x| x.as_f64().is_none_or(|f| !f.is_finite() || f < 0.))
        {
            return Err(failure("invalid Cursor token count"));
        }
    }
    let t = v["timestamp"]
        .as_i64()
        .or_else(|| v["timestamp"].as_str()?.parse().ok())
        .ok_or_else(|| failure("invalid Cursor usage time"))?;
    let model = v["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| failure("invalid Cursor usage model"))?;
    if t < from || t > to {
        return Ok(None);
    }
    let n = |k: &str| u[k].as_f64().unwrap_or(0.) as u64;
    let totals = Totals::from_array([
        n("inputTokens"),
        n("cacheReadTokens"),
        n("cacheWriteTokens"),
        n("outputTokens"),
        0,
    ]);
    let session = v["conversationId"].as_str().unwrap_or("");
    let cost = u["totalCents"].as_f64().map(|c| c / 100.);
    Ok(Some(Entry {
        provider: "cursor".into(),
        session: session.into(),
        timestamp: t,
        model: model.into(),
        rate_model: Some(rate_model(model)),
        tokens: totals,
        cost,
        key: Some(hash(
            json!([scope, t, model, session, totals, cost]).to_string(),
        )),
        ..Default::default()
    }))
}
async fn scope(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    account: &str,
    user: u64,
    team: Option<u64>,
    from: i64,
    to: i64,
) -> std::result::Result<Vec<Entry>, String> {
    let mut pages = Vec::new();
    let mut total = None;
    let mut previous = Vec::new();
    let mut size = 0usize;
    for page in 1.. {
        let max = total.map_or(1000, |n: usize| {
            n.div_ceil(1000).saturating_mul(2).saturating_add(1)
        });
        if page > max {
            return Err(failure("Cursor account page limit exceeded"));
        }
        let mut data = json!({"page":page,"pageSize":1000,"startDate":from.to_string(),"endDate":to.to_string()});
        if let Some(team) = team {
            data["teamId"] = json!(team);
            data["userId"] = json!(user);
        }
        let v = post(
            client,
            format!("{endpoint}/GetFilteredUsageEvents"),
            token,
            &data,
        )
        .await?;
        let count = if v.as_object().is_some_and(|o| o.is_empty()) {
            Some(0)
        } else {
            v.get("totalUsageEventsCount")
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| failure("invalid Cursor account count"))
                })
                .transpose()?
        };
        if total.zip(count).is_some_and(|(a, b)| a != b) {
            return Err(failure("inconsistent Cursor account count"));
        }
        total = count.or(total);
        let events = match v["usageEventsDisplay"].as_array() {
            Some(e) if e.len() <= 1000 => e.as_slice(),
            None if v
                .as_object()
                .unwrap()
                .keys()
                .all(|k| k == "totalUsageEventsCount") =>
            {
                &[]
            }
            _ => return Err(failure("invalid Cursor account page")),
        };
        let keys: Vec<_> = events.iter().map(|e| hash(e.to_string())).collect();
        let overlap = (1..=keys.len().min(previous.len()))
            .rev()
            .find(|&i| previous[previous.len() - i..] == keys[..i])
            .unwrap_or(0);
        let records: Vec<_> = events
            .iter()
            .map(|v| normalize(v, team, from, to))
            .collect::<std::result::Result<_, _>>()?;
        size += records.len();
        pages.push((records, overlap));
        previous = keys;
        if events.len() < 1000 {
            break;
        }
    }
    let mut extra = size
        .checked_sub(total.unwrap_or(size))
        .ok_or_else(|| failure("incomplete Cursor account pages"))?;
    let mut occurrences = BTreeMap::new();
    let mut rows = Vec::new();
    for (records, overlap) in pages {
        let skip = extra.min(overlap);
        extra -= skip;
        for mut row in records.into_iter().skip(skip).flatten() {
            let id = row.key.take().unwrap();
            let seq = occurrences.entry(id.clone()).or_insert(0usize);
            row.key = Some(format!("{account}:{id}:{seq}"));
            *seq += 1;
            rows.push(row);
        }
    }
    if extra > 0 {
        return Err(failure("inconsistent Cursor account page boundaries"));
    }
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post as route};
    #[tokio::test]
    async fn cursor_account_usage_counts_paid_occurrences_across_overlapping_pages() {
        // The local dashboard peer never receives an installed credential.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let event = json!({"timestamp":"100","conversationId":"s","model":"cursor-grok-test-thinking-high-fast","tokenUsage":{"inputTokens":10,"cacheReadTokens":2,"totalCents":2}});
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"fixture-account"}"#);
        let access = format!("header.{payload}.unused");
        let router = Router::new()
            .route(
                "/auth/exchange_user_api_key",
                route(move |h: HeaderMap| async move {
                    assert_eq!(h["authorization"], "Bearer fixture-key");
                    Json(json!({"accessToken":access}))
                }),
            )
            .route(
                "/aiserver.v1.DashboardService/GetTeams",
                route(|| async { Json(json!({"teams":[{"id":2},{"id":2}]})) }),
            )
            .route(
                "/aiserver.v1.DashboardService/GetMe",
                route(|| async { Json(json!({"userId":9})) }),
            )
            .route(
                "/aiserver.v1.DashboardService/GetFilteredUsageEvents",
                route(move |h: HeaderMap, Json(q): Json<Value>| {
                    let event = event.clone();
                    async move {
                        assert!(
                            h["authorization"]
                                .to_str()
                                .unwrap()
                                .starts_with("Bearer header.")
                        );
                        assert_eq!(q["pageSize"], 1000);
                        let (events, total) = if q["teamId"] == 2 {
                            assert_eq!(q["userId"], 9);
                            (vec![event], 1)
                        } else if q["page"] == 1 {
                            let mut rows = vec![json!({"timestamp":"90"}); 998];
                            rows.extend([event.clone(), event]);
                            (rows, 1001)
                        } else {
                            assert_eq!(q["page"], 2);
                            let mut next = event.clone();
                            next["timestamp"] = json!(150);
                            (vec![event, next], 1001)
                        };
                        Json(json!({"totalUsageEventsCount":total,"usageEventsDisplay":events}))
                    }
                }),
            );
        let peer = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (account, rows) = fetch(&api, "fixture-key", 90, 190).await.unwrap();
        assert_eq!(account, hash("fixture-account"));
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .filter_map(|r| r.key.as_deref())
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        for r in rows {
            assert_eq!(r.cost, Some(0.02));
            assert_eq!(r.rate_model.as_deref(), Some("xai/grok-test"));
        }
        peer.abort();
    }
}
