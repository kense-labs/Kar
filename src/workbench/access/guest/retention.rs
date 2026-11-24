//! Share drafts have a bounded lifetime. An accepted file belongs to the chat turn that used it.
use super::*;
use std::io::Write;

const RESOLVED_LIMIT: usize = 20;

pub(super) fn signature(text: &str, files: &[Attachment]) -> String {
    let mut hash = Sha256::new();
    let mut field = |data: &[u8]| {
        hash.update((data.len() as u64).to_be_bytes());
        hash.update(data);
    };
    field(text.as_bytes());
    for f in files {
        field(f.name.as_bytes());
        field(f.base64.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn check(expected: &str, got: &str) -> AccessResult<()> {
    if expected == got {
        Ok(())
    } else {
        Err(AccessError::Conflict(
            "request ID belongs to another proposal".into(),
        ))
    }
}

impl Proposal {
    pub(super) fn check_signature(&self, got: &str) -> AccessResult<()> {
        check(&self.signature, got)
    }
    fn resolved(&self) -> bool {
        matches!(self.status, Decision::Accepted | Decision::Dismissed)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Retired {
    pub(super) author: Uuid,
    pub(super) key: Uuid,
    pub(super) status: Decision,
    signature: String,
    files: Vec<Uuid>,
    delivery: Option<InputStatus>,
}

impl Retired {
    pub(super) fn replay(
        &self,
        id: Uuid,
        share: &Share,
        got: &str,
        text: &str,
        mut files: Vec<Attachment>,
    ) -> AccessResult<Value> {
        check(&self.signature, got)?;
        for (file, stored_id) in files.iter_mut().zip(&self.files) {
            file.id = *stored_id;
        }
        let mut view = self.receipt(id, share);
        view["text"] = json!(text);
        view["attachments"] = json!(
            files
                .iter()
                .map(|f| json!({"id": f.id, "name":f.name,"bytes":f.size}))
                .collect::<Vec<_>>()
        );
        Ok(view)
    }

    pub(super) fn receipt(&self, id: Uuid, share: &Share) -> Value {
        let mut value = json!({"id":id,"request_id":self.key,"author":share.guests.get(&self.author).map(|g| &g.name),"status":self.status});
        if let Some(status) = self.delivery {
            value["delivery"] = json!(status);
        }
        value
    }
}

pub(in crate::workbench::access) fn needs_maintenance(s: &Share) -> bool {
    let closed = s.revoked || now() >= s.expires_ms;
    s.proposals.values().any(|p| {
        p.signature.is_empty()
            || closed && p.status == Decision::Pending
            || p.resolved()
                && p.files.iter().any(|f| {
                    !f.base64.is_empty()
                        || p.status == Decision::Accepted && !s.published.contains_key(&f.id)
                })
    }) || s.proposals.values().filter(|p| p.resolved()).count() > RESOLVED_LIMIT
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Published {
    turn: Uuid,
    name: String,
    size: usize,
    position: usize,
}

pub(super) fn chat_files(db: &Registry, share: &Share) -> HashMap<Uuid, Vec<Value>> {
    let mut by_turn: HashMap<Uuid, Vec<_>> = HashMap::new();
    for s in db
        .shares
        .values()
        .filter(|s| s.chat == share.chat && s.project == share.project)
    {
        for (id, f) in &s.published {
            by_turn
                .entry(f.turn)
                .or_default()
                .push((f.position, json!({"id":id,"name":f.name,"bytes":f.size})));
        }
    }
    by_turn
        .into_iter()
        .map(|(turn, mut files)| {
            files.sort_unstable_by_key(|f| f.0);
            (turn, files.into_iter().map(|(_, v)| v).collect())
        })
        .collect()
}

fn blob_path(state: &FsPath, id: Uuid) -> PathBuf {
    state.join(format!("guest-{id}.attachment"))
}

fn publish(state: &FsPath, f: &Attachment) -> crate::Result<()> {
    let bytes = STANDARD
        .decode(&f.base64)
        .map_err(|_| Error::Protocol("invalid saved guest attachment".into()))?;
    if bytes.len() != f.size {
        return Err(Error::Protocol(
            "saved guest attachment length differs".into(),
        ));
    }
    let path = blob_path(state, f.id);
    let mut tmp = tempfile::NamedTempFile::new_in(state)?;
    tmp.write_all(&bytes)?;
    tmp.as_file().sync_all()?;
    match tmp.persist_noclobber(&path) {
        Ok(_) => {
            #[cfg(unix)]
            File::open(state)?.sync_all()?;
            Ok(())
        }
        // A crash can leave the immutable bytes in place before the registry rename.
        Err(e)
            if e.error.kind() == io::ErrorKind::AlreadyExists && std::fs::read(&path)? == bytes =>
        {
            Ok(())
        }
        Err(e) => Err(e.error.into()),
    }
}

pub(in crate::workbench::access) fn maintain(
    db: &mut Registry,
    state: &FsPath,
) -> AccessResult<()> {
    let mut published_chats = std::collections::BTreeSet::new();
    for s in db.shares.values_mut().filter(|s| needs_maintenance(s)) {
        let closed = s.revoked || now() >= s.expires_ms;
        let mut resolved = Vec::new();
        for p in s.proposals.values_mut() {
            if p.signature.is_empty() {
                p.signature = signature(&p.text, &p.files);
            }
            if p.status == Decision::Pending && closed {
                p.status = Decision::Dismissed;
                s.revision += 1;
            }
            if !p.resolved() {
                continue;
            }
            resolved.push((p.sequence, p.created_ms, p.id));
            for (position, f) in p.files.iter_mut().enumerate() {
                if p.status == Decision::Accepted && !s.published.contains_key(&f.id) {
                    publish(state, f)?;
                    s.published.insert(
                        f.id,
                        Published {
                            turn: p.id,
                            name: f.name.clone(),
                            size: f.size,
                            position,
                        },
                    );
                    published_chats.insert(s.chat);
                }
                f.base64.clear();
            }
        }
        resolved.sort_unstable();
        let excess = resolved.len().saturating_sub(RESOLVED_LIMIT);
        for (_, _, id) in resolved.into_iter().take(excess) {
            let p = s.proposals.remove(&id).expect("collected from proposals");
            // Keep only a retry receipt after removing the text and file contents.
            s.retired.insert(
                id,
                Retired {
                    author: p.author,
                    key: p.key,
                    status: p.status,
                    signature: p.signature,
                    files: p.files.into_iter().map(|f| f.id).collect(),
                    delivery: p.delivery,
                },
            );
        }
    }
    // A delayed receipt from an older share changes the current share's timeline as well.
    for s in db
        .shares
        .values_mut()
        .filter(|s| !s.revoked && published_chats.contains(&s.chat))
    {
        s.revision += 1;
    }
    Ok(())
}

pub(super) fn read_attachment(app: &Workbench, who: Principal, id: Uuid) -> AccessResult<Vec<u8>> {
    let (chat, turn) = {
        let db = app.0.access.registry.lock().unwrap();
        let s = db
            .shares
            .get(&who.share)
            .filter(|s| s.active(app))
            .ok_or(AccessError::Invalid)?;
        if let Some(f) = s
            .proposals
            .values()
            .filter(|p| {
                p.author == who.guest && matches!(p.status, Decision::Pending | Decision::Sending)
            })
            .flat_map(|p| &p.files)
            .find(|f| f.id == id)
        {
            return STANDARD
                .decode(&f.base64)
                .map_err(|_| Error::Protocol("invalid saved guest attachment".into()).into());
        }
        let turn = db
            .shares
            .values()
            .filter(|v| v.chat == s.chat && v.project == s.project)
            .find_map(|v| v.published.get(&id))
            .ok_or(AccessError::Missing)?;
        (s.chat, turn.turn)
    };
    // A rolled-back or removed turn no longer grants access to its file.
    if !app
        .0
        .chats
        .history(chat)?
        .turns
        .iter()
        .any(|t| t.id == turn)
    {
        return Err(AccessError::Missing);
    }
    let state = app
        .0
        .access
        .file
        .parent()
        .expect("workbench state directory");
    Ok(std::fs::read(blob_path(state, id)).map_err(Error::Io)?)
}
