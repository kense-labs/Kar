//! Resolve the native owner of a recorded turn without switching the parent chat.
use super::*;
use crate::chat::{HandoffStatus, ProviderSession};
use std::collections::HashSet;

pub(in crate::chat) struct Launch {
    pub owner: ChatInfo,
    pub options: SessionOptions,
    pub staged: Option<Uuid>,
}

pub(super) fn options(
    parent: &ChatInfo,
    owner: &ChatInfo,
    mut live: SessionOptions,
) -> SessionOptions {
    // Credentials, MCP endpoints, custom arguments and timeouts may be specific to
    // the active process. Only a caller configuring this fork can supply new ones.
    if owner.runtime != parent.runtime
        || owner.program != parent.program
        || owner.protocol != parent.protocol
        || owner.native_id != parent.native_id
    {
        let mut saved = owner.options();
        saved.unset_env.extend(live.unset_env);
        return saved;
    }
    live.model.clone_from(&owner.model);
    live.settings.clone_from(&owner.settings);
    live.approvals = owner.approvals;
    live.unset_env.extend(owner.unset_env.iter().cloned());
    live
}

fn position(state: &log::State, turn: Uuid) -> Result<usize> {
    state
        .turns
        .iter()
        .position(|t| t.id == turn)
        .ok_or_else(|| {
            Error::Refused(format!(
                "missing historical turn {turn} in chat {}",
                state.info.id
            ))
        })
}

pub(super) fn witness(turn: &super::super::ChatTurn) -> Result<&str> {
    turn.items
        .iter()
        .find(|item| item.source.path.is_empty())
        .and_then(|i| i.source.session_id.as_deref())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Refused("selected turn lacks a native root session identity".into()))
}

impl Launch {
    pub fn prepare(
        store: &ChatStore,
        state: &log::State,
        turn: Uuid,
        live: SessionOptions,
    ) -> Result<Self> {
        if let Some(mut child) = store.staged_fork(state.info.id)? {
            let origin = child.forked_from.as_ref().expect("validated staged fork");
            if origin.turn != turn {
                return Err(Error::Refused(
                    "retry the pending fork at its original turn".into(),
                ));
            }
            child.native_id.clone_from(&origin.native_source);
            let staged = Some(child.id);
            let options = options(&state.info, &child, live);
            return Ok(Self {
                owner: child,
                options,
                staged,
            });
        }
        let mut parent_state = None;
        let mut visited = HashSet::new();
        loop {
            let current = parent_state.as_ref().unwrap_or(state);
            if !visited.insert(current.info.id) {
                return Err(Error::Refused(
                    "historical fork ancestry contains a cycle".into(),
                ));
            }
            let index = position(current, turn)?;
            let selected = &current.turns[index];
            if let Some(origin) = &current.info.forked_from
                && (index < current.inherited_len
                    || current
                        .turns
                        .iter()
                        .position(|t| t.id == origin.turn)
                        .is_some_and(|end| index <= end))
            {
                // An inherited timeline retains its original native IDs. Reconstruct
                // launch settings from the actual owner, not from the child header.
                let mut file = File::open(store.path(origin.chat)).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Error::Refused(format!(
                            "original chat {} is missing for this inherited turn",
                            origin.chat
                        ))
                    } else {
                        error.into()
                    }
                })?;
                let ancestor = log::with_lock(&mut file, false, |file| {
                    log::read_before_archive(file, origin.chat, Some(origin.turn))
                        .map(|(state, _)| state)
                })?;
                let cut = position(&ancestor, origin.turn)?;
                let original = ancestor
                    .turns
                    .get(index)
                    .filter(|t| t.id == turn && index <= cut)
                    .ok_or_else(|| {
                        Error::Refused("inherited prefix does not match its original chat".into())
                    })?;
                if current.info.checkpoint_store() != ancestor.info.checkpoint_store()
                    || witness(&ancestor.turns[cut])? != origin.native_source
                    || witness(selected)? != witness(original)?
                    || selected
                        .checkpoint
                        .as_ref()
                        .and_then(|c| c.after_tree.as_ref())
                        != original
                            .checkpoint
                            .as_ref()
                            .and_then(|c| c.after_tree.as_ref())
                {
                    return Err(Error::Refused(
                        "inherited turn has conflicting native or workspace evidence".into(),
                    ));
                }
                parent_state = Some(ancestor);
                continue;
            }
            let mut owner = current.info.clone();
            // A ready handoff records the source binding and count of all turns
            // preceding the switch. Multiple switches may occur without a new turn.
            let mut previous_end = 0;
            let mut selected_route = None;
            for handoff in current
                .handoffs
                .iter()
                .filter(|h| h.status == HandoffStatus::Ready)
            {
                if handoff.through_turn < previous_end
                    || handoff.through_turn > current.turns.len()
                    || handoff.target.is_none()
                {
                    return Err(Error::Refused(
                        "incomplete provider handoff history for the selected turn".into(),
                    ));
                }
                previous_end = handoff.through_turn;
                if selected_route.is_none() && handoff.through_turn > index {
                    selected_route = Some(&handoff.from);
                }
            }
            if let Some(workspace) = current.handoffs.iter().filter(|h| h.through_turn > index).find_map(|h| h.source_workspace.as_ref()) {
                owner.workspace = workspace.clone();
                owner.worktree_id = None;
            }
            if let Some(route) = selected_route {
                route.activate(&mut owner);
            }
            if witness(selected)? != owner.native_id {
                return Err(Error::Refused(
                    "historical provider binding does not match the selected turn's native session"
                        .into(),
                ));
            }
            let options = options(&state.info, &owner, live);
            return Ok(Self {
                owner,
                options,
                staged: None,
            });
        }
    }

    pub fn activate(&self, parent: &mut ChatInfo) {
        ProviderSession::current(&self.owner).activate(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ancestor_or_mismatched_native_identity_never_selects_the_active_provider() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("chats")).unwrap();
        let info = ChatInfo {
            process_only: false,
            antigravity_profile: None,
            provider_instance: None,
            imported: None,
            id: Uuid::new_v4(),
            workspace: dir.path().into(),
            worktree_id: None,
            runtime: "qwen".into(),
            protocol: Protocol::Qwen,
            native_id: "current".into(),
            created_ms: 1,
            program: "qwen".into(),
            model: None,
            settings: Default::default(),
            unset_env: Default::default(),
            approvals: crate::ApprovalMode::Deny,
            forked_from: None,
        };
        let mut file = tempfile::tempfile().unwrap();
        log::header(&mut file, &info).unwrap();
        let (mut state, _) = log::read(&mut file, info.id).unwrap();
        let turn = Uuid::new_v4();
        state
            .apply(super::super::super::ChatUpdate::Begin {
                background_note: None,
                attachments: Vec::new(),
                id: turn,
                at: 2,
                text: "one".into(),
                before_tree: None,
                repositories: Vec::new(),
            })
            .unwrap();
        state.turns[0].items[0].source.session_id = Some("old".into());
        let check = |s: &log::State| {
            Launch::prepare(&store, s, turn, info.options())
                .err()
                .unwrap()
                .to_string()
        };
        assert!(check(&state).contains("does not match"));
        state.info.forked_from = Some(ForkOrigin {
            mode: ForkMode::Native,
            chat: Uuid::new_v4(),
            turn,
            native_source: "old".into(),
            native_workspace: None,
            anchor: "select".into(),
            checkpoint_store: info.id,
            worktree_base: None,
        });
        assert!(check(&state).contains("is missing"));
    }
}
