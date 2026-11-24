//! Ephemeral next prompts belong to a particular live controller and completed user turn.
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptSuggestion {
    pub turn_id: Uuid,
    pub text: String,
}

#[derive(Debug)]
struct Cache {
    enabled: bool,
    generation: u64,
    owners: HashMap<Uuid, Uuid>,
    values: VecDeque<(Uuid, PromptSuggestion)>,
}

#[derive(Clone, Debug)]
pub struct PromptSuggestions {
    state: Arc<Mutex<Cache>>,
    updates: watch::Sender<()>,
    settings: watch::Sender<()>,
}

impl PromptSuggestions {
    pub fn new(enabled: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(Cache {
                enabled,
                generation: 0,
                owners: HashMap::new(),
                values: VecDeque::new(),
            })),
            updates: watch::channel(()).0,
            settings: watch::channel(()).0,
        }
    }
    pub fn source(&self, chat: Uuid) -> PromptSuggestionSource {
        PromptSuggestionSource {
            chat,
            hub: self.clone(),
        }
    }
    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().enabled
    }
    /// Clears every offered prompt when the preference changes, including in-flight samples.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap();
        if state.enabled == enabled {
            return;
        }
        state.enabled = enabled;
        state.generation += 1;
        state.values.clear();
        self.settings.send_replace(());
        self.updates.send_replace(());
    }
    pub fn current(&self, chat: Uuid) -> Option<PromptSuggestion> {
        self.state
            .lock()
            .unwrap()
            .values
            .iter()
            .find(|(id, _)| *id == chat)
            .map(|(_, value)| value.clone())
    }
    /// Subscribe before reading the current value. A notification may concern another chat.
    pub fn changes(&self) -> watch::Receiver<()> {
        self.updates.subscribe()
    }
    pub(crate) fn detach(&self, chat: Uuid) {
        let mut state = self.state.lock().unwrap();
        state.owners.remove(&chat);
        if state.remove(chat) {
            self.updates.send_replace(());
        }
    }
}

impl Cache {
    fn remove(&mut self, chat: Uuid) -> bool {
        let len = self.values.len();
        self.values.retain(|(key, _)| *key != chat);
        len != self.values.len()
    }
}

#[derive(Clone, Debug)]
pub struct PromptSuggestionSource {
    chat: Uuid,
    hub: PromptSuggestions,
}

/// Current turn identity and outstanding sample belong to one native transport.
pub(crate) struct Native {
    source: PromptSuggestionSource,
    owner: Uuid,
    setting: watch::Receiver<()>,
    turn: Option<(Uuid, u64)>,
    completed: bool,
    sample: Option<(String, String)>,
    claude_seen: VecDeque<Uuid>,
    claude_initialized: bool,
    claude_opted_in: bool,
    claude_enabled: bool,
}

impl Native {
    pub(crate) fn open(source: PromptSuggestionSource) -> Self {
        let owner = Uuid::new_v4();
        let setting = source.hub.settings.subscribe();
        let mut state = source.hub.state.lock().unwrap();
        state.owners.insert(source.chat, owner);
        if state.remove(source.chat) {
            source.hub.updates.send_replace(());
        }
        drop(state);
        Self {
            source,
            owner,
            setting,
            turn: None,
            completed: false,
            sample: None,
            claude_seen: VecDeque::new(),
            claude_initialized: false,
            claude_opted_in: false,
            claude_enabled: false,
        }
    }
    pub(crate) fn start(&mut self, turn: Option<Uuid>) {
        self.completed = false;
        self.sample = None;
        let mut cache = self.source.hub.state.lock().unwrap();
        if cache.owners.get(&self.source.chat) != Some(&self.owner) {
            return;
        }
        self.turn = turn
            .filter(|_| cache.enabled)
            .map(|id| (id, cache.generation));
        if cache.remove(self.source.chat) {
            self.source.hub.updates.send_replace(());
        }
    }
    pub(crate) fn complete(&mut self, turn: Uuid) -> bool {
        if self.completed {
            return false;
        }
        let cache = self.source.hub.state.lock().unwrap();
        self.completed = cache.enabled
            && cache.owners.get(&self.source.chat) == Some(&self.owner)
            && self.turn == Some((turn, cache.generation));
        self.completed
    }
    pub(crate) fn request(&mut self, root: &str, native_turn: &str) -> serde_json::Value {
        let id = format!("{PREFIX}{}", Uuid::new_v4());
        self.sample = Some((id.clone(), native_turn.into()));
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/sample","params":{
            "threadId":root,"expectedTurnId":native_turn,"effort":"low",
            "prompt":"Predict the next message the user will send in this conversation. Reply with only that message, written as the user would type it, in under 20 words. Do not use tools. If there is no clear next step, reply with nothing."
        }})
    }
    pub(crate) fn result(&mut self, packet: &serde_json::Value, secrets: &[String]) {
        if let Some((id, turn)) = &self.sample
            && packet["id"] == *id
        {
            let valid = packet["result"]["sourceTurnId"] == *turn;
            self.sample = None;
            if valid
                && let Some(text) = packet["result"]["text"].as_str()
                && !text.trim_start().starts_with("Delegated task")
            {
                self.offer(text, secrets);
            }
        }
    }
    pub(crate) fn claude_message(
        &mut self,
        packet: &serde_json::Value,
        idle: bool,
        secrets: &[String],
    ) {
        if packet["isReplay"] == true || packet["is_replay"] == true {
            return;
        }
        let Some(id) = packet["uuid"].as_str().and_then(|s| s.parse().ok()) else {
            return;
        };
        if self.claude_seen.contains(&id) {
            return;
        }
        self.claude_seen.push_back(id);
        if self.claude_seen.len() > 256 {
            self.claude_seen.pop_front();
        }
        if idle && let Some(text) = packet["suggestion"].as_str() {
            self.offer(text, secrets);
        }
    }
    fn offer(&self, text: &str, secrets: &[String]) {
        let text = text.trim();
        let Some((turn_id, generation)) = self.turn else {
            return;
        };
        if !self.completed || text.is_empty() {
            return;
        }
        let mut cache = self.source.hub.state.lock().unwrap();
        if !cache.enabled
            || generation != cache.generation
            || cache.owners.get(&self.source.chat) != Some(&self.owner)
        {
            return;
        }
        let mut text = text.to_owned();
        crate::transport::redact_text(&mut text, secrets);
        let next = PromptSuggestion { turn_id, text };
        if cache
            .values
            .iter()
            .any(|(chat, v)| *chat == self.source.chat && v == &next)
        {
            return;
        }
        cache.remove(self.source.chat);
        cache.values.push_back((self.source.chat, next));
        if cache.values.len() > 256 {
            cache.values.pop_front();
        }
        self.source.hub.updates.send_replace(());
    }
    pub(crate) async fn changed(native: &mut Option<Self>) {
        if let Some(native) = native {
            let _ = native.setting.changed().await;
        } else {
            std::future::pending::<()>().await
        }
    }
    pub(crate) fn initialize_claude(&mut self) -> bool {
        self.claude_initialized = true;
        self.claude_enabled = self.source.hub.enabled();
        self.claude_opted_in = self.claude_enabled;
        self.claude_enabled
    }
    pub(crate) fn claude_setting(&mut self) -> Option<(bool, bool)> {
        if !self.claude_initialized {
            return None;
        }
        let enabled = self.source.hub.enabled();
        if enabled == self.claude_enabled {
            return None;
        }
        self.claude_enabled = enabled;
        let initialize = enabled && !self.claude_opted_in;
        self.claude_opted_in |= enabled;
        Some((initialize, enabled))
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        let mut cache = self.source.hub.state.lock().unwrap();
        if cache.owners.get(&self.source.chat) == Some(&self.owner) {
            cache.owners.remove(&self.source.chat);
            if cache.remove(self.source.chat) {
                self.source.hub.updates.send_replace(());
            }
        }
    }
}
pub(crate) const PREFIX: &str = "kar-suggestion-";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn late_claude_events_and_replaced_or_disabled_controllers_cannot_restore_old_prompts() {
        let hub = PromptSuggestions::new(true);
        let chat = Uuid::new_v4();
        let mut first = Native::open(hub.source(chat));
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        first.start(Some(a));
        assert!(first.complete(a));
        let echo = json!({"uuid":Uuid::new_v4(),"suggestion":"  an idea  "});
        first.claude_message(&echo, true, &[]);
        assert_eq!(hub.current(chat).unwrap().turn_id, a);
        first.start(Some(b));
        let during = json!({"uuid":Uuid::new_v4(),"suggestion":"from previous input"});
        first.claude_message(&during, false, &[]);
        assert!(first.complete(b));
        first.claude_message(&during, true, &[]);
        first.claude_message(&echo, true, &[]);
        assert!(hub.current(chat).is_none());
        hub.set_enabled(false);
        hub.set_enabled(true);
        first.claude_message(
            &json!({"uuid":Uuid::new_v4(),"suggestion":"disabled then re-enabled"}),
            true,
            &[],
        );
        assert!(hub.current(chat).is_none());
        first.start(Some(a));
        assert!(first.complete(a));
        let mut next = Native::open(hub.source(chat));
        next.start(Some(b));
        assert!(next.complete(b));
        next.claude_message(
            &json!({"uuid":Uuid::new_v4(),"suggestion":"new"}),
            true,
            &[],
        );
        first.claude_message(
            &json!({"uuid":Uuid::new_v4(),"suggestion":"old process"}),
            true,
            &[],
        );
        drop(first);
        assert_eq!(
            hub.current(chat).unwrap(),
            PromptSuggestion {
                turn_id: b,
                text: "new".into()
            }
        );
    }
}
