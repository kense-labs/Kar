use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
#[derive(Clone, Default, Serialize, Deserialize)]
pub(in crate::workbench) struct Log {
    seq: u64,
    rows: VecDeque<(u64, Value)>,
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    seen: VecDeque<String>,
}
impl Log {
    pub(in crate::workbench) fn push(&mut self, row: Value) {
        if let Some(id) = row["id"].as_str().filter(|s| !s.is_empty()) {
            if self.seen.iter().any(|s| s == id)
                || self.rows.iter().any(|(_, v)| v["id"].as_str() == Some(id))
            {
                return;
            }
            self.seen.push_back(id.to_owned());
            if self.seen.len() > 4096 {
                self.seen.pop_front();
            }
        }
        self.seq += 1;
        self.rows.push_back((self.seq, row));
        if self.rows.len() > 1000 {
            self.rows.pop_front();
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Index {
    epoch: Uuid,
    log: Log,
    positions: BTreeMap<Uuid, u64>,
    other: [u64; 3],
    gap: u64,
}
pub(in crate::workbench) struct Hub {
    file: PathBuf,
    index: Mutex<Index>,
}
impl Hub {
    pub(in crate::workbench) fn open(dir: &Path) -> crate::Result<Self> {
        let file = dir.join("milestones.json");
        let i = match std::fs::File::open(&file) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Index {
                epoch: Uuid::new_v4(),
                log: Log::default(),
                positions: BTreeMap::new(),
                other: [0; 3],
                gap: 0,
            },
            Err(e) => return Err(e.into()),
        };
        let hub = Self {
            file,
            index: Mutex::new(i),
        };
        if !hub.file.exists() {
            hub.save(&hub.index.lock().unwrap())?;
        }
        Ok(hub)
    }
    fn save(&self, i: &Index) -> crate::Result<()> {
        let dir = self.file.parent().unwrap();
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut f, i)?;
        f.flush()?;
        f.as_file().sync_all()?;
        f.persist(&self.file).map_err(|e| e.error)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
    pub(super) fn read(&self, app: &Workbench, p: &Params) -> crate::Result<Page> {
        let mut lock = self.index.lock().unwrap();
        let mut next = lock.clone();
        let mut changed = false;
        let mut rows = vec![];
        for id in app.0.chats.changed_milestone_chats(&next.positions)? {
            let pos = next.positions.get(&id).copied().unwrap_or(0);
            let (end, events) = app.0.chats.milestone_rows(id, pos)?;
            if end != pos {
                changed = true;
                next.positions.insert(id, end);
            }
            if events.is_empty() {
                continue;
            }
            let info = app.0.chats.saved_info(id)?;
            let (project, parent) = {
                let db = app.0.catalog.lock().unwrap();
                let project = db
                    .projects
                    .values()
                    .filter(|p| app.owns_chat(&db, p, &info).unwrap_or(false))
                    .map(|p| p.id)
                    .next();
                let parent = crate::workbench::mcp::orchestrator::milestone_parent(&db, id);
                (project, parent)
            };
            let Some(project) = project else { continue };
            let workflow = app.0.workflows.milestone_run(id);
            for mut v in events {
                v["projectId"] = json!(project);
                v["parentThreadId"] = json!(parent);
                v["isSubagentChild"] = json!(parent.is_some());
                if let Some(run) = workflow {
                    v["workflowRunId"] = json!(run);
                }
                rows.push(v);
            }
        }
        let sources = [
            app.0.workflows.milestone_log(),
            app.0.access.milestone_log(),
            app.0.catalog.lock().unwrap().milestone_watches.clone(),
        ];
        for (index, log) in sources.iter().enumerate() {
            let at = next.other[index];
            if at == log.seq {
                continue;
            }
            if log.rows.front().is_some_and(|(n, _)| *n > at + 1) {
                next.gap = next.log.seq.saturating_add(1);
            }
            rows.extend(
                log.rows
                    .iter()
                    .filter(|(n, _)| *n > at)
                    .map(|(_, v)| v.clone()),
            );
            next.other[index] = log.seq;
            changed = true;
        }
        rows.sort_by_key(|v| v["at"].as_i64().unwrap_or(0));
        for mut v in rows {
            let at = v["at"].as_i64().unwrap_or(0);
            v.as_object_mut().unwrap().remove("at");
            v["occurredAt"] = json!(
                jiff::Timestamp::from_millisecond(at)
                    .map_err(|e| crate::Error::Protocol(e.to_string()))?
                    .to_string()
            );
            if v.get("isSubagentChild").is_none()
                && let Some(chat) = v["threadId"].as_str().and_then(|s| s.parse::<Uuid>().ok())
            {
                let parent = crate::workbench::mcp::orchestrator::milestone_parent(
                    &app.0.catalog.lock().unwrap(),
                    chat,
                );
                v["parentThreadId"] = json!(parent);
                v["isSubagentChild"] = json!(parent.is_some());
                if let Some(run) = app.0.workflows.milestone_run(chat) {
                    v["workflowRunId"] = json!(run);
                }
            }
            let d = v.as_object_mut().unwrap();
            for key in ["projectId", "threadId", "parentThreadId"] {
                d.entry(key).or_insert(Value::Null);
            }
            d.entry("isSubagentChild").or_insert(json!(false));
            d.entry("actor").or_insert(json!({"type":"system"}));
            next.log.push(v);
        }
        if changed {
            self.save(&next)?;
            *lock = next;
        }
        let curr = &*lock;
        let fmt = |n: u64| format!("{}.{n}", curr.epoch);
        let edge = curr.log.seq;
        let Some(cursor) = &p.cursor else {
            return Ok(Page {
                cursor: fmt(edge),
                events: vec![],
                truncated: false,
                has_more: false,
            });
        };
        let seq = cursor
            .rsplit_once('.')
            .and_then(|(epoch, n)| {
                (epoch == curr.epoch.to_string())
                    .then(|| n.parse::<u64>().ok())
                    .flatten()
            })
            .filter(|n| *n <= edge);
        let mut truncated = seq.is_none()
            || seq.is_some_and(|n| {
                n < curr.gap
                    || curr
                        .log
                        .rows
                        .front()
                        .is_some_and(|(i, _)| n < i.saturating_sub(1))
            });
        let floor = p.max_age_ms.map(|m| {
            jiff::Timestamp::now()
                .as_millisecond()
                .saturating_sub(m as i64)
        });
        let mut events = vec![];
        let mut last = seq.unwrap_or(0);
        let mut more = false;
        for (n, v) in curr.log.rows.iter().filter(|(n, _)| *n > seq.unwrap_or(0)) {
            if floor.is_some_and(|floor| {
                v["occurredAt"]
                    .as_str()
                    .and_then(|s| s.parse::<jiff::Timestamp>().ok())
                    .is_some_and(|t| t.as_millisecond() < floor)
            }) {
                truncated = true;
                continue;
            }
            if p.matches(v) {
                if events.len() >= p.limit() {
                    more = true;
                    break;
                }
                last = *n;
                events.push((
                    fmt(*n),
                    json!({"eventId":v["id"],"timestamp":v["occurredAt"],"data":v}),
                ));
            }
        }
        Ok(Page {
            cursor: fmt(if more { last } else { edge }),
            events,
            truncated,
            has_more: more,
        })
    }
}
