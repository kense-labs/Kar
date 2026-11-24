//! Project-authorized upload tickets and chat-bound durable attachments.
use super::{
    Workbench,
    access::{credential, digest},
    http::{ApiError, blocking},
    missing,
};
use crate::{
    Error, Result,
    chat::{Attachment, AttachmentKind},
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::PathBuf,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

const TICKET_MS: u64 = 600_000;
const PENDING_MS: u64 = 86_400_000;
#[derive(Clone, Deserialize, Serialize)]
struct Upload {
    project: Uuid,
    created_ms: u64,
    spec: Spec,
    secret_hash: String,
    chat: Option<Uuid>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Download {
    #[serde(default,skip_serializing_if="std::ops::Not::not")]
    as_html:bool,
    project: Uuid,
    chat: Uuid,
    attachment: Uuid,
    secret_hash: String,
    expires_ms: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Registry {
    uploads: BTreeMap<Uuid, Upload>,
    reads: BTreeMap<Uuid, Download>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Spec {
    name: String,
    mime: String,
    kind: AttachmentKind,
    bytes: u64,
}
#[derive(Deserialize)]
struct Claim {
    attachment: Uuid,
}
type Reply<T> = std::result::Result<Json<T>, ApiError>;
fn now() -> u64 {
    super::access::now()
}
fn path(app: &Workbench, id: Uuid) -> PathBuf {
    app.0.state.join("pending-attachments").join(id.to_string())
}
fn matches_hash(token: &str, hash: &str) -> bool {
    bool::from(digest(token).as_bytes().ct_eq(hash.as_bytes()))
}
fn allow(upload: &Upload, key: &str) -> Result<()> {
    if upload.chat.is_some()
        || now() >= upload.created_ms.saturating_add(TICKET_MS)
        || !matches_hash(key, &upload.secret_hash)
    {
        return Err(missing());
    }
    Ok(())
}
async fn issue(
    State(app): State<Workbench>,
    Path(p): Path<Uuid>,
    Json(spec): Json<Spec>,
) -> Reply<Value> {
    Attachment::validate(&spec.name, &spec.mime, spec.kind, spec.bytes)?;
    blocking(move||{
        app.project(p)?;let secret=credential()?;let id=Uuid::new_v4();let at=now();
        app.update(|db|{
            if !db.projects.contains_key(&p){return Err(missing());}
            db.assets.reads.retain(|_,r|r.expires_ms>at);
            // A published chat reference owns its own bytes and can be reused without this ticket.
            let expired:Vec<_>=db.assets.uploads.iter().filter(|(_,u)|at>=u.created_ms.saturating_add(PENDING_MS)).map(|(id,_)|*id).collect();
            for old in expired{
                match fs::remove_file(path(&app,old)){Err(e) if e.kind()!=std::io::ErrorKind::NotFound=>return Err(e.into()),_=>{}}
                db.assets.uploads.remove(&old);
            }
            db.assets.uploads.insert(id,Upload{project:p,spec,secret_hash:digest(&secret),created_ms:at,chat:None});
            Ok(json!({"id":id,"upload_path":format!("/assets/upload/{id}/{secret}"),"expires_ms":at+TICKET_MS}))
        })
    }).await
}
async fn upload(
    State(app): State<Workbench>,
    Path((id, token)): Path<(Uuid, String)>,
    body: Body,
) -> Reply<Value> {
    let (size, project) = {
        let db = app.0.catalog.lock().unwrap();
        let u = db.assets.uploads.get(&id).ok_or_else(missing)?;
        allow(u, &token)?;
        (u.spec.bytes, u.project)
    };
    app.project(project)?;
    let data = to_bytes(body, size as usize)
        .await
        .map_err(|_| Error::Refused("upload exceeds declared size".into()))?;
    if data.len() as u64 != size {
        return Err(Error::Refused("upload length differs from ticket".into()).into());
    }
    blocking(move || {
        let db = app.0.catalog.lock().unwrap();
        let u = db.assets.uploads.get(&id).ok_or_else(missing)?;
        allow(u, &token)?;
        if !db.projects.contains_key(&u.project) {
            return Err(missing());
        }
        if u.spec.kind == AttachmentKind::Image
            && crate::InputImage::from_bytes(&data)?.mime_type() != u.spec.mime
        {
            return Err(Error::Refused(
                "image contents do not match declared type".into(),
            ));
        }
        let destination = path(&app, id);
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        match tmp.persist_noclobber(&destination) {
            Ok(_) => {
                #[cfg(unix)]
                File::open(parent)?.sync_all()?;
            }
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::read(destination)?.as_slice() != data.as_ref() {
                    return Err(Error::Refused(
                        "attachment already contains different bytes".into(),
                    ));
                }
            }
            Err(e) => return Err(e.error.into()),
        }
        Ok(json!({"id":id,"bytes":size}))
    })
    .await
}
async fn claim(
    State(app): State<Workbench>,
    Path((p, c)): Path<(Uuid, Uuid)>,
    Json(q): Json<Claim>,
) -> Reply<Attachment> {
    blocking(move || {
        let info = app.binding(p, c)?;
        let final_attachment = app.update(|db| {
            let project = db.projects.get(&p).ok_or_else(missing)?;
            if !app.owns_chat(db, project, &info)? {
                return Err(missing());
            }
            let grant = db.assets.uploads.get_mut(&q.attachment);
            if let Some(grant) = grant {
                if grant.project != p || grant.chat.is_some_and(|owner| owner != c) {
                    return Err(missing());
                }
                if grant.chat.is_none() {
                    if now() >= grant.created_ms.saturating_add(PENDING_MS) {
                        return Err(missing());
                    }
                    let bytes = fs::read(path(&app, q.attachment))?;
                    if bytes.len() as u64 != grant.spec.bytes {
                        return Err(Error::Refused("staged upload changed".into()));
                    }
                    let a = app.0.chats.persist_attachment(
                        c,
                        q.attachment,
                        &grant.spec.name,
                        &grant.spec.mime,
                        grant.spec.kind,
                        &bytes,
                    )?;
                    grant.chat = Some(c);
                    return Ok(a);
                }
            }
            app.0.chats.read_attachment(c, q.attachment)
        })?;
        match fs::remove_file(path(&app, q.attachment)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        Ok(final_attachment)
    })
    .await
}
async fn discard(State(app): State<Workbench>, Path((p, id)): Path<(Uuid, Uuid)>) -> Reply<Value> {
    blocking(move || {
        app.project(p)?;
        app.update(|db| {
            let u = db
                .assets
                .uploads
                .get(&id)
                .filter(|u| u.project == p)
                .ok_or_else(missing)?;
            if u.chat.is_some() {
                return Err(Error::Refused(
                    "attachment is already assigned to a chat".into(),
                ));
            }
            match fs::remove_file(path(&app, id)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            db.assets.uploads.remove(&id);
            Ok(json!({"id":id}))
        })
    })
    .await
}
async fn sign_read(
    State(app): State<Workbench>,
    Path((p, c, id)): Path<(Uuid, Uuid, Uuid)>,
) -> Reply<Value> {
    sign_view(app,p,c,id,false).await
}
async fn sign_html(State(app):State<Workbench>,Path((p,c,id)):Path<(Uuid,Uuid,Uuid)>)->Reply<Value>{
    sign_view(app,p,c,id,true).await
}
async fn sign_view(app:Workbench,p:Uuid,c:Uuid,id:Uuid,as_html:bool)->Reply<Value>{
    blocking(move || {
        app.binding(p, c)?;
        let a=app.0.chats.visible_attachment(c, id)?;
        if as_html && (a.kind!=AttachmentKind::File || a.mime!="text/html"){return Err(missing());}
        let key = credential()?;
        let ticket = Uuid::new_v4();
        let at = now();
        let expires_ms = at + TICKET_MS;
        app.update(|db| {
            db.assets.reads.retain(|_, r| r.expires_ms > at);
            db.assets.reads.insert(
                ticket,
                Download {
                    as_html,
                    project: p,
                    chat: c,
                    attachment: id,
                    secret_hash: digest(&key),
                    expires_ms,
                },
            );
            Ok(json!({"read_path":format!("/assets/read/{ticket}/{key}"),"expires_ms":expires_ms}))
        })
    })
    .await
}
async fn download(
    State(app): State<Workbench>,
    Path((id, key)): Path<(Uuid, String)>,
) -> std::result::Result<Response, ApiError> {
    let read = app
        .0
        .catalog
        .lock()
        .unwrap()
        .assets
        .reads
        .get(&id)
        .filter(|r| r.expires_ms > now() && matches_hash(&key, &r.secret_hash))
        .cloned()
        .ok_or_else(missing)?;
    let as_html=read.as_html;
    let (a, data) = tokio::task::spawn_blocking(move || -> Result<_> {
        app.binding(read.project, read.chat)?;
        let a = app.0.chats.visible_attachment(read.chat, read.attachment)?;
        let data = fs::read(app.0.chats.attachment_path(&a))?;
        Ok((a, data))
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))??;
    // File downloads share the service origin. Keep arbitrary documents outside the browser execution context.
    if as_html && a.kind==AttachmentKind::File && a.mime=="text/html" {
        return Ok(([
            (header::CONTENT_TYPE,"text/html; charset=utf-8"),(header::X_CONTENT_TYPE_OPTIONS,"nosniff"),
            (header::CACHE_CONTROL,"private, no-store"),(header::REFERRER_POLICY,"no-referrer"),
            (header::CONTENT_SECURITY_POLICY,"sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; media-src data:; base-uri 'none'; form-action 'none'")
        ],data).into_response());
    }
    let mime = if a.kind == AttachmentKind::Image {
        a.mime.as_str()
    } else {
        "application/octet-stream"
    };
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        data,
    )
        .into_response())
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{p}/attachments/uploads", post(issue))
        .route("/projects/{p}/attachments/{id}", delete(discard))
        .route("/projects/{p}/chats/{c}/attachments", post(claim))
        .route("/projects/{p}/chats/{c}/html-renders/{id}/url",post(sign_html))
        .route(
            "/projects/{p}/chats/{c}/attachments/{id}/url",
            post(sign_read),
        )
}
pub(super) fn tickets() -> Router<Workbench> {
    Router::new()
        .route("/assets/upload/{id}/{key}", put(upload))
        .route("/assets/read/{id}/{key}", get(download))
}
