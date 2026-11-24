//! An app's current content is frozen alongside the native prompt before delivery.
use super::*;
use crate::chat::log::State;
#[derive(Serialize, Deserialize)]
pub(super) struct Entry {
    source: Uuid,
    turn: Uuid,
    item: String,
    server: String,
    tool: String,
    text: String,
}
impl Inbox {
    pub(super) fn app_snapshot(&self, state: &State) -> Result<Option<String>> {
        let entries: Vec<_> = self
            .app_context
            .values()
            .filter(|e| e.source != state.info.id || state.turns.iter().any(|t| t.id == e.turn))
            .collect();
        if entries.is_empty() {
            Ok(None)
        } else {
            Ok(Some(serde_json::to_string(&entries)?))
        }
    }
}
impl ChatStore {
    pub(crate) fn set_app_context(
        &self,
        target: Uuid,
        source: Uuid,
        turn: Uuid,
        item: &str,
        server: &str,
        tool: &str,
        text: String,
    ) -> Result<()> {
        let _lock = self.inbox_lock(target)?;
        let info = self.saved_info(target)?;
        let mut inbox = self.read_inbox(&info)?;
        let key = format!("{source}/{turn}/{item}");
        if text.is_empty() {
            inbox.app_context.remove(&key);
        } else {
            inbox.app_context.insert(
                key,
                Entry {
                    source,
                    turn,
                    item: item.into(),
                    server: server.into(),
                    tool: tool.into(),
                    text,
                },
            );
        }
        self.save_inbox(&inbox)
    }
}
