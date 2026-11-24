pub(crate) mod native;
use super::{ChatInfo, ChatStore, log};
use crate::{
    ApprovalMode, Direction, Error, Protocol, Record, RecordKind, Result,
    observe::sent_prompt_text, runtime,
};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

impl ChatStore {
    /// Adopt a previous `kar run --record` capture. The stream id becomes its stable chat id.
    pub fn import_recording(
        &self,
        runtime_id: &str,
        recording: &Path,
        workspace: &Path,
        program: Option<&Path>,
    ) -> Result<ChatInfo> {
        let mut lines = BufReader::new(File::open(recording)?).lines();
        let first: Record = serde_json::from_str(
            &lines
                .next()
                .transpose()?
                .ok_or_else(|| Error::Protocol("empty recording".into()))?,
        )?;
        let id = first.stream_id;
        let native_id = first
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("recording has no bound root session".into()))?;
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(Error::Protocol("workspace must be a directory".into()));
        }
        let mut rt = runtime(runtime_id)?;
        if let Some(bin) = program {
            rt.program = bin.to_owned();
        }
        if rt.program.is_relative() && rt.program.components().count() > 1 {
            rt.program = rt.program.canonicalize()?;
        }
        let worktree = crate::workbench_git::pin_checkout(&workspace)?;
        let info = ChatInfo {
            process_only: false,
            antigravity_profile: None,
            provider_instance: None,
            imported: None,
            worktree_id: worktree.as_ref().map(|(id, _)| *id),
            forked_from: None,
            id,
            workspace,
            runtime: rt.id,
            protocol: rt.protocol,
            program: rt.program,
            native_id,
            model: None,
            settings: Default::default(),
            unset_env: Default::default(),
            approvals: ApprovalMode::Deny,
            created_ms: first.timestamp_ms,
        };
        let lease = self.lease(id, true)?;
        lease.try_lock().map_err(std::io::Error::from)?;
        if self.path(id).exists() {
            return Err(Error::Refused(format!(
                "recording already has a chat: {id}"
            )));
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        log::header(tmp.as_file_mut(), &info)?;
        let mut writer = log::Log::new(tmp.as_file().try_clone()?, lease, info.clone());
        if info.protocol == Protocol::Claude || info.runtime == "grok" {
            writer.native_store = Some(self.clone());
        }
        writer.attach(id, None)?;
        let records = std::iter::once(Result::Ok(first))
            .chain(lines.map(|line| Ok(serde_json::from_str(&line?)?)));
        for (seq, r) in records.enumerate() {
            let r: Record = r?;
            if r.stream_id != id || r.seq != seq as u64 {
                return Err(Error::InvalidCursor);
            }
            let root = r.agent_path.is_empty()
                && r.session_id.as_ref().is_none_or(|s| s == &info.native_id);
            if root && r.direction == Direction::ToRuntime && r.kind == RecordKind::Request {
                if matches!(
                    r.native["method"].as_str(),
                    Some("thread/start" | "thread/resume" | "session/new" | "session/load")
                ) && let Some(cwd) = r.native.pointer("/params/cwd").and_then(|v| v.as_str())
                    && Path::new(cwd) != info.workspace
                {
                    return Err(Error::Protocol(
                        "recording identifies a different workspace".into(),
                    ));
                }
                let text = sent_prompt_text(&r.native).or_else(|| match info.protocol {
                    Protocol::Morph => r.native["task"].as_str().map(str::to_owned),
                    Protocol::Cybion => r.native["input"].as_str().map(str::to_owned),
                    _ => None,
                });
                if let Some(text) = text
                    && !r.events.iter().any(|event| {
                        matches!(
                            event,
                            crate::Event::SteeringInput { .. }
                                | crate::Event::ManualCompactionRequested
                        )
                    })
                {
                    let input = (info.protocol == Protocol::Claude && r.native["type"] == "user")
                        .then(|| r.native["uuid"].as_str()?.parse().ok())
                        .flatten();
                    writer.import_prompt(input, text, r.timestamp_ms)?;
                }
            }
            writer.import_record(&r)?;
        }
        if writer.state.turns.is_empty() {
            return Err(Error::Protocol(
                "no dispatched text prompt found in this recording".into(),
            ));
        }
        if let Some(id) = writer.state.active() {
            writer.end(id, super::TurnStatus::Interrupted)?;
        }
        tmp.persist_noclobber(self.path(id)).map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(info)
    }
}
