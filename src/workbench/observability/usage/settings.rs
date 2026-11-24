//! Immutable revisions and request receipts share one atomically replaced file.
use super::*;
use axum::extract::Query;
use uuid::Uuid;
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Retention {
    Days(u32),
    Name(String),
}
impl Default for Retention {
    fn default() -> Self {
        Self::Days(92)
    }
}
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Override {
    input_cost_per_million_tokens: f64,
    output_cost_per_million_tokens: f64,
    cache_read_cost_per_million_tokens: Option<f64>,
    cache_write_cost_per_million_tokens: Option<f64>,
}
impl Override {
    pub fn rates(&self) -> [f64; 5] {
        let input = self.input_cost_per_million_tokens;
        let write = self.cache_write_cost_per_million_tokens.unwrap_or(input);
        [
            input,
            self.cache_read_cost_per_million_tokens.unwrap_or(input),
            write,
            write,
            self.output_cost_per_million_tokens,
        ]
        .map(|p| p / 1e6)
    }
}
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(in crate::workbench) struct Settings {
    history_retention_days: Retention,
    model_aliases: BTreeMap<String, String>,
    pub(super) price_overrides: BTreeMap<String, Override>,
}
impl Settings {
    fn validate(&self) -> Result<()> {
        match &self.history_retention_days {
            Retention::Days(n) if *n > 0 => {}
            Retention::Name(s) if s == "unlimited" => {}
            _ => {
                return Err(Error::Refused(
                    "usage retention must be positive days or unlimited".into(),
                ));
            }
        }
        for k in self
            .model_aliases
            .keys()
            .chain(self.model_aliases.values())
            .chain(self.price_overrides.keys())
        {
            if k.is_empty() || k.trim() != k || k.chars().any(char::is_control) {
                return Err(Error::Refused("invalid usage model name".into()));
            }
        }
        if self
            .price_overrides
            .values()
            .any(|p| p.rates().iter().any(|n| !n.is_finite() || *n < 0.))
        {
            return Err(Error::Refused(
                "usage rates must be nonnegative finite numbers".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn cutoff(&self, at: i64) -> i64 {
        match self.history_retention_days {
            Retention::Days(n) => at.saturating_sub(i64::from(n) * 86_400_000).max(0),
            Retention::Name(_) => 0,
        }
    }
    pub(super) fn aliases(&self) -> BTreeMap<&str, &str> {
        let mut memo: BTreeMap<&str, Option<&str>> = BTreeMap::new();
        for start in self.model_aliases.keys() {
            let mut chain = Vec::new();
            let mut target = start.as_str();
            while !memo.contains_key(target)
                && let Some(next) = self.model_aliases.get(target)
            {
                chain.push(target);
                memo.insert(target, None);
                target = next;
            }
            let resolved = memo.get(target).copied().unwrap_or(Some(target));
            for name in chain {
                memo.insert(name, resolved);
            }
        }
        memo.into_iter()
            .filter_map(|(k, v)| Some((k, v?)))
            .collect()
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(in crate::workbench) struct Version {
    pub revision: u64,
    pub settings: Settings,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    request_id: Uuid,
    expected_revision: u64,
    settings: Settings,
}
#[derive(Default, Serialize, Deserialize)]
struct Log {
    changes: Vec<Receipt>,
}
impl Log {
    fn version(&self, revision: Option<u64>) -> Result<Version> {
        let n = revision.unwrap_or(self.changes.len() as u64);
        if n == 0 {
            return Ok(Version::default());
        }
        let change = usize::try_from(n - 1)
            .ok()
            .and_then(|i| self.changes.get(i))
            .ok_or_else(|| Error::Io(std::io::ErrorKind::NotFound.into()))?;
        Ok(Version {
            revision: n,
            settings: change.settings.clone(),
        })
    }
}
pub(in crate::workbench) struct Store {
    file: PathBuf,
    gate: Mutex<()>,
}
impl Store {
    pub(super) fn new(dir: &Path) -> Self {
        Self {
            file: dir.join("usage-settings.json"),
            gate: Default::default(),
        }
    }
    fn log(&self) -> Result<Log> {
        match std::fs::File::open(&self.file) {
            Ok(f) => Ok(serde_json::from_reader(std::io::BufReader::new(f))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Log::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub(in crate::workbench) fn read(&self, revision: Option<u64>) -> Result<Version> {
        self.log()?.version(revision)
    }
    pub(super) fn with_cutoff<T>(&self, apply: impl FnOnce(i64) -> Result<T>) -> Result<T> {
        let _guard = self.gate.lock().unwrap();
        let cutoff = self.read(None)?.settings.cutoff(now());
        apply(cutoff)
    }
    fn put(&self, q: Update) -> Result<Version> {
        q.settings.validate()?;
        let _guard = self.gate.lock().unwrap();
        let mut log = self.log()?;
        if let Some((i, prior)) = log
            .changes
            .iter()
            .enumerate()
            .find(|(_, r)| r.request_id == q.request_id)
        {
            if prior.expected_revision != q.expected_revision || prior.settings != q.settings {
                return Err(Error::Refused(
                    "usage settings request ID already belongs to a different edit".into(),
                ));
            }
            return log.version(Some(i as u64 + 1));
        }
        if log.changes.len() as u64 != q.expected_revision {
            return Err(Error::Busy);
        }
        log.changes.push(Receipt {
            request_id: q.request_id,
            expected_revision: q.expected_revision,
            settings: q.settings,
        });
        let updated = log.version(None)?;
        super::super::triage::atomic(&self.file, &serde_json::to_vec(&log)?)?;
        #[cfg(unix)]
        std::fs::File::open(self.file.parent().expect("settings directory"))?.sync_all()?;
        Ok(updated)
    }
}
#[derive(Deserialize)]
pub(in crate::workbench::observability) struct Update {
    request_id: Uuid,
    expected_revision: u64,
    settings: Settings,
}
#[derive(Deserialize)]
pub(in crate::workbench::observability) struct At {
    revision: Option<u64>,
}
pub(in crate::workbench::observability) async fn get(
    State(app): State<Workbench>,
    Query(q): Query<At>,
) -> std::result::Result<Json<Version>, ApiError> {
    let service = app.0.observability.usage.clone();
    super::super::super::http::blocking(move || service.settings.read(q.revision)).await
}
pub(in crate::workbench::observability) async fn put(
    State(app): State<Workbench>,
    Json(q): Json<Update>,
) -> std::result::Result<Json<Version>, ApiError> {
    let service = app.0.observability.usage.clone();
    super::super::super::http::blocking(move || service.settings.put(q)).await
}
