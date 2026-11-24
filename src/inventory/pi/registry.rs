use super::*;
use crate::process::OwnedProcess;
use serde_json::json;
use std::{collections::HashSet, ffi::OsString, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const PREFIX: &str = "OAR-PI-TOOLS ";
const LIMIT: u64 = 18 * 1024 * 1024;

pub(in crate::inventory) async fn tools(
    runtime: &Runtime,
    opts: &InventoryOptions,
) -> Result<InventoryResult<ToolEntry>> {
    let mut owner = None;
    let result = runtime
        .query_workspace(opts, true, async |probe, path, version, _| {
            let cmd = match reader_command(probe, path, version) {
                Ok(cmd) => cmd,
                Err(reason) => return Ok(reason),
            };
            let running = owner.insert(Reader::launch(probe, cmd)?);
            let snapshot = running.snapshot().await?;
            Ok(InventoryResult::observed(
                &probe.cwd,
                project(&snapshot)?,
                false,
                View::Registered,
            ))
        })
        .await;
    if let Some(mut running) = owner {
        if matches!(result, Ok(InventoryResult::Ok { .. })) {
            let _ = tokio::time::timeout(Duration::from_millis(500), running.child.wait()).await;
        }
        running.child.stop().await;
    }
    result
}

fn reader_command(
    probe: &Probe,
    path: &Path,
    version: Option<&str>,
) -> std::result::Result<tokio::process::Command, InventoryResult<ToolEntry>> {
    use crate::pi_sdk;
    let configured = probe.get("OAR_PI_SDK_DIR").filter(|p| !p.is_empty());
    if let Some(dir) = configured
        && pi_sdk::sdk(path).is_none()
    {
        let companion = pi_sdk::companion(&probe.cwd.join(dir), version).map_err(|e| {
            let reason = e.message();
            match e {
                pi_sdk::CompanionError::Installation => {
                    InventoryResult::unsupported(UnsupportedCode::InstallationUnsupported, reason)
                }
                pi_sdk::CompanionError::Version {
                    installed,
                    required,
                } => InventoryResult::Unsupported {
                    code: UnsupportedCode::VersionUnsupported,
                    reason: reason.into(),
                    installed_version: Some(installed),
                    required_version: required,
                },
            }
        })?;
        let mut node = probe.command(&pi_sdk::node_for(path));
        node.arg(companion.entry);
        return Ok(rpc_command(probe, node));
    }
    if pi_sdk::standalone(path) {
        return Err(InventoryResult::unsupported(
            UnsupportedCode::NativeQueryUnavailable,
            "This standalone Pi requires a matching official SDK in OAR_PI_SDK_DIR to read its registered tools",
        ));
    }
    Ok(command(probe, path))
}

fn project(snapshot: &Value) -> Result<Vec<ToolEntry>> {
    let active = rows(&snapshot["active"])?
        .iter()
        .map(|n| n.as_str().ok_or_else(invalid))
        .collect::<Result<HashSet<_>>>()?;
    rows(&snapshot["tools"])?
        .iter()
        .map(|row| {
            let name = name(row)?;
            Ok(ToolEntry {
                active: Some(active.contains(name.as_str())),
                name,
                description: Some(field(row, "description").ok_or_else(invalid)?),
                input_schema: row["parameters"].as_object().cloned(),
                ..Default::default()
            })
        })
        .collect()
}

struct Reader {
    child: OwnedProcess,
    _files: tempfile::TempDir,
}
impl Reader {
    fn launch(probe: &Probe, mut cmd: tokio::process::Command) -> Result<Self> {
        let files = tempfile::tempdir()?;
        let library = files.path().join("pi-tools.node");
        std::fs::write(
            &library,
            include_bytes!(concat!(env!("OUT_DIR"), "/pi-tools.node")),
        )?;
        let mut options = probe
            .get("NODE_OPTIONS")
            .map(OsString::from)
            .unwrap_or_default();
        if options
            .to_string_lossy()
            .split_whitespace()
            .any(|s| s == "--no-addons")
        {
            return Err(Error::Unsupported(
                "Pi's Node host has disabled native addons".into(),
            ));
        }
        options.push(" --experimental-addon-modules");
        cmd.arg("--extension")
            .arg(library)
            .env("NODE_OPTIONS", options)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(Self {
            child: OwnedProcess::spawn(cmd).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::Unsupported("Pi's registry host is unavailable".into())
                } else {
                    e.into()
                }
            })?,
            _files: files,
        })
    }

    async fn snapshot(&mut self) -> Result<Value> {
        let child = self.child.inner();
        let mut stdin = child.stdin.take();
        let id = uuid::Uuid::new_v4().to_string();
        let request = format!("{}\n", json!({"id":id,"type":"get_state"}));
        let pipe = stdin.as_mut().expect("piped stdin");
        pipe.write_all(request.as_bytes()).await?;
        pipe.flush().await?;
        let mut out =
            BufReader::new(child.stdout.take().expect("piped stdout").take(LIMIT)).lines();
        let mut err =
            BufReader::new(child.stderr.take().expect("piped stderr").take(1024 * 1024)).lines();
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut ready = false;
        let mut acknowledged = false;
        while stdout_open || stderr_open {
            let line = tokio::select! {
                next=out.next_line(), if stdout_open=> match next? {
                    Some(line)=>line,
                    None if out.get_ref().get_ref().limit() == 0 => return Err(invalid()),
                    None=> {stdout_open=false;stdin.take();continue}
                },
                next=err.next_line(), if stderr_open => {
                    match next? {
                        Some(line) if unsupported_loader(&line)=>return Err(unavailable_loader()),
                        None if err.get_ref().get_ref().limit() == 0 => return Err(invalid()),
                        None=>stderr_open=false,
                        _=>{}
                    }
                    continue
                }
            };
            if let Some(frame) = line.strip_prefix(PREFIX) {
                match frame {
                    "ready" => ready = true,
                    "unsupported" => return Err(unavailable_loader()),
                    "failed" => return Err(invalid()),
                    _ if acknowledged => return Ok(serde_json::from_str(frame)?),
                    _ => return Err(invalid()),
                }
            } else if let Ok(reply) = serde_json::from_str::<Value>(&line) {
                if reply["type"] == "extension_ui_request"
                    && let Some(request) = reply["id"].as_str()
                    && let Some(pipe) = stdin.as_mut()
                {
                    let cancel =
                        json!({"type":"extension_ui_response","id":request,"cancelled":true});
                    pipe.write_all(format!("{cancel}\n").as_bytes()).await?;
                    pipe.flush().await?;
                } else if reply["id"] == id
                    && reply["type"] == "response"
                    && reply["command"] == "get_state"
                {
                    if reply["success"] != true {
                        return Err(invalid());
                    }
                    // A loader failure may arrive on stderr after this reply. Close
                    // stdin in either case, but accept a snapshot only from a ready reader.
                    acknowledged = ready;
                    // This reply is Pi's startup barrier, after all session_start handlers.
                    // EOF asks the isolated RPC process to dispose its session.
                    stdin.take();
                }
            }
        }
        Err(invalid())
    }
}
fn unsupported_loader(line: &str) -> bool {
    line.contains("--experimental-addon-modules is not allowed")
        || (line.contains("pi-tools.node")
            && (line.contains("use require()")
                || line.contains("Unknown file extension")
                || line.contains("code signature")
                || line.contains("ERR_DLOPEN_FAILED")))
}
fn unavailable_loader() -> Error {
    Error::Unsupported("The selected Pi runtime cannot load the native registry reader".into())
}
