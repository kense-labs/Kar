use super::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
impl ChatStore {
    pub(crate) fn changed_milestone_chats(
        &self,
        positions: &std::collections::BTreeMap<Uuid, u64>,
    ) -> Result<Vec<Uuid>> {
        let mut out = vec![];
        for row in std::fs::read_dir(self.directory())? {
            let row = row?;
            let path = row.path();
            if path.extension().and_then(|s| s.to_str()) != Some("karchat") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
            else {
                continue;
            };
            let length = row.metadata()?.len();
            if positions.get(&id) == Some(&length) {
                continue;
            }
            out.push(id);
        }
        Ok(out)
    }

    /// Scan only newly committed journal bytes. Ignore an interrupted append until recovery.
    pub(crate) fn milestone_rows(&self, id: Uuid, after: u64) -> Result<(u64, Vec<Value>)> {
        let mut f = File::open(self.path(id))?;
        log::with_lock(&mut f, false, |f| {
            f.seek(SeekFrom::Start(after))?;
            let mut reader = BufReader::new(f);
            let mut at = after;
            let mut line = vec![];
            let mut out = vec![];
            #[derive(Deserialize)]
            struct Row {
                _kar_milestones: Option<Stamp>,
            }
            #[derive(Deserialize)]
            struct Stamp {
                at: i64,
                events: Vec<Value>,
            }
            loop {
                line.clear();
                reader.read_until(b'\n', &mut line)?;
                if line.last() != Some(&b'\n') {
                    break;
                }
                at += line.len() as u64;
                if !line.windows(17).any(|w| w == b"\"_kar_milestones\"") {
                    continue;
                }
                if let Some(stamp) = serde_json::from_slice::<Row>(&line)?._kar_milestones {
                    for (i, mut v) in stamp.events.into_iter().enumerate() {
                        v["id"] = json!(format!("chat:{id}:{at}:{i}"));
                        v["threadId"] = json!(id);
                        v["at"] = json!(stamp.at);
                        out.push(v);
                    }
                }
            }
            Ok((at, out))
        })
    }
}
