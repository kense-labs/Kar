//! A pending workspace move reserves its continuation's FIFO slot at MCP admission.
//! Only the original controller may settle outstanding native attempts. Fresh dispatch
//! resumes when the journal publishes the new workspace and native route together.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct WorkspaceGate {
    command: Uuid,
    continuation: Option<Uuid>,
}
impl ChatStore {
    pub(crate) fn reserve_workspace_inputs(
        &self,
        chat: Uuid,
        command: Uuid,
        continuation: Option<(Uuid, &str)>,
    ) -> Result<()> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        if let Some(gate) = &inbox.workspace_gate {
            return if gate.command == command && gate.continuation == continuation.map(|p| p.0) {
                Ok(())
            } else {
                Err(Error::Busy)
            };
        }
        if let Some((id, text)) = continuation {
            crate::input::check_text(text, crate::input::text_limit(&state.info.runtime))?;
            state.check_new_prompt_id(id)?;
            if state.find(id).is_some() || inbox.entries.iter().any(|e| e.id == id) {
                return Err(different_prompt());
            }
            inbox.entries.push(Entry {
                uncertainty_acknowledged: false,
                refusal: None,
                attachments: Vec::new(),
                initial_attachments: None,
                promotion: None,
                initial_text: None,
                id,
                text: text.to_owned(),
                withdrawn: false,
            });
        }
        inbox.workspace_gate = Some(WorkspaceGate {
            command,
            continuation: continuation.map(|p| p.0),
        });
        self.save_inbox(&inbox)
    }

    pub(crate) fn release_workspace_inputs(&self, chat: Uuid, command: Uuid) -> Result<()> {
        let _lock = self.inbox_lock(chat)?;
        let state = self.read_state(File::open(self.path(chat))?, chat)?;
        let mut inbox = self.read_inbox(&state.info)?;
        if let Some(gate) = inbox
            .workspace_gate
            .as_ref()
            .filter(|g| g.command == command)
        {
            if !state.handoffs.iter().any(|h| {
                h.request.id == command
                    && h.source_workspace.is_some()
                    && h.status == super::super::HandoffStatus::Ready
            }) && let Some(input) = gate.continuation
                && let Some(entry) = inbox.entries.iter_mut().find(|e| e.id == input)
            {
                entry.withdrawn = true;
            }
            inbox.workspace_gate = None;
            self.save_inbox(&inbox)?;
        }
        Ok(())
    }

    /// Recovery also releases reservations made just before a host failed to commit its catalog.
    pub(crate) fn recover_workspace_inputs(
        &self,
        pending: &std::collections::BTreeSet<Uuid>,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Header {
            workspace_gate: Option<WorkspaceGate>,
        }
        for file in std::fs::read_dir(&self.dir)? {
            let path = file?.path();
            if path.extension().is_none_or(|e| e != "inputs") {
                continue;
            }
            let Some(chat) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            // A corrupt, unrelated queue is reported by its own queue APIs.
            let Ok(header) =
                serde_json::from_reader::<_, Header>(std::io::BufReader::new(File::open(path)?))
            else {
                continue;
            };
            if let Some(gate) = header.workspace_gate
                && !pending.contains(&gate.command)
            {
                self.release_workspace_inputs(chat, gate.command)?;
            }
        }
        Ok(())
    }
}
