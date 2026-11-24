use super::*;
#[derive(Default, Clone, Serialize, Deserialize)]
pub(in super::super) struct Identity {
    model_id: u64,
    model_quality: u8,
    time_quality: u8,
    keys: Vec<String>,
    label_only: bool,
}
#[derive(Clone, Copy)]
enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}
#[derive(Default)]
struct Fields<'a>(BTreeMap<u32, Vec<Field<'a>>>);
fn varint(bytes: &[u8], offset: &mut usize) -> io::Result<u64> {
    let mut value = 0;
    for shift in (0..=63).step_by(7) {
        let b = *bytes
            .get(*offset)
            .ok_or_else(|| invalid("truncated Antigravity varint"))?;
        *offset += 1;
        if shift == 63 && b > 1 {
            return Err(invalid("invalid Antigravity varint"));
        }
        value |= u64::from(b & 127) << shift;
        if b < 128 {
            return Ok(value);
        }
    }
    Err(invalid("overflowing Antigravity varint"))
}
impl<'a> Fields<'a> {
    fn decode(bytes: &'a [u8]) -> io::Result<Self> {
        let mut at = 0;
        let mut result = Self::default();
        while at < bytes.len() {
            let tag = varint(bytes, &mut at)?;
            let number = u32::try_from(tag >> 3)
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid("invalid Antigravity tag"))?;
            let field = match tag & 7 {
                0 => Field::Int(varint(bytes, &mut at)?),
                wire @ (1 | 2 | 5) => {
                    let len = match wire {
                        1 => 8,
                        5 => 4,
                        _ => usize::try_from(varint(bytes, &mut at)?)
                            .map_err(|_| invalid("overflowing Antigravity field"))?,
                    };
                    let end = at
                        .checked_add(len)
                        .filter(|n| *n <= bytes.len())
                        .ok_or_else(|| invalid("truncated Antigravity field"))?;
                    let slice = &bytes[at..end];
                    at = end;
                    if wire != 2 {
                        continue;
                    }
                    Field::Bytes(slice)
                }
                _ => return Err(invalid("unsupported Antigravity wire type")),
            };
            result.0.entry(number).or_default().push(field);
        }
        Ok(result)
    }
    fn int(&self, k: u32) -> u64 {
        match self.0.get(&k).and_then(|a| a.first()) {
            Some(Field::Int(v)) => *v,
            _ => 0,
        }
    }
    fn bytes(&self, k: u32) -> Option<&'a [u8]> {
        match self.0.get(&k)?.first()? {
            Field::Bytes(v) => Some(*v),
            _ => None,
        }
    }
    fn nested(&self, k: u32) -> io::Result<Self> {
        Self::decode(self.bytes(k).unwrap_or(&[]))
    }
    fn text(&self, k: u32) -> io::Result<&str> {
        std::str::from_utf8(self.bytes(k).unwrap_or(&[]))
            .map(str::trim)
            .map_err(|_| invalid("invalid Antigravity text"))
    }
    fn timestamp(&self) -> Option<i64> {
        let s = self.int(1);
        if s == 0 {
            return None;
        }
        i64::try_from(s.checked_mul(1000)?.checked_add(self.int(2) / 1_000_000)?).ok()
    }
}
fn normalize(name: &str) -> String {
    use std::sync::LazyLock;
    static VARIANT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\s*\(([^)]*)\)\s*$").unwrap());
    static CLAUDE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^claude-(\d+(?:\.\d+)?)-(sonnet|opus|haiku)").unwrap()
    });
    let lower = name.to_lowercase();
    let s = VARIANT
        .replace(&lower, "-$1")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    if s.starts_with("claude-") {
        CLAUDE.replace(&s, "claude-$2-$1").replace('.', "-")
    } else {
        s
    }
}
fn concrete(m: &str) -> bool {
    !matches!(
        m,
        "gemini-3-flash-agent"
            | "gemini-3-flash-agent-a"
            | "gemini-3-flash-agent-b"
            | "gemini-3-flash-a"
            | "gemini-3-flash-b"
    ) && (m.starts_with("gpt-")
        || ["gemini-", "claude-sonnet-", "claude-opus-", "claude-haiku-"]
            .iter()
            .any(|p| {
                m.strip_prefix(p)
                    .is_some_and(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            }))
}
fn rate(m: &str) -> String {
    match m {
        "gemini-3-flash-agent"
        | "gemini-3-flash-agent-a"
        | "gemini-3-flash-agent-b"
        | "gemini-3-flash-a"
        | "gemini-3-flash-b" => "gemini-3.5-flash".into(),
        "gemini-3.7-flash-safety-le" => "gemini-3.7-flash".into(),
        "gemini-pro-orchestrator-neon" => "gemini-4-neon".into(),
        "gemini-pro-orchestrator-g1-pro" => "gemini-4-argon".into(),
        _ => {
            let base = [
                "-extra-low",
                "-low",
                "-medium",
                "-high",
                "-tiered",
                "-control",
                "-thinking",
                "-agent",
                "-orchestrator",
            ]
            .into_iter()
            .find_map(|p| m.strip_suffix(p))
            .unwrap_or(m);
            if base == "gemini-3-flash" {
                "gemini-3-flash-preview".into()
            } else {
                base.into()
            }
        }
    }
}
struct Sample {
    model: u64,
    tokens: Totals,
    keys: Vec<String>,
}
fn sample(v: Fields<'_>) -> io::Result<Sample> {
    let output = v.int(3).max(v.int(9).saturating_add(v.int(10)));
    let mut keys = vec![];
    for k in [11, 12, 7] {
        let s = v.text(k)?;
        if !s.is_empty() {
            keys.push(format!("{k}:{s}"));
        }
    }
    Ok(Sample {
        model: v.int(1),
        tokens: Totals::from_array([v.int(2), v.int(5), v.int(4), output, output.min(v.int(9))]),
        keys,
    })
}
struct Metadata {
    id: u64,
    model: String,
    at: Option<i64>,
    samples: Vec<Sample>,
}
fn metadata(bytes: &[u8], step: bool) -> io::Result<Metadata> {
    let root = Fields::decode(bytes)?;
    if !step && root.bytes(1).is_none() {
        return Err(invalid("missing Antigravity generation metadata"));
    }
    let data = if step { root } else { root.nested(1)? };
    let inner = if step { Some(data.nested(24)?) } else { None };
    let model = inner.as_ref().unwrap_or(&data);
    let names = [
        normalize(model.text(if step { 12 } else { 19 })?),
        normalize(model.text(if step { 8 } else { 21 })?),
    ];
    let name = names
        .iter()
        .find(|m| concrete(m))
        .or_else(|| {
            names
                .iter()
                .find(|m| !m.is_empty() && !m.starts_with("model-") && !m.starts_with("model_"))
        })
        .cloned()
        .unwrap_or_default();
    let mut samples = vec![];
    if let Some(b) = data.bytes(if step { 9 } else { 4 }) {
        samples.push(sample(Fields::decode(b)?)?);
    }
    let id = match model.int(if step { 1 } else { 3 }) {
        0 => samples.first().map_or(0, |s| s.model),
        i => i,
    };
    for b in data
        .0
        .get(&(if step { 28 } else { 17 }))
        .into_iter()
        .flatten()
    {
        let Field::Bytes(b) = b else {
            return Err(invalid("invalid Antigravity retry metadata"));
        };
        if let Some(u) = Fields::decode(b)?.bytes(2) {
            samples.push(sample(Fields::decode(u)?)?);
        }
    }
    let at = if step {
        data.nested(8)?.timestamp().or(data.nested(1)?.timestamp())
    } else {
        data.nested(9)?.nested(4)?.timestamp()
    };
    Ok(Metadata {
        id,
        model: name,
        at,
        samples,
    })
}
pub(super) fn rows(db: &Tables, path: &Path) -> io::Result<Vec<Entry>> {
    let session = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mtime = fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mut traj = None;
    db.read("trajectory_metadata_blob", |c, r| {
        if let Some(SqlValue::Blob(b)) = c.get(&r, "data")
            && traj.is_none()
        {
            traj = Fields::decode(b)?.nested(2)?.timestamp();
        }
        Ok(())
    })?;
    let mut sources = [BTreeMap::new(), BTreeMap::new()];
    for (index, table) in ["steps", "gen_metadata"].into_iter().enumerate() {
        let step = index == 0;
        db.read(table, |c, r| {
            let field = c.get(&r, if step { "metadata" } else { "data" });
            if step && matches!(field, None | Some(SqlValue::Null)) {
                return Ok(());
            }
            let Some(SqlValue::Blob(bytes)) = field else {
                return Err(invalid("missing Antigravity blob"));
            };
            let idx = c.int(&r, "idx").unwrap_or(r.rowid);
            sources[index].insert(idx, metadata(bytes, step)?);
            Ok(())
        })?;
    }
    let mut rows = Vec::new();
    for (source, items) in sources.iter().enumerate() {
        for (idx, m) in items {
            if m.id != 0 && concrete(&m.model) {
                rows.push(Entry {
                    model: m.model.clone(),
                    native_meta: Some(Identity {
                        model_id: m.id,
                        label_only: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            for (i, v) in m.samples.iter().enumerate() {
                if v.tokens.sum() == 0 {
                    continue;
                }
                let id = if v.model == 0 { m.id } else { v.model };
                let name = std::iter::once(m)
                    .chain((source == 0).then(|| sources[1].get(idx)).flatten())
                    .find(|m| (id == 0 || m.id == id) && !m.model.is_empty())
                    .map_or("", |m| &m.model);
                rows.push(Entry {
                    provider: "antigravity".into(),
                    model: name.into(),
                    session: session.into(),
                    timestamp: m.at.or(traj).unwrap_or(mtime),
                    tokens: v.tokens,
                    key: Some(
                        v.keys
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{session}:{source}:{idx}:{i}")),
                    ),
                    native_meta: Some(Identity {
                        model_id: id,
                        keys: v.keys.clone(),
                        time_quality: if m.at.is_some() {
                            2
                        } else {
                            u8::from(traj.is_some())
                        },
                        model_quality: if concrete(name) {
                            4
                        } else {
                            u8::from(!name.is_empty())
                        },
                        label_only: false,
                    }),
                    ..Default::default()
                });
            }
        }
    }
    Ok(rows)
}
fn fallback(id: u64) -> Option<&'static str> {
    Some(match id {
        246 => "gemini-2.5-pro",
        312 => "gemini-2.5-flash",
        313 => "gemini-2.5-flash-thinking",
        329 => "gemini-2.5-flash-thinking",
        330 => "gemini-2.5-flash-lite",
        281 => "claude-sonnet-4",
        282 => "claude-sonnet-4",
        290 => "claude-opus-4",
        291 => "claude-opus-4",
        333 => "claude-sonnet-4-5",
        334 => "claude-sonnet-4-5",
        340 => "claude-haiku-4-5",
        341 => "claude-haiku-4-5",
        342 => "gpt-oss-120b-medium",
        1026 => "claude-opus-4-6",
        1035 => "claude-sonnet-4-6",
        1016 => "gemini-3.1-pro",
        1036 => "gemini-3.1-pro",
        1037 => "gemini-3.1-pro",
        1018 => "gemini-3-flash-preview",
        1084 => "gemini-3-flash-preview",
        1047 => "gemini-3-flash-preview",
        1020 => "gemini-3.5-flash-medium",
        1021 => "gemini-3.1-flash-image",
        1050 => "gemini-3.1-flash-lite",
        1071 => "gemini-3.6-flash-high",
        1072 => "gemini-3.6-flash-medium",
        1073 => "gemini-3.6-flash-low",
        1132 => "gemini-3.5-flash-high",
        1133 => "gemini-3.5-flash-high",
        1187 => "gemini-3.5-flash-extra-low",
        1196 => "gemini-3.6-flash",
        1266 => "gemini-3.6-flash",
        1298 => "gemini-3.7-flash-high",
        1299 => "gemini-3.7-flash-medium",
        1300 => "gemini-3.7-flash-low",
        1301 => "gemini-3.7-flash-tiered",
        1318 => "gemini-3.8-flash-high",
        1319 => "gemini-3.8-flash-medium",
        1320 => "gemini-3.8-flash-low",
        1322 => "gemini-3.8-flash-tiered",
        1402 => "claude-opus-5-5",
        1417 => "gemini-4-argon",
        1418 => "gemini-4-argon",
        1419 => "gemini-4-argon",
        1537 => "gemini-pro-orchestrator-neon",
        _ => return None,
    })
}

pub(in super::super) fn merged(
    files: &BTreeMap<PathBuf, Arc<scans::FileUsage>>,
) -> BTreeMap<PathBuf, Vec<Entry>> {
    // One inference can carry up to three identities. A bridging row connects copies that
    // never shared their first ID. Retain the first source, best model and best-dated sample.
    let mut learned: BTreeMap<u64, Option<&str>> = BTreeMap::new();
    let mut rows = Vec::new();
    let mut parents = vec![];
    let mut keys = BTreeMap::new();
    fn find(parents: &mut [usize], mut i: usize) -> usize {
        while parents[i] != i {
            parents[i] = parents[parents[i]];
            i = parents[i];
        }
        i
    }
    for (path, f) in files.iter().filter(|(_, f)| f.format == "antigravity") {
        for r in &f.entries {
            let Some(m) = &r.native_meta else { continue };
            if m.label_only {
                learned
                    .entry(m.model_id)
                    .and_modify(|e| {
                        if *e != Some(&r.model) {
                            *e = None;
                        }
                    })
                    .or_insert(Some(&r.model));
                continue;
            }
            let i = rows.len();
            rows.push((path, r, m));
            parents.push(i);
            for key in &m.keys {
                if let Some(j) = keys.insert(key.as_str(), i) {
                    let a = find(&mut parents, i);
                    let b = find(&mut parents, j);
                    parents[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, (&PathBuf, Entry, u8, u8)> = BTreeMap::new();
    for (i, (path, r, m)) in rows.into_iter().enumerate() {
        let group = find(&mut parents, i);
        let mut e = r.clone();
        let learned = learned.get(&m.model_id);
        let (quality, model) = if m.model_quality == 4 {
            (4, e.model.clone())
        } else if let Some(Some(s)) = learned {
            (3, (*s).into())
        } else if learned.is_none()
            && let Some(s) = fallback(m.model_id)
        {
            (2, s.into())
        } else if !e.model.is_empty() {
            (1, e.model.clone())
        } else if m.model_id > 0 {
            (0, format!("antigravity-model-{}", m.model_id))
        } else {
            (0, "antigravity-unknown".into())
        };
        e.model = model;
        e.native_meta = None;
        groups
            .entry(group)
            .and_modify(|(_, a, mq, tq)| {
                a.tokens = a.tokens.zip(e.tokens, u64::max);
                if quality > *mq {
                    *mq = quality;
                    a.model = e.model.clone();
                }
                if m.time_quality > *tq || (m.time_quality == *tq && e.timestamp < a.timestamp) {
                    *tq = m.time_quality;
                    a.timestamp = e.timestamp;
                }
            })
            .or_insert((path, e, quality, m.time_quality));
    }
    let mut out: BTreeMap<PathBuf, Vec<Entry>> = BTreeMap::new();
    for (_, (path, mut e, _, _)) in groups {
        let price = rate(&e.model);
        if price != e.model {
            e.rate_model = Some(price);
        }
        out.entry(path.clone()).or_default().push(e);
    }
    out
}
