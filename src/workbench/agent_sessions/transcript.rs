use super::discover::{self, Found, Source};
use crate::chat::{
    Role,
    import::native::{Message, Origin, Transcript},
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::{BufReader, Read},
    path::Path,
};
use uuid::Uuid;

const MAX_HISTORY: usize = 32 * 1024 * 1024;
const MAX_MESSAGES: usize = 200;
fn string(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}
fn extract(v: &Value) -> String {
    if let Some(s) = string(v) {
        return s.into();
    }
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|b| {
            matches!(
                b["type"].as_str(),
                Some("text" | "input_text" | "output_text")
            )
        })
        .filter_map(|b| string(&b["text"]))
        .collect::<Vec<_>>()
        .join("\n")
}
fn tag<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let (_, start) = s.split_once(&format!("<{key}>"))?;
    Some(start.split_once(&format!("</{key}>"))?.0.trim())
}
fn claude_input(s: &str) -> Option<(String, String)> {
    if [
        "<task-notification>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<local-command-caveat>",
        "<bash-stdout>",
        "<bash-stderr>",
    ]
    .iter()
    .any(|p| s.starts_with(p))
        || matches!(
            s,
            "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
        )
    {
        return None;
    }
    if let Some(t) = s
        .strip_prefix("<bash-input>")
        .and_then(|s| s.strip_suffix("</bash-input>"))
    {
        return Some((format!("!{t}"), String::new()));
    }
    if s.starts_with("<command-name>") || s.starts_with("<command-message>") {
        let name = tag(s, "command-name")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("/{}", tag(s, "command-message").unwrap_or_default()));
        let args = tag(s, "command-args").unwrap_or_default();
        return Some((
            if args.is_empty() {
                name
            } else {
                format!("{name} {args}")
            },
            args.into(),
        ));
    }
    Some((s.into(), s.into()))
}
fn title(s: &str, source: Source) -> Option<String> {
    let mut s = s.trim();
    if source == Source::Codex {
        loop {
            if let Some(rest) = s.strip_prefix("## My request for Codex:") {
                s = rest.trim_start();
                continue;
            }
            if s.starts_with("# AGENTS.md instructions") {
                s = s.split_once('\n').map_or("", |(_, r)| r).trim_start();
                continue;
            }
            let tag = [
                "environment_context",
                "recommended_plugins",
                "user_instructions",
                "INSTRUCTIONS",
                "in-app-browser-context",
                "external_codex_apps_open_page",
            ]
            .into_iter()
            .find(|tag| {
                s.strip_prefix(&format!("<{tag}"))
                    .is_some_and(|v| v.starts_with('>') || v.starts_with(char::is_whitespace))
            });
            let Some((_, after)) = tag.and_then(|tag| s.split_once(&format!("</{tag}>"))) else {
                break;
            };
            s = after.trim_start();
        }
    }
    if (s.starts_with("<wake>") || s.starts_with("<wake "))
        && let Some((_, body)) = s.split_once('>')
    {
        s = body.split("</wake>").next().unwrap_or(body).trim()
    }
    let mut width = 0;
    let result = s
        .lines()
        .next()?
        .chars()
        .take_while(|c| {
            width += c.len_utf16();
            width <= 100
        })
        .collect::<String>();
    (!result.trim().is_empty()).then(|| result.trim().to_owned())
}

struct Row {
    entry: Option<String>,
    parent: Option<String>,
    message: Option<Message>,
    title: Option<String>,
    event: bool,
    canonical_turn: Option<String>,
}
#[derive(Default)]
struct Projection {
    rows: Vec<Row>,
    id: Option<String>,
    model: Option<String>,
    name: Option<String>,
    custom: Option<String>,
    synthetic: bool,
    real: bool,
    base: Option<(String, u64)>,
    bytes: usize,
    dropped: usize,
}
impl Projection {
    fn append(&mut self, source: Source, row: &Value, at: u64) -> Option<()> {
        let ty = row["type"].as_str().unwrap_or_default();
        let p = &row["payload"];
        let m = &row["message"];
        let entry = string(&row["id"]).map(str::to_owned);
        let mut record = Row {
            entry,
            parent: string(&row["parentId"]).map(str::to_owned),
            message: None,
            title: None,
            event: false,
            canonical_turn: None,
        };
        let (role, text, provider_id) = match source {
            Source::Pi => {
                if ty == "session" {
                    if self.id.is_none() {
                        self.id = string(&row["id"]).map(str::to_owned);
                    }
                    return Some(());
                }
                if ty == "session_info" {
                    self.name = string(&row["name"]).map(str::to_owned);
                }
                if ty == "message" {
                    (
                        m["role"].as_str(),
                        extract(&m["content"]),
                        record.entry.clone(),
                    )
                } else {
                    (None, String::new(), None)
                }
            }
            Source::Claude => {
                if ["isSidechain", "isMeta", "isCompactSummary"]
                    .iter()
                    .any(|k| row[*k] == true)
                {
                    return Some(());
                }
                if let Some(id) = string(&row["sessionId"]) {
                    self.id = Some(id.into());
                }
                fn saved_title(v: &Value) -> Option<String> {
                    let text = string(v)?;
                    claude_input(text)
                        .filter(|(t, _)| t == text)
                        .map(|(t, _)| t)
                }
                if let Some(name) = saved_title(&row["aiTitle"]) {
                    self.name = Some(name);
                }
                if row.get("customTitle").is_some() {
                    self.custom = saved_title(&row["customTitle"]);
                }
                if let Some(model) = string(&m["model"]).filter(|s| *s != "<synthetic>") {
                    self.model = Some(model.into())
                }
                let mut text = extract(&m["content"]);
                if ty == "user" {
                    let Some((actual, for_title)) = claude_input(&text) else {
                        return Some(());
                    };
                    text = actual;
                    record.title = title(&for_title, source);
                } else if ty == "assistant" && !text.is_empty() {
                    if m["model"] == "<synthetic>" {
                        self.synthetic = true
                    } else {
                        self.real = true
                    }
                }
                (Some(ty), text, string(&row["uuid"]).map(str::to_owned))
            }
            Source::Codex => {
                if ty == "session_meta" && self.id.is_none() {
                    self.id = string(&p["id"])
                        .or_else(|| string(&p["session_id"]))
                        .map(str::to_owned);
                    self.base = string(&p["history_base"]["thread_id"])
                        .zip(p["history_base"]["end_byte_offset"].as_u64())
                        .map(|(s, n)| (s.into(), n));
                }
                if ty == "turn_context"
                    && let Some(model) = string(&p["model"])
                {
                    self.model = Some(model.into());
                }
                if ty == "event_msg" && p["type"] == "user_message" {
                    record.event = true;
                    (
                        Some("user"),
                        p["message"].as_str().unwrap_or_default().to_owned(),
                        None,
                    )
                } else if ty == "response_item" && p["type"] == "message" {
                    let meta = &p["internal_chat_message_metadata_passthrough"];
                    let parsed = meta
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok());
                    record.canonical_turn =
                        string(&parsed.as_ref().unwrap_or(meta)["turn_id"]).map(str::to_owned);
                    (
                        p["role"].as_str(),
                        extract(&p["content"]),
                        string(&p["id"]).map(str::to_owned),
                    )
                } else {
                    return Some(());
                }
            }
        };
        let role = match role {
            Some("user") => Some(Role::User),
            Some("assistant") => Some(Role::Assistant),
            _ => None,
        };
        if let Some(role) = role
            && !text.trim().is_empty()
        {
            if role == Role::User && source != Source::Claude {
                record.title = title(&text, source);
            }
            self.bytes = self.bytes.checked_add(text.len())?;
            if self.bytes > MAX_HISTORY {
                return None;
            }
            record.message = Some(Message {
                role,
                text,
                at,
                provider_id,
            });
        }
        if source == Source::Pi || record.message.is_some() {
            self.rows.push(record);
            if source == Source::Claude && self.rows.len() > MAX_MESSAGES {
                let keep_first = self.rows[0]
                    .message
                    .as_ref()
                    .is_some_and(|m| m.role == Role::User);
                let evicted = self.rows.remove(usize::from(keep_first));
                self.bytes -= evicted.message.map_or(0, |m| m.text.len());
                self.dropped += 1;
            }
        }
        Some(())
    }
    fn active_pi(&mut self) -> Option<()> {
        if self.rows.last()?.entry.is_none() {
            return Some(());
        }
        let mut ids = HashMap::new();
        for (i, r) in self.rows.iter().enumerate() {
            if let Some(id) = &r.entry
                && ids.insert(id.as_str(), i).is_some()
            {
                return None;
            }
        }
        let mut path = Vec::new();
        let mut next = Some(self.rows.len() - 1);
        let mut seen = HashSet::new();
        while let Some(i) = next {
            if !seen.insert(i) {
                return None;
            }
            path.push(i);
            next = self.rows[i]
                .parent
                .as_deref()
                .and_then(|p| ids.get(p))
                .copied();
        }
        let mut entries = std::mem::take(&mut self.rows)
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        self.rows = path
            .into_iter()
            .rev()
            .filter_map(|i| entries[i].take())
            .collect();
        Some(())
    }
    fn dedup_codex(&mut self) {
        for group in self.rows.split_inclusive_mut(|r| {
            r.message
                .as_ref()
                .is_some_and(|m| m.role == Role::Assistant)
        }) {
            let canonical = group
                .iter()
                .filter(|r| r.event)
                .filter_map(|r| Some(r.message.as_ref()?.text.trim().to_owned()))
                .collect::<HashSet<_>>();
            let ids = group
                .iter()
                .filter(|r| {
                    !r.event
                        && r.message.as_ref().is_some_and(|m| {
                            m.role == Role::User && canonical.contains(m.text.trim())
                        })
                })
                .filter_map(|r| r.canonical_turn.clone())
                .collect::<HashSet<_>>();
            for row in group {
                if !row.event
                    && row.message.as_ref().is_some_and(|m| {
                        m.role == Role::User
                            && (canonical.contains(m.text.trim())
                                || row
                                    .canonical_turn
                                    .as_ref()
                                    .is_some_and(|id| ids.contains(id)))
                    })
                {
                    row.message = None;
                }
            }
        }
    }
}
fn read(found: &Found, offset: Option<u64>, budget: &mut u64) -> Option<Projection> {
    let file = found.file().ok()?;
    let limit = offset.unwrap_or(u64::MAX).min(*budget);
    let stream = discover::input(&file, &found.path).ok()?;
    let mut reader = BufReader::new(stream.take(offset.unwrap_or(u64::MAX)));
    let mut projection = Projection::default();
    let mut bytes = 0;
    let mut first_cwd = false;
    loop {
        let (n, row) = super::jsonl::record(&mut reader, budget, projection.bytes).ok()?;
        if n == 0 {
            break;
        }
        bytes += n;
        if bytes > limit {
            return None;
        }
        let Some(row) = row else { continue };
        if !first_cwd && let Some(path) = discover::cwd(&row, found.source).ok()? {
            if path != found.original_cwd {
                return None;
            }
            first_cwd = true;
        }
        let at = row["timestamp"]
            .as_str()
            .and_then(|v| v.parse::<jiff::Timestamp>().ok())
            .and_then(|v| v.as_millisecond().try_into().ok())
            .unwrap_or(found.modified);
        projection.append(found.source, &row, at)?;
        if projection.rows.len() > 100_000 {
            return None;
        }
    }
    if !first_cwd || offset.is_some_and(|o| o != bytes) {
        return None;
    }
    found.check(&file).ok()?;
    Some(projection)
}
pub(super) enum Parsed {
    Empty,
    Ready(Box<Transcript>),
}
pub(super) fn parse(
    found: &Found,
    bases: &HashMap<(String, String), &Found>,
    budget: &mut u64,
) -> Option<Parsed> {
    let mut data = read(found, None, budget)?;
    if found.source == Source::Codex {
        let mut inherited = Vec::new();
        let mut seen = HashSet::new();
        let mut bytes = data.bytes;
        let mut base = data.base.take();
        while let Some((id, end)) = base {
            if !seen.insert(id.clone()) {
                inherited.clear();
                break;
            }
            let prev = bases
                .get(&(found.instance.id.clone(), id))
                .filter(|f| f.project == found.project);
            let Some(mut old) = prev.and_then(|f| read(f, Some(end), budget)) else {
                inherited.clear();
                break;
            };
            bytes = bytes.checked_add(old.bytes)?;
            if bytes > MAX_HISTORY {
                return None;
            }
            base = old.base.take();
            inherited.push(old.rows);
        }
        let current = std::mem::take(&mut data.rows);
        data.rows = inherited
            .into_iter()
            .rev()
            .flatten()
            .chain(current)
            .collect();
        data.dedup_codex()
    }
    if found.source == Source::Pi && !data.rows.is_empty() {
        data.active_pi()?
    }
    if data.rows.iter().all(|r| r.message.is_none()) && data.dropped == 0 {
        return Some(Parsed::Empty);
    }
    let native_id = data.id.or_else(|| {
        (found.source == Source::Claude)
            .then(|| found.path.file_stem()?.to_str().map(str::to_owned))
            .flatten()
    })?;
    if found.source == Source::Claude {
        let uuid = Uuid::parse_str(&native_id).ok()?;
        if uuid.hyphenated().to_string() != native_id.to_lowercase()
            || !(1..=8).contains(&uuid.get_version_num())
            || uuid.get_variant() != uuid::Variant::RFC4122
            || (data.dropped == 0 && data.synthetic && !data.real)
        {
            return None;
        }
    }
    let derived = data
        .rows
        .iter()
        .filter(|r| r.message.is_some())
        .find_map(|r| r.title.clone());
    if found.source == Source::Claude && derived.is_none() && !data.real && !data.synthetic {
        return None;
    }
    let mut messages: Vec<_> = data.rows.into_iter().filter_map(|r| r.message).collect();
    let user = messages.iter().position(|m| m.role == Role::User)?;
    let name = found
        .codex_title
        .clone()
        .or(data.custom)
        .or(data.name)
        .or(derived)
        .or_else(|| title(&messages[user].text, found.source))
        .unwrap_or_else(|| "Imported chat".into());
    let omitted = messages.len().saturating_sub(MAX_MESSAGES);
    if omitted > 0 {
        if user < omitted {
            let first = messages.remove(user);
            messages.drain(..omitted);
            messages.insert(0, first);
        } else {
            messages.drain(..omitted);
        }
    }
    Some(Parsed::Ready(Box::new(Transcript {
        workspace: found.cwd.clone(),
        model: data.model,
        origin: Origin {
            instance: Some((*found.instance).clone()),
            project: None,
            runtime: found.source.runtime().into(),
            native_id,
            transcript: found.path.clone(),
            title: name,
            omitted_messages: omitted + data.dropped,
            stamp: found.instance.receipt(&found.stamp),
        },
        messages,
    })))
}

pub(super) fn rollout(path: &Path) -> Option<&str> {
    let filename = path
        .file_name()?
        .to_str()?
        .strip_suffix(".zst")
        .or_else(|| path.file_name()?.to_str())?
        .strip_suffix(".jsonl")?;
    let end = filename.get(filename.len().checked_sub(36)?..)?;
    Uuid::parse_str(end).ok()?;
    Some(end)
}
