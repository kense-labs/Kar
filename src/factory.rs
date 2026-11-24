//! Factory Droid's native stream-jsonrpc interface.
mod projection;
pub(crate) mod recovery;
mod session;
use crate::{Catalog, Error, Inventory, Result, Runtime, SessionOptions};
pub(crate) use projection::Projection;
use serde_json::{Value, json};
pub(crate) use session::{
    answer, context, open, redact_frame, request, resolved_requests, settings,
};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
};

pub(crate) const EXEC_ARGS: &[&str] = &[
    "exec",
    "--input-format",
    "stream-jsonrpc",
    "--output-format",
    "stream-jsonrpc",
];
const API_VERSION: &str = "1.0.0";

async fn write_frame(stdin: &mut (impl AsyncWrite + Unpin), frame: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&frame)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    Ok(())
}

pub(crate) async fn inventory(
    rt: &Runtime,
    options: &SessionOptions,
    kind: Catalog,
) -> Result<Value> {
    let mut command = options.command(&rt.program)?;
    command.current_dir(&options.cwd);
    project(
        kind,
        &inventory_command(command, kind, options.handshake_timeout).await?,
    )
}
pub(crate) async fn inventory_command(
    mut command: Command,
    kind: Catalog,
    timeout: Duration,
) -> Result<Value> {
    let (method, params) = match kind {
        Catalog::Models => ("droid.list_models", json!({})),
        Catalog::Inventory(Inventory::Skills) => {
            ("droid.list_skills", json!({"includeContent":false}))
        }
        _ => return Err(Error::Unsupported("Factory pre-session inventory".into())),
    };
    command
        .args(EXEC_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::process::OwnedProcess::spawn(command)?;
    let mut stdin = child.inner().stdin.take().expect("piped stdin");
    let stdout = child.inner().stdout.take().expect("piped stdout");
    let id = uuid::Uuid::new_v4().to_string();
    let read = async {
        write_frame(&mut stdin, request(&id, method, params)).await?;
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let frame: Value = serde_json::from_str(&line)?;
            match frame["type"].as_str() {
                Some("response") if frame["id"].as_str() == Some(&id) => {
                    if let Some(error) = frame.get("error").filter(|e| !e.is_null()) {
                        if error["code"] == -32601 {
                            return Err(Error::Unsupported(format!(
                                "Factory does not expose {method}"
                            )));
                        }
                        let message = error["message"].as_str().unwrap_or("query refused");
                        return Err(Error::Refused(format!("{method}: {message}")));
                    }
                    return Ok(frame["result"].clone());
                }
                Some("request") if frame["id"].is_string() => {
                    let refusal = json!({
                        "jsonrpc":"2.0", "factoryApiVersion":API_VERSION, "type":"response",
                        "id":frame["id"], "error":{"code":-32601,"message":"Unhandled inventory request"}
                    });
                    write_frame(&mut stdin, refusal).await?;
                }
                _ => {}
            }
        }
        Err(Error::Closed(format!(
            "Factory exited before replying to {method}"
        )))
    };
    let answer = tokio::time::timeout(timeout, read)
        .await
        .unwrap_or_else(|_| Err(Error::Timeout(method.into())));
    drop(stdin);
    if tokio::time::timeout(Duration::from_millis(500), child.wait())
        .await
        .is_err()
    {
        child.stop().await;
    }
    answer
}

pub(crate) fn project(kind: Catalog, result: &Value) -> Result<Value> {
    let (key, required, fields): (_, _, &[&str]) = match kind {
        Catalog::Models => (
            "models",
            "id",
            &[
                "id",
                "displayName",
                "modelProvider",
                "supportedReasoningEfforts",
                "defaultReasoningEffort",
                "isCustom",
                "disabled",
                "disabledReason",
            ],
        ),
        Catalog::Inventory(Inventory::Skills) => (
            "skills",
            "name",
            &[
                "name",
                "description",
                "filePath",
                "location",
                "pluginId",
                "enabled",
                "userInvocable",
            ],
        ),
        _ => return Err(Error::Unsupported("Factory pre-session inventory".into())),
    };
    let rows = result[key]
        .as_array()
        .ok_or_else(|| Error::Protocol(format!("Factory returned an invalid {key} catalog")))?;
    let values: Vec<_> = rows
        .iter()
        .filter(|row| row[required].as_str().is_some_and(|s| !s.is_empty()))
        .map(|row| {
            Value::Object(
                fields
                    .iter()
                    .filter_map(|&field| Some((field.to_owned(), row.get(field)?.clone())))
                    .collect(),
            )
        })
        .collect();
    if matches!(kind, Catalog::Inventory(Inventory::Skills)) {
        let mut report = json!({"skills":values});
        if let Some(available) = result.get("projectAvailable") {
            report["projectAvailable"] = available
                .as_bool()
                .ok_or_else(|| Error::Protocol("invalid Factory project availability".into()))?
                .into();
        }
        Ok(report)
    } else {
        Ok(values.into())
    }
}
