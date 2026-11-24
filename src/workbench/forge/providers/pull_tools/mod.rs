use serde::{Deserialize,Serialize};
use std::collections::BTreeMap;
use crate::workbench::forge::checked_number;
use super::*;
mod ci;
mod host;
mod update;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Checks,
    Logs {
        check: Option<String>,
        tail_lines: Option<u16>,
    },
    Rerun {
        check: Option<String>,
    },
    Comment {
        body: String,
    },
    Update {
        title: Option<String>,
        body: Option<String>,
        state: Option<String>,
        ready: Option<bool>,
        add_labels: Option<Vec<String>>,
        remove_labels: Option<Vec<String>>,
        request_reviewers: Option<Vec<String>>,
    },
    Merge {
        expected_head_sha: String,
        method: Option<String>,
        #[serde(default)]
        auto: bool,
    },
}
impl Request {
    pub(in crate::workbench) fn mutates(&self) -> bool {
        !matches!(self, Self::Checks | Self::Logs { .. })
    }
}
pub(in crate::workbench::forge) async fn execute(
    c: &Context,
    n: u64,
    action: &Request,
) -> Result<Value, ForgeError> {
    if n == 0 {
        return Err(ForgeError::bad("a positive pull number is required"));
    }
    if c.kind != "github" {
        return host::execute(c, n, action).await;
    }
    let url = rest::project(c)?;
    let pull = issue::child(url.clone(), &["pulls", &n.to_string()]);
    let raw = get(c, pull.clone()).await?;
    if checked_number(&raw)? != n {
        return Err(ForgeError::bad("pull identity changed"));
    }
    match action {
        Request::Checks | Request::Logs { .. } | Request::Rerun { .. } => {
            ci::run(c, &url, &raw, action).await
        }
        Request::Comment { body } => {
            if body.trim().is_empty() {
                return Err(ForgeError::bad("a comment body is required"));
            }
            send(
                c,
                Method::POST,
                issue::child(url, &["issues", &n.to_string(), "comments"]),
                Some(json!({"body":body})),
                true,
            )
            .await
        }
        other => update::apply(c, url, pull, &raw, other).await,
    }
}
