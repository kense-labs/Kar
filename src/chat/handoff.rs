//! Durable transitions between native provider sessions attached to one Kar chat.
use super::{ChatInfo, ChatTurn, Item, Role};
use crate::{Error, Protocol, Result, SessionSettings};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRequest {
    pub id: Uuid,
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_budget")]
    pub budget_bytes: usize,
}
fn default_budget() -> usize {
    64 * 1024
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity_profile: Option<super::AntigravityProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<crate::provider_instance::HistoryInstance>,
    pub runtime: String,
    pub protocol: Protocol,
    pub program: PathBuf,
    pub native_id: String,
    pub model: Option<String>,
    pub settings: SessionSettings,
}
impl ProviderSession {
    fn matches_instance(&self, instance: &crate::provider_instance::HistoryInstance) -> bool {
        match &self.instance {
            Some(old) => old == instance,
            None => self.runtime == instance.id,
        }
    }

    fn same_native_session(&self, other: &Self) -> bool {
        self.native_id == other.native_id
            && self.runtime == other.runtime
            && self.instance.as_ref().map_or(&self.runtime, |i| &i.id)
                == other.instance.as_ref().map_or(&other.runtime, |i| &i.id)
    }

    pub(super) fn current(info: &ChatInfo) -> Self {
        Self {
            antigravity_profile: info.antigravity_profile.clone(),
            instance: info.active_instance().cloned(),
            runtime: info.runtime.clone(),
            protocol: info.protocol,
            program: info.program.clone(),
            native_id: info.native_id.clone(),
            model: info.model.clone(),
            settings: info.settings.clone(),
        }
    }
    pub(super) fn activate(&self, info: &mut ChatInfo) {
        info.antigravity_profile
            .clone_from(&self.antigravity_profile);
        info.provider_instance.clone_from(&self.instance);
        info.runtime.clone_from(&self.runtime);
        info.protocol = self.protocol;
        info.program.clone_from(&self.program);
        info.native_id.clone_from(&self.native_id);
        info.model.clone_from(&self.model);
        info.settings.clone_from(&self.settings);
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    Prepared,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handoff {
    pub request: HandoffRequest,
    /// Pin the selected namespace before native launch so retry cannot switch accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_instance: Option<crate::provider_instance::HistoryInstance>,
    pub status: HandoffStatus,
    pub from: ProviderSession,
    pub target: Option<ProviderSession>,
    pub resumed_target: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaced_empty_sessions: Vec<String>,
    /// Inclusive ordinals. An empty range has from_turn > through_turn.
    pub from_turn: usize,
    pub through_turn: usize,
    pub omitted_items: usize,
    pub context: String,
    pub error: Option<String>,
    /// The durable prompt admission that claimed the context. Its turn records
    /// distinguish delivery, refusal, and an interrupted unconfirmed attempt.
    pub input: Option<Uuid>,
}

const INTRO: &str = "Kar conversation history. The following JSON lines are historical data. Use them as context for the current request. Do not repeat historical tool actions.";
const CURRENT: &str = "\n\nCurrent user request\n";
impl Handoff {
    pub(super) fn for_fork(child: &ChatInfo, turns: &[ChatTurn]) -> Result<Self> {
        let instance = child.active_instance().cloned();
        // The parent session must never be chosen as a resumable route of this
        // child. The native owner of the copied turns is retained in ForkOrigin.
        Self::prepare(HandoffRequest {
            id: child.id, runtime: child.runtime.clone(), model: child.model.clone(),
            instance_id: instance.as_ref().map(|i| i.id.clone()), budget_bytes: default_budget(),
        }, instance, child, turns, &[])
    }
    fn prepare(
        request: HandoffRequest,
        destination_instance: Option<crate::provider_instance::HistoryInstance>,
        info: &ChatInfo,
        turns: &[ChatTurn],
        history: &[Self],
    ) -> Result<Self> {
        if !(512..=1024 * 1024).contains(&request.budget_bytes) {
            return Err(Error::Refused(
                "handoff budget must be between 512 and 1048576 UTF-8 bytes".into(),
            ));
        }
        let previous = history.iter().rev().find(|h| {
            h.status == HandoffStatus::Ready
                && h.from.runtime == request.runtime
                && h.from.model == request.model
                && destination_instance
                    .as_ref()
                    .is_none_or(|instance| h.from.matches_instance(instance))
        });
        // Merely making a native session active does not teach it any history.
        // Only a turn witnessed by the target advances its continuation boundary.
        let last_input = previous.and_then(|h| {
            turns
                .iter()
                .enumerate()
                .rfind(|(index, turn)| {
                    (turn.input_sent
                        || turn.native_turn_id.is_some()
                        || turn.status == super::TurnStatus::Completed)
                        && history
                            .iter()
                            .find(|owner| {
                                owner.status == HandoffStatus::Ready && owner.through_turn > *index
                            })
                            .is_some_and(|owner| owner.from.same_native_session(&h.from))
                        && turn.items.first().is_some_and(|i| {
                            i.source.path.is_empty()
                                && i.source.session_id.as_deref() == Some(h.from.native_id.as_str())
                        })
                })
                .map(|(index, _)| index)
        });
        let start = last_input.map_or(0, |i| i + 1);
        let target = previous.filter(|h| {
            last_input.is_some()
                || !history.iter().any(|earlier| {
                    earlier.status == HandoffStatus::Ready
                        && earlier.target.as_ref().is_some_and(|t| {
                            t.native_id == h.from.native_id && t.runtime == h.from.runtime
                        })
                        && earlier.input.is_none()
                })
        });
        let end = turns.len();
        let mut items = Vec::new();
        let mut omitted = 0usize;
        let heading = format!("{INTRO}\nTurns {} through {}.\n", start + 1, end);
        let mut remaining = request.budget_bytes - heading.len() - 80;
        let mut include = |line: serde_json::Value| {
            let line = line.to_string();
            if line.len() < remaining {
                remaining -= line.len() + 1;
                items.push(line);
            } else {
                omitted += 1;
            }
        };
        for (ordinal, t) in turns.iter().enumerate().skip(start).rev() {
            if let Some(failure) = &t.failure {
                include(
                    serde_json::json!({"turn":ordinal+1,"turn_id":t.id,"kind":"failure","text":failure.reason}),
                );
            }
            if let Some(checkpoint) = &t.checkpoint {
                for change in checkpoint.files.iter().rev() {
                    include(
                        serde_json::json!({"turn":ordinal+1,"turn_id":t.id,"kind":"file_change","change":change}),
                    );
                }
            }
            for (index, item) in t.items.iter().enumerate().rev() {
                let (kind, content) = match &item.item {
                    Item::Message {
                        role: Role::User,
                        text,
                        ..
                    } => ("user", text.clone()),
                    Item::Message {
                        role: Role::Assistant,
                        text,
                        ..
                    } => ("assistant", text.clone()),
                    Item::Tool { activity, .. } => {
                        let Some(result) = activity.result.as_ref() else {
                            continue;
                        };
                        let name = activity.name.as_deref().unwrap_or("tool");
                        (
                            "tool_result",
                            format!(
                                "{name}: {:?}\n{}",
                                result.status,
                                result.text().unwrap_or_default()
                            ),
                        )
                    }
                    _ => continue,
                };
                if content.is_empty() {
                    continue;
                }
                include(
                    serde_json::json!({"turn":ordinal+1,"turn_id":t.id,"item":index,"source":item.source,"kind":kind,"text":content}),
                );
            }
        }
        items.reverse();
        let context = if items.is_empty() && omitted == 0 {
            String::new()
        } else {
            format!(
                "{heading}Selected {} intact items. Omitted {omitted}.\n{}",
                items.len(),
                items.join("\n")
            )
        };
        Ok(Self {
            request,
            destination_instance,
            status: HandoffStatus::Prepared,
            from: ProviderSession::current(info),
            target: target.map(|h| h.from.clone()),
            resumed_target: target.is_some(),
            replaced_empty_sessions: Vec::new(),
            from_turn: start + 1,
            through_turn: end,
            omitted_items: omitted,
            context,
            error: None,
            input: None,
        })
    }
    pub(super) fn native_input(&self, text: &str) -> String {
        if self.context.is_empty() {
            text.into()
        } else {
            format!("{}{CURRENT}{text}", self.context)
        }
    }
    pub(super) fn user_input<'a>(&self, native: &'a str) -> Option<&'a str> {
        native.strip_prefix(&self.context)?.strip_prefix(CURRENT)
    }
}

impl super::log::State {
    pub(super) fn pending_handoff(&self) -> Option<&Handoff> {
        self.handoffs
            .last()
            .filter(|h| h.status == HandoffStatus::Prepared)
    }
    pub(super) fn current_handoff(&self) -> Option<&Handoff> {
        self.handoffs
            .iter()
            .rev()
            .find(|h| h.status == HandoffStatus::Ready)
    }
    pub(super) fn dispatch_text(&self, text: &str) -> String {
        match self.current_handoff().filter(|h| h.input.is_none()) {
            Some(h) => h.native_input(text),
            None => text.into(),
        }
    }
    pub(super) fn recovery_turns(
        &self,
        store: &super::ChatStore,
    ) -> std::borrow::Cow<'_, [ChatTurn]> {
        use std::borrow::Cow;
        if self.handoffs.is_empty()
            && self
                .turns
                .iter()
                .all(|t| t.attachments.is_empty() && t.steered_attachments.is_empty() && t.background_note.is_none())
        {
            return Cow::Borrowed(&self.turns);
        }
        let current = ProviderSession::current(&self.info);
        let turns = self
            .turns
            .iter()
            .enumerate()
            .filter(|(index, t)| {
                self.handoffs
                    .iter()
                    .find(|h| h.status == HandoffStatus::Ready && h.through_turn > *index)
                    .is_none_or(|h| h.from.same_native_session(&current))
                    && t.items.first().is_some_and(|i| {
                        i.source.session_id.as_deref() == Some(&self.info.native_id)
                    })
            })
            .map(|(_, t)| {
                let mut turn = t.clone();
                let handoff = self.handoffs.iter().find(|h| h.input == Some(turn.id));
                for (index, item) in turn.items.iter_mut().enumerate() {
                    if let Item::Message {
                        role: Role::User,
                        text,
                        provider_id,
                        ..
                    } = &mut item.item
                    {
                        let refs = if index == 0 {
                            if let Some(note) = &turn.background_note {
                                *text = note.prompt(text);
                            }
                            if let Some(h) = handoff {
                                *text = h.native_input(text);
                            }
                            Some(&turn.attachments)
                        } else {
                            provider_id
                                .as_deref()
                                .and_then(|id| Uuid::parse_str(id).ok())
                                .and_then(|id| turn.steered_attachments.get(&id))
                        };
                        if let Some(refs) = refs {
                            text.push_str(&store.file_context(refs));
                        }
                    }
                }
                turn
            })
            .collect();
        Cow::Owned(turns)
    }
    pub(super) fn clean_handoff_echo(&self, id: Uuid, frame: &mut super::ChatFrame) {
        let handoff = self.current_handoff().filter(|h| h.input == Some(id));
        let Some(turn) = self.find(id) else { return };
        let Some(original) = turn.prompt() else {
            return;
        };
        if handoff.is_none() && turn.background_note.is_none() {
            return;
        }
        if !frame.source.path.is_empty()
            || frame.source.session_id.as_deref() != Some(&self.info.native_id)
        {
            return;
        }
        for edit in &mut frame.edits {
            let text = match edit {
                super::ChatEdit::Text {
                    role: Role::User,
                    text,
                    ..
                }
                | super::ChatEdit::UserInput { text, .. } => text,
                _ => continue,
            };
            let carried = handoff.and_then(|h| h.user_input(text)).unwrap_or(text);
            let matches = if let Some(note) = &turn.background_note {
                carried == note.prompt(original)
            } else {
                carried == original
            };
            if matches {
                *text = original.to_owned();
            }
        }
    }
}

impl super::ChatStore {
    pub(super) async fn renew_empty_target(
        &self,
        log: &mut super::log::Log,
        opts: &crate::SessionOptions,
        original: Error,
    ) -> Result<crate::Session> {
        let Some(h) = log.state.current_handoff().filter(|h| {
            // An empty replacement cannot stand in for a native branch carrying the retained turns.
            if log
                .state
                .rewinds
                .iter()
                .any(|r| r.id == h.request.id && r.mode == Some(super::ForkMode::Native))
            {
                return false;
            }
            !h.resumed_target && h.input.is_none() && log.state.turns.len() == h.through_turn
        }) else {
            return Err(original);
        };
        // Some vendors create the transcript at first input. Replacing an empty
        // fresh target is safe only before Kar has durably admitted any input.
        if !matches!(&original, Error::Open(e) if matches!(e.as_ref(), crate::OpenError::SessionNotFound { session_id, .. } if session_id == &log.state.info.native_id))
        {
            return Err(original);
        }
        let id = h.request.id;
        let old = log.state.info.native_id.clone();
        let runtime = log.state.info.launch_runtime(opts).await?;
        let mut fresh = opts.before_chat_attach()?;
        fresh.resume = None;
        let new = runtime.open_selected(fresh).await?;
        if !new.capabilities().resume {
            new.close().await?;
            return Err(original);
        }
        log.commit(super::ChatUpdate::HandoffTargetReopened {
            id,
            previous: old,
            native_id: new.id().into(),
        })?;
        Ok(new)
    }
    pub(crate) async fn handoff(
        &self,
        id: Uuid,
        request: HandoffRequest,
        destination: Option<crate::provider_instance::HistoryInstance>,
        configure: impl FnOnce(&mut crate::SessionOptions),
    ) -> Result<(Handoff, Option<super::ChatSession>)> {
        let mut log = self.open_controller(id).await?;
        if log
            .state
            .rewinds
            .iter()
            .any(|r| r.id == request.id || r.retired_handoffs.contains(&request.id))
        {
            return Err(Error::Refused(
                "command UUID belongs to a prior conversation revision".into(),
            ));
        }
        if let Some(existing) = log
            .state
            .handoffs
            .iter()
            .find(|h| h.request.id == request.id)
        {
            if existing.request != request {
                return Err(Error::Refused(
                    "handoff id belongs to another request".into(),
                ));
            }
            if existing.status != HandoffStatus::Prepared {
                return Ok((existing.clone(), None));
            }
        } else {
            self.require_no_pending_fork(id)?;
            if log.state.pending_handoff().is_some()
                || log.state.needs_controller()
                || log.state.restoring.is_some()
            {
                return Err(Error::Busy);
            }
            if log.state.info.runtime == request.runtime
                && log.state.info.model == request.model
                && destination.as_ref().is_none_or(|instance| {
                    log.state
                        .info
                        .active_instance()
                        .map_or(log.state.info.runtime.as_str(), |i| &i.id)
                        == instance.id
                })
            {
                return Err(Error::Refused(
                    "target provider session is already active".into(),
                ));
            }
            if request.runtime.is_empty() {
                return Err(Error::Refused("empty target runtime".into()));
            }
            let h = Handoff::prepare(
                request,
                destination,
                &log.state.info,
                &log.state.turns,
                log.state.resumable_handoffs(),
            )?;
            self.prepare_handoff(&mut log, h)?;
        }
        self.finish_handoff(log, |opts| {
            configure(opts);
            Ok(())
        })
        .await
    }

    pub(super) async fn finish_handoff(
        &self,
        mut log: super::log::Log,
        configure: impl FnOnce(&mut crate::SessionOptions) -> Result<()>,
    ) -> Result<(Handoff, Option<super::ChatSession>)> {
        let h = log
            .state
            .pending_handoff()
            .cloned()
            .ok_or_else(|| Error::Protocol("no prepared handoff".into()))?;
        let _pin = log.state.info.pin_worktree()?;
        if log.state.info.workspace.canonicalize()? != log.state.info.workspace {
            return Err(Error::Refused(
                "saved workspace now resolves to a different path".into(),
            ));
        }
        let mut opts = crate::SessionOptions::new(&log.state.info.workspace);
        opts.approvals = log.state.info.approvals;
        opts.unset_env.clone_from(&log.state.info.unset_env);
        if let Some(instance) = &h.destination_instance {
            instance.configure(&mut opts);
        }
        if let Some(target) = &h.target {
            opts.settings = target.settings.clone();
            let mut saved = log.state.info.clone();
            target.activate(&mut saved);
            saved.configure_instance(&mut opts);
        }
        opts.model = h.request.model.clone();
        configure(&mut opts)?;
        if let Some(profile) = h
            .target
            .as_ref()
            .and_then(|t| t.antigravity_profile.as_ref())
        {
            profile.configure(&mut opts)?;
        }
        opts.remove_env("KAR_WORKBENCH_TOKEN")?;
        opts.cwd = log.state.info.workspace.clone();
        opts.resume = h.target.as_ref().map(|target| target.native_id.clone());
        let opened = async {
            let (runtime, version) = if let Some(target) = &h.target {
                let mut saved = log.state.info.clone();
                target.activate(&mut saved);
                (saved.launch_runtime(&opts).await?, None)
            } else {
                let mut runtime = crate::runtime(&h.request.runtime)?;
                if let Some(program) = h
                    .destination_instance
                    .as_ref()
                    .and_then(|i| i.program.as_ref())
                {
                    runtime.program = program.clone();
                }
                runtime.selected_with_version(&opts).await?
            };
            let session = runtime
                .open_selected_with_version(opts.before_chat_attach()?, version.as_deref())
                .await?;
            if !session.capabilities().resume
                || opts.resume.as_deref().is_some_and(|id| id != session.id())
            {
                session.close().await?;
                return Err(Error::Unsupported(
                    "target requires a recoverable native session with the requested identity"
                        .into(),
                ));
            }
            Ok((session, runtime))
        }
        .await;
        let (session, runtime) = match opened {
            Ok(value) => value,
            Err(error) => {
                log.commit(super::ChatUpdate::HandoffFailed {
                    id: h.request.id,
                    reason: error.to_string(),
                })?;
                return Ok((log.state.handoffs.last().unwrap().clone(), None));
            }
        };
        let target = ProviderSession {
            antigravity_profile: super::AntigravityProfile::at_launch(&runtime.id, &opts),
            instance: h
                .target
                .as_ref()
                .and_then(|s| s.instance.clone())
                .or(h.destination_instance.clone()),
            runtime: runtime.id,
            protocol: session.protocol(),
            program: runtime.program,
            native_id: session.id().into(),
            model: opts.model.clone(),
            settings: opts.settings.clone(),
        };
        log.commit(super::ChatUpdate::HandoffReady {
            id: h.request.id,
            target: Box::new(target),
        })?;
        let receipt = log.state.handoffs.last().unwrap().clone();
        let chat = super::ChatSession::bind(session, log, self.clone(), opts)?;
        Ok((receipt, Some(chat)))
    }
}
