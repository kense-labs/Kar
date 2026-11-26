use super::*;
use crate::workbench::forge::checked_number;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stack: bool,
    },
}
impl Request {
    pub(in crate::workbench) fn mutates(&self) -> bool {
        !matches!(self, Self::Checks | Self::Logs { .. })
    }
}

async fn execute_stack_merge(
    c: &Context,
    target_n: u64,
    expected_head_sha: &str,
    method: &Option<String>,
    auto: bool,
) -> Result<Option<Value>, ForgeError> {
    let stack_info = super::stack::read_stack(c, target_n, true).await?;
    if stack_info.is_null() {
        return Ok(None);
    }
    let Some(layers) = stack_info["layers"].as_array() else {
        return Ok(None);
    };
    let Some(target_idx) = layers
        .iter()
        .position(|l| l["number"].as_u64() == Some(target_n))
    else {
        return Ok(None);
    };
    let open_layers: Vec<(u64, Option<String>)> = layers[..=target_idx]
        .iter()
        .filter(|l| l["state"].as_str() == Some("open"))
        .filter_map(|l| {
            let num = l["number"].as_u64()?;
            let sha = l["headSha"].as_str().map(str::to_owned);
            Some((num, sha))
        })
        .collect();
    if open_layers.len() <= 1 {
        return Ok(None);
    }
    if let Some((_, Some(top_sha))) = open_layers.iter().find(|(num, _)| *num == target_n)
        && top_sha != expected_head_sha
    {
        return Err(ForgeError::bad("pull head changed"));
    }
    let landing_base = stack_info["base"].as_str().map(str::to_owned);
    let mut merged_numbers = Vec::with_capacity(open_layers.len());
    let mut last_reply = Value::Null;

    for (idx, (layer_num, cached_sha)) in open_layers.into_iter().enumerate() {
        if idx > 0
            && !auto
            && c.kind == "github"
            && let Some(base_branch) = &landing_base
        {
            let repo_url = rest::project(c)?;
            let pull_url = issue::child(repo_url, &["pulls", &layer_num.to_string()]);
            let _ = send(
                c,
                Method::PATCH,
                pull_url,
                Some(json!({"base": base_branch})),
                true,
            )
            .await;
        }
        let sha = if layer_num == target_n {
            expected_head_sha.to_owned()
        } else if let Some(s) = cached_sha {
            s
        } else if c.kind == "github" {
            let repo_url = rest::project(c)?;
            let raw = get(
                c,
                issue::child(repo_url, &["pulls", &layer_num.to_string()]),
            )
            .await?;
            raw.pointer("/head/sha")
                .and_then(Value::as_str)
                .ok_or_else(|| ForgeError::bad("missing stack layer head sha"))?
                .to_owned()
        } else {
            return Err(ForgeError::bad("missing stack layer head sha"));
        };
        let single = Request::Merge {
            expected_head_sha: sha,
            method: method.clone(),
            auto,
            stack: false,
        };
        last_reply = Box::pin(execute(c, layer_num, &single)).await?;
        merged_numbers.push(layer_num);
    }

    Ok(Some(json!({
        "merged": !auto,
        "auto": auto,
        "stack": true,
        "merged_layers": merged_numbers,
        "last": last_reply,
    })))
}

pub(in crate::workbench::forge) async fn execute(
    c: &Context,
    n: u64,
    action: &Request,
) -> Result<Value, ForgeError> {
    if n == 0 {
        return Err(ForgeError::bad("a positive pull number is required"));
    }
    if let Request::Merge {
        expected_head_sha,
        method,
        auto,
        stack: true,
    } = action
        && let Some(stack_result) =
            execute_stack_merge(c, n, expected_head_sha, method, *auto).await?
    {
        return Ok(stack_result);
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
