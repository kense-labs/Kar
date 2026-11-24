use super::Workbench;
use crate::{
    Error, Result,
    chat::{Handoff, HandoffRequest, HandoffStatus},
};
use uuid::Uuid;

impl Workbench {
    pub fn handoffs(&self, project: Uuid, chat: Uuid) -> Result<Vec<Handoff>> {
        self.binding(project, chat)?;
        Ok(self.0.chats.history(chat)?.handoffs)
    }

    pub async fn handoff(
        &self,
        project: Uuid,
        chat: Uuid,
        request: HandoffRequest,
    ) -> Result<Handoff> {
        self.binding(project, chat)?;
        let app = self.clone();
        tokio::spawn(async move {
            let controller = app.controller(chat);
            // A switch requested mid-turn must not silently occur after the caller's observed turn.
            let mut owner = controller.owner.try_lock().map_err(|_| Error::Busy)?;
            if app.archived(chat) {
                return Err(Error::Refused("chat is archived".into()));
            }
            if let Some(h) = app
                .0
                .chats
                .history(chat)?
                .handoffs
                .into_iter()
                .find(|h| h.request.id == request.id)
            {
                if h.request != request {
                    return Err(Error::Refused(
                        "handoff id belongs to another request".into(),
                    ));
                }
                if h.status != HandoffStatus::Prepared {
                    return Ok(h);
                }
            }
            if let Some(source) = &*owner {
                if source.needs_controller()?
                    || app.0.chats.queued_inputs(chat)?.iter().any(|i| {
                        matches!(
                            i.status,
                            crate::chat::InputStatus::Pending | crate::chat::InputStatus::Sending
                        )
                    })
                {
                    return Err(Error::Busy);
                }
            }
            if let Some(source) = owner.take() {
                source.close().await?;
            }
            let tools = app.0.mcp.issue(project, chat, &request.runtime)?;
            let (receipt, mut next) = app
                .0
                .chats
                .handoff(chat, request, |opts| {
                    opts.prompt_suggestions = app.suggestion_source(chat);
                    if let Some(lease) = &tools { lease.configure(opts); }
                })
                .await?;
            if let Some(session) = &mut next {
                controller.bind_model_tools(session, tools);
            }
            *owner = next;
            Ok(receipt)
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
    }
}
