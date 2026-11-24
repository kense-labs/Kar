use crate::native_file::open_regular as regular;
use crate::{Error, ModelRequest, Result};
use jiff::Timestamp;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const PREFIX: &str = "kar-rs-claude-requests-";
const SIGNATURE: &[u8] = b"kar-rs Claude request capture v1\n";

pub(super) type Directory = crate::private_dir::PrivateDir;
pub(super) fn directory(cwd: &Path) -> Result<Directory> {
    let root = std::env::temp_dir().canonicalize()?;
    if root.starts_with(cwd) {
        return Err(Error::Unsupported(
            "Claude request capture requires a temporary directory outside the workspace".into(),
        ));
    }
    create_directory(&root)
}
pub(super) fn create_directory(root: &Path) -> Result<Directory> {
    Directory::create(root, PREFIX, SIGNATURE)
}

fn native_name<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let stem = value.strip_suffix(suffix)?;
    (!stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
    .then_some(value)
}
pub(super) struct Entry {
    pub id: String,
    pub at: Option<Timestamp>,
    pub model: Option<String>,
    request: Option<String>,
    response: String,
}
impl Entry {
    pub fn from_row(row: &Value) -> Option<Self> {
        let request = match row.get("request_file") {
            None | Some(Value::Null) => None,
            Some(s) => Some(native_name(s.as_str()?, ".request.json")?.to_owned()),
        };
        Some(Self {
            id: row["message_id"].as_str()?.to_owned(),
            at: row["timestamp"].as_str().and_then(|s| s.parse().ok()),
            model: row["model"].as_str().map(String::from),
            request,
            response: native_name(row["response_file"].as_str()?, ".response.json")?.to_owned(),
        })
    }
    fn body_name(&self) -> &str {
        self.request.as_ref().unwrap_or(&self.response)
    }
    pub fn read(&self, dir: &Path, secrets: &[String]) -> Option<ModelRequest> {
        let file = regular(&dir.join(self.body_name())).ok()?;
        let at = Timestamp::try_from(file.metadata().ok()?.modified().ok()?)
            .ok()?
            .to_string();
        let mut value: Value = serde_json::from_reader(BufReader::new(file)).ok()?;
        value.as_object()?;
        let model = value["model"]
            .as_str()
            .or(self.model.as_deref())
            .map(|s| crate::model_request::masked(s, secrets));
        let (body, withheld) = if self.request.is_none() {
            (None, vec!["not_captured".into()])
        } else if value.get("not_recorded").is_some() {
            (None, vec!["body".into()])
        } else {
            let hidden = value["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|v| v["content"].as_array().into_iter().flatten())
                .any(|b| b["type"] == "thinking" && b["thinking"] == "<REDACTED>");
            crate::model_request::mask_body(&mut value, secrets);
            (
                Some(value),
                if hidden {
                    vec!["thinking".into()]
                } else {
                    vec![]
                },
            )
        };
        Some(ModelRequest {
            at,
            format: "anthropic.messages".into(),
            model,
            body,
            withheld,
            provider: None,
        })
    }
}
pub(super) struct Files {
    pub dir: PathBuf,
    offset: u64,
    pub latest: Option<Entry>,
    retired: HashSet<String>,
}
impl Files {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.into(),
            offset: 0,
            latest: None,
            retired: Default::default(),
        }
    }
    pub fn rows(&mut self) -> io::Result<Vec<Value>> {
        let mut file = match regular(&self.dir.join("index.jsonl")) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            result => result?,
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if len == self.offset {
            return Ok(vec![]);
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = BufReader::new(file.take(len - self.offset));
        let mut offset = self.offset;
        let mut rows = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            buf.read_until(b'\n', &mut line)?;
            if !line.ends_with(b"\n") {
                break;
            }
            offset += line.len() as u64;
            if !line.iter().all(u8::is_ascii_whitespace) {
                rows.push(serde_json::from_slice(&line)?);
            }
        }
        self.offset = offset;
        Ok(rows)
    }
    pub fn retire(&mut self, e: Entry) {
        self.retired.insert(e.response);
        self.retired.extend(e.request);
    }
    pub fn prune(&mut self) -> io::Result<()> {
        let keep = self.latest.as_ref();
        let mtime = keep.and_then(|e| {
            fs::symlink_metadata(self.dir.join(e.body_name()))
                .ok()?
                .modified()
                .ok()
        });
        for f in fs::read_dir(&self.dir)? {
            let f = f?;
            let os_name = f.file_name();
            let Some(name) = os_name.to_str() else {
                continue;
            };
            if keep.is_some_and(|e| e.response == name || e.request.as_deref() == Some(name)) {
                continue;
            }
            let old = native_name(name, ".request.json").is_some()
                && mtime.is_some_and(|time| {
                    fs::symlink_metadata(f.path())
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t < time)
                });
            if old || self.retired.contains(name) {
                fs::remove_file(f.path())?;
            }
        }
        self.retired.clear();
        Ok(())
    }
    pub fn needs_prune(&self) -> bool {
        !self.retired.is_empty()
    }
}
