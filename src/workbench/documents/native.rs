//! A Codex turn edits a private base snapshot. Only a confirmed terminal turn can publish it.
use super::{FileError, FileQuery};
use crate::{Error, Outcome, Result, Session, SessionOptions, workbench::Workbench};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use kar_documents::{
    http::{DocumentService, Failure, receipt_response},
    native::{NativeBegin, NativeJob, NativeRequest},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path as FsPath;
use uuid::Uuid;

#[derive(Deserialize)]
pub(super) struct Edit {
    operation_id: String,
    base_revision: u64,
    runtime: String,
    model: Option<String>,
    instructions: String,
}

pub(super) async fn propose(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Query(q): Query<FileQuery>,
    Json(input): Json<Edit>,
) -> std::result::Result<Response, FileError> {
    if input.runtime != "codex" {
        return Err(Error::Unsupported(
            "the workbench native document editor currently implements Codex".into(),
        )
        .into());
    }
    let service = app.document_service(project, q.path).await?;
    let request = NativeRequest {
        author: "assistant".into(),
        operation_id: input.operation_id,
        base_revision: input.base_revision,
        runtime: input.runtime,
        model: input.model,
        instructions: input.instructions,
    };
    // Dropping an HTTP connection must not cancel dispatch or release the private copy mid-turn.
    tokio::spawn(edit(service, request))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}

async fn open(cwd: &FsPath, req: &NativeRequest, resume: Option<String>) -> Result<Session> {
    let mut rt = crate::runtime("codex")?;
    let mut opts = SessionOptions::new(cwd);
    opts.model = req.model.clone();
    opts.resume = resume.clone();
    opts.remove_env("KAR_WORKBENCH_TOKEN")?;
    for option in [
        "sandbox_mode=\"workspace-write\"",
        "approval_policy=\"never\"",
        "sandbox_workspace_write.writable_roots=[]",
        "sandbox_workspace_write.network_access=false",
        "sandbox_workspace_write.exclude_slash_tmp=true",
        "sandbox_workspace_write.exclude_tmpdir_env_var=true",
    ] {
        rt.args.extend(["-c".into(), option.into()]);
    }
    let expected = cwd.canonicalize()?;
    let session = rt.open(opts).await?;
    let info = session.info();
    let sandbox = &info["sandbox"];
    let scoped = info["cwd"]
        .as_str()
        .is_some_and(|p| FsPath::new(p) == expected)
        && info["approvalPolicy"] == "never"
        && sandbox["type"] == "workspaceWrite"
        && sandbox["excludeSlashTmp"] == true
        && sandbox["excludeTmpdirEnvVar"] == true
        && sandbox.get("networkAccess").is_none_or(|v| v == false)
        && sandbox.get("writableRoots").is_none_or(|v| {
            v.as_array().is_some_and(|roots| {
                roots
                    .iter()
                    .all(|r| r.as_str().is_some_and(|p| FsPath::new(p) == expected))
            })
        })
        && resume.as_deref().is_none_or(|id| id == session.id());
    if !scoped {
        let _ = session.close().await;
        return Err(Error::Refused(
            "Codex did not confirm the document copy's working directory and write policy".into(),
        ));
    }
    Ok(session)
}

fn status(code: StatusCode, value: &str, reason: Option<String>) -> Response {
    (code, Json(json!({"status":value, "reason":reason}))).into_response()
}

async fn edit(
    service: DocumentService,
    request: NativeRequest,
) -> std::result::Result<Response, FileError> {
    let claim = request.clone();
    match service.run(move |store| store.begin_native(claim)).await? {
        NativeBegin::Finished(receipt) => Ok(receipt_response(receipt).into_response()),
        NativeBegin::Running => Ok(status(StatusCode::ACCEPTED, "running", None)),
        NativeBegin::Failed { reason } => Ok(status(
            StatusCode::UNPROCESSABLE_ENTITY,
            "failed",
            Some(reason),
        )),
        NativeBegin::Ready(job) => dispatch(service, request, job).await,
        NativeBegin::Interrupted {
            native_session_id,
            cwd,
            prompt,
        } => {
            let Some(id) = native_session_id else {
                return Ok(status(StatusCode::CONFLICT, "interrupted", None));
            };
            let session = match open(&cwd, &request, Some(id.clone())).await {
                Ok(session) => session,
                Err(e) => {
                    return Ok(status(
                        StatusCode::CONFLICT,
                        "interrupted",
                        Some(e.to_string()),
                    ));
                }
            };
            let outcome = saved_outcome(&session, &prompt).await;
            let closed = session.close().await;
            if closed.is_err() || outcome.is_none() {
                return Ok(status(StatusCode::CONFLICT, "interrupted", None));
            }
            let job = service
                .run(move |s| s.recover_native(&request, &id))
                .await?;
            finish(service, job, outcome.unwrap()).await
        }
    }
}

async fn dispatch(
    service: DocumentService,
    request: NativeRequest,
    mut job: NativeJob,
) -> std::result::Result<Response, FileError> {
    let session = match open(job.cwd(), &request, None).await {
        Ok(session) => session,
        Err(e) => {
            job.fail(e.to_string()).map_err(Failure)?;
            return Ok(status(
                StatusCode::UNPROCESSABLE_ENTITY,
                "failed",
                Some(e.to_string()),
            ));
        }
    };
    if let Err(e) = job.mark_dispatched(Some(session.id().to_owned())) {
        let _ = session.close().await;
        return Err(Failure(e).into());
    }
    let result = match session.prompt_with_id(job.input_id(), job.prompt()).await {
        Ok(turn) => turn.wait().await,
        Err(e) => Err(e),
    };
    let closed = session.close().await;
    match (result, closed) {
        (Ok(outcome), Ok(())) => finish(service, job, outcome).await,
        (Err(e), _) | (_, Err(e)) => Ok(status(
            StatusCode::CONFLICT,
            "interrupted",
            Some(e.to_string()),
        )),
    }
}

async fn finish(
    service: DocumentService,
    job: NativeJob,
    outcome: Outcome,
) -> std::result::Result<Response, FileError> {
    if outcome == Outcome::Completed {
        let receipt = service.publish(move |s| job.finish(s)).await?;
        Ok(receipt_response(receipt).into_response())
    } else {
        let reason = format!("native turn ended with {outcome:?}");
        job.fail(reason.clone()).map_err(Failure)?;
        Ok(status(
            StatusCode::UNPROCESSABLE_ENTITY,
            "failed",
            Some(reason),
        ))
    }
}

async fn saved_outcome(session: &Session, prompt: &str) -> Option<Outcome> {
    // One dedicated, fresh native thread per operation. Never infer completion from the file alone.
    let id = session.id();
    let full = session
        .rpc("thread/read", json!({"threadId":id,"includeTurns":true}))
        .await;
    if let Ok(r) = full
        && r.pointer("/thread/id").and_then(Value::as_str) == Some(id)
        && let Some(outcome) = r
            .pointer("/thread/turns")
            .and_then(|turns| single_outcome(turns, prompt))
    {
        return Some(outcome);
    }
    let page = session
        .rpc(
            "thread/turns/list",
            json!({"threadId":id,"limit":2,"itemsView":"full"}),
        )
        .await
        .ok()?;
    if !page["nextCursor"].is_null() {
        return None;
    }
    single_outcome(&page["data"], prompt)
}

fn single_outcome(turns: &Value, prompt: &str) -> Option<Outcome> {
    let [turn] = turns.as_array()?.as_slice() else {
        return None;
    };
    turn["id"].as_str().filter(|t| !t.is_empty())?;
    let mut users = turn["items"]
        .as_array()?
        .iter()
        .filter(|i| i["type"] == "userMessage");
    let user = users.next()?;
    let [block] = user["content"].as_array()?.as_slice() else {
        return None;
    };
    if users.next().is_some() || block["type"] != "text" || block["text"] != prompt {
        return None;
    }
    match turn["status"].as_str()? {
        "completed" => Some(Outcome::Completed),
        "interrupted" => Some(Outcome::Aborted),
        "failed" => Some(Outcome::Failed),
        _ => None,
    }
}
