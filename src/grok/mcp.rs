use crate::{Direction, Error, McpServer, Result, SessionOptions, Subscription, stream::Journal};
use std::{collections::BTreeSet, time::Duration};

pub(crate) struct Startup {
    records: Subscription,
    servers: BTreeSet<String>,
}
impl Startup {
    /// Subscribe before the native actor starts. Live frames keep their full payload even with
    /// a tiny journal, including notifications that precede the session/new or resume reply.
    pub(crate) fn for_options(options: &SessionOptions, journal: &Journal) -> Result<Option<Self>> {
        let servers: BTreeSet<_> = options
            .mcp_servers
            .iter()
            .filter(|(_, s)| matches!(s, McpServer::HostedHttp { .. }))
            .map(|(name, _)| name.clone())
            .collect();
        if servers.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            records: journal.subscribe(None)?,
            servers,
        }))
    }

    pub(crate) async fn wait(mut self, id: &str, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            let mut initialized = false;
            let mut ready = BTreeSet::new();
            while let Some(record) = self.records.next().await {
                let frame = &record.native;
                let params = &frame["params"];
                if record.direction != Direction::FromRuntime
                    || params["sessionId"].as_str() != Some(id)
                {
                    continue;
                }
                match frame["method"].as_str() {
                    Some("_x.ai/mcp_initialized") => initialized = true,
                    Some("_x.ai/mcp/server_status") => {
                        let Some(name) = params["name"].as_str() else {
                            continue;
                        };
                        if !self.servers.contains(name) {
                            continue;
                        }
                        match params["status"].as_str() {
                            None | Some("initializing") => continue,
                            Some("ready") => {
                                ready.insert(name.to_owned());
                            }
                            Some(status) => {
                                return Err(Error::Refused(format!(
                                    "Grok served MCP server {name:?}: {}",
                                    params["reason"].as_str().unwrap_or(status)
                                )));
                            }
                        }
                    }
                    _ => continue,
                }
                if initialized && ready.len() == self.servers.len() {
                    return Ok(());
                }
            }
            Err(Error::Closed("Grok exited during MCP startup".into()))
        })
        .await
        .map_err(|_| Error::Timeout("Grok MCP startup".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordKind;
    use serde_json::json;

    fn setup() -> (Journal, Startup) {
        let journal = Journal::new(Some(1)).unwrap();
        let mut opts = SessionOptions::new(".");
        for name in ["first", "second"] {
            opts.mcp_servers.insert(
                name.into(),
                McpServer::HostedHttp {
                    url: String::new(),
                    headers: Default::default(),
                },
            );
        }
        let startup = Startup::for_options(&opts, &journal).unwrap().unwrap();
        (journal, startup)
    }

    fn send(journal: &Journal, id: &str, name: Option<&str>, status: &str) {
        let (method, params) = match name {
            Some(name) => (
                "_x.ai/mcp/server_status",
                json!({"sessionId":id,"name":name,"status":status}),
            ),
            None => ("_x.ai/mcp_initialized", json!({"sessionId":id})),
        };
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            json!({"method":method,"params":params}),
            vec![],
        );
    }

    #[tokio::test]
    async fn waits_for_this_sessions_discovery_and_each_served_endpoint() {
        for early_init in [true, false] {
            let (journal, startup) = setup();
            send(&journal, "other", Some("first"), "failed");
            send(&journal, "other", None, "");
            send(&journal, "chat", Some("first"), "ready");
            send(&journal, "chat", Some("unrelated"), "failed");
            if early_init {
                send(&journal, "chat", None, "");
            } else {
                send(&journal, "chat", Some("second"), "ready");
            }
            let mut waiting = Box::pin(startup.wait("chat", Duration::from_secs(2)));
            tokio::select! { biased; _ = &mut waiting => panic!("startup completed early"), _ = std::future::ready(()) => {} }
            if early_init {
                send(&journal, "chat", Some("second"), "ready");
            } else {
                send(&journal, "chat", None, "");
            }
            waiting.await.unwrap();
        }
    }

    #[tokio::test]
    async fn startup_reports_failure_closure_and_timeout() {
        let (j, s) = setup();
        send(&j, "chat", Some("first"), "ready");
        send(&j, "chat", Some("first"), "failed");
        assert!(matches!(
            s.wait("chat", Duration::from_secs(1)).await,
            Err(Error::Refused(_))
        ));
        let (j, s) = setup();
        j.close();
        assert!(matches!(
            s.wait("chat", Duration::from_secs(1)).await,
            Err(Error::Closed(_))
        ));
        let (_j, s) = setup();
        assert!(matches!(
            s.wait("chat", Duration::from_millis(1)).await,
            Err(Error::Timeout(_))
        ));
    }
}
