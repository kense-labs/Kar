//! A guest can see public messages in one chat and propose a message for its owner.
use super::*;
use crate::chat::{ChatCursor, InputStatus, Item, Role};
mod retention;
mod delivery;
mod files;
use axum::{
    Extension,
    extract::{DefaultBodyLimit, Query},
    response::sse::{Event, KeepAlive, Sse},
    routing::put,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::stream;
pub(super) use retention::{maintain, needs_maintenance};
use std::{collections::HashMap, convert::Infallible, time::Duration};

const DAY_MS: u64 = 86_400_000;
const MESSAGE_BYTES: usize = 10 * 1024 * 1024;
const SHARE_PENDING_BYTES: usize = 2 * MESSAGE_BYTES;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Share {
    project: Uuid,
    chat: Uuid,
    expires_ms: u64,
    invite: String,
    revoked: bool,
    revision: u64,
    guests: BTreeMap<Uuid, Guest>,
    proposals: BTreeMap<Uuid, Proposal>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    retired: BTreeMap<Uuid, retention::Retired>,
    /// Original chat turn that published each immutable attachment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    published: BTreeMap<Uuid, retention::Published>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Guest {
    name: String,
    digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Proposal {
    author: Uuid,
    key: Uuid,
    id: Uuid,
    text: String,
    files: Vec<Attachment>,
    status: Decision,
    created_ms: u64,
    #[serde(default)]
    sequence: u64,
    #[serde(default)]
    signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery: Option<InputStatus>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Attachment {
    id: Uuid,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    base64: String,
    size: usize,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Pending,
    Sending,
    Accepted,
    Dismissed,
}
impl Proposal {
    fn view(&self, share: &Share) -> Value {
        let mut view = json!({"id":self.id,"request_id":self.key,"author":share.guests.get(&self.author).map(|g|&g.name),"text":self.text,"status":self.status,
            "attachments":self.files.iter().map(|f|json!({"id":f.id,"name":f.name,"bytes":f.size})).collect::<Vec<_>>()});
        if let Some(status) = self.delivery {
            view["delivery"] = json!(status);
        }
        view
    }
}
impl super::AccessStore {
    pub(in crate::workbench) fn pending_guest_counts(&self,at:u64)->BTreeMap<Uuid,usize> {
        let registry=self.registry.lock().unwrap();
        let mut counts=BTreeMap::new();
        for share in registry.shares.values().filter(|s|!s.revoked && at<s.expires_ms){
            let count=share.proposals.values().filter(|p|p.status==Decision::Pending).count();
            *counts.entry(share.chat).or_default()+=count;
        }
        counts
    }
}
impl Share {
    fn active(&self, app: &Workbench) -> bool {
        !self.revoked
            && now() < self.expires_ms
            && !app.archived(self.chat)
            && app.binding(self.project, self.chat).is_ok()
    }
}

#[derive(Deserialize, Default)]
struct ShareOptions {
    ttl_seconds: Option<u32>,
}
async fn create(
    State(app): State<Workbench>,
    Path((project, chat)): Path<(Uuid, Uuid)>,
    Json(opts): Json<ShareOptions>,
) -> AccessResult<Json<Value>> {
    app.binding(project, chat)?;
    if app.archived(chat) {
        return Err(AccessError::Bad("chat is archived".into()));
    }
    let ttl = u64::from(opts.ttl_seconds.unwrap_or(86400)) * 1000;
    if !(1000..=DAY_MS).contains(&ttl) {
        return Err(AccessError::Bad(
            "share TTL must be between one second and one day".into(),
        ));
    }
    update(app, move |db| {
        for old in db.shares.values_mut().filter(|s| s.chat == chat) {
            old.revoked = true
        }
        let id = Uuid::new_v4();
        let code = credential()?;
        let expires_ms = now() + ttl;
        db.shares.insert(
            id,
            Share {
                project,
                chat,
                expires_ms,
                invite: digest(&code),
                revoked: false,
                revision: 0,
                guests: Default::default(),
                proposals: Default::default(),
                retired: Default::default(),
                published: Default::default(),
            },
        );
        Ok(json!({"id":id,"project":project,"chat":chat,"expires_ms":expires_ms,"credential":code}))
    })
    .await
}
async fn revoke_share(
    State(app): State<Workbench>,
    Path(id): Path<Uuid>,
) -> AccessResult<Json<()>> {
    update(app, move |db| {
        db.shares.get_mut(&id).ok_or(AccessError::Missing)?.revoked = true;
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
struct Join {
    credential: String,
    name: String,
}
async fn redeem(
    State(app): State<Workbench>,
    Path(id): Path<Uuid>,
    Json(input): Json<Join>,
) -> AccessResult<Json<Value>> {
    let host = app.clone();
    update(app,move|db|{
        let s=db.shares.get_mut(&id).filter(|s|s.active(&host)).ok_or(AccessError::Invalid)?;
        if !bool::from(digest(&input.credential).as_bytes().ct_eq(s.invite.as_bytes())){return Err(AccessError::Invalid)}
        let name=input.name.trim();
        if name.is_empty() || name.chars().count()>40 || name.chars().any(char::is_control){return Err(AccessError::Bad("guest name must fit on one line and be at most 40 characters".into()))}
        let guest=Uuid::new_v4();let code=credential()?;
        s.guests.insert(guest,Guest{name:name.into(),digest:digest(&code)});
        Ok(json!({"share":id,"guest":guest,"token":format!("{guest}.{code}"),"expires_ms":s.expires_ms}))
    }).await
}

#[derive(Clone, Copy)]
struct Principal {
    share: Uuid,
    guest: Uuid,
}

async fn guest_auth(
    State(app): State<Workbench>,
    Path(params): Path<HashMap<String, String>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(id) = params.get("share").and_then(|v| v.parse().ok()) else {
        return AccessError::Invalid.into_response();
    };
    let Some((guest, secret)) = bearer(req.headers()).split_once('.') else {
        return AccessError::Invalid.into_response();
    };
    let Ok(guest) = guest.parse() else {
        return AccessError::Invalid.into_response();
    };
    let mut updates = app.0.access.changed.subscribe();
    let expires = {
        let db = app.0.access.registry.lock().unwrap();
        let Some(s) = db.shares.get(&id).filter(|s| s.active(&app)) else {
            return AccessError::Invalid.into_response();
        };
        if !s
            .guests
            .get(&guest)
            .is_some_and(|g| bool::from(g.digest.as_bytes().ct_eq(digest(secret).as_bytes())))
        {
            return AccessError::Invalid.into_response();
        }
        s.expires_ms
    };
    req.extensions_mut().insert(Principal { share: id, guest });
    let expiry = tokio::time::sleep(Duration::from_millis(expires.saturating_sub(now())));
    let revoked = async move {
        tokio::pin!(expiry);
        loop {
            if !app
                .0
                .access
                .registry
                .lock()
                .unwrap()
                .shares
                .get(&id)
                .is_some_and(|s| s.active(&app))
            {
                return;
            }
            tokio::select! {_=&mut expiry=>return,r=updates.changed()=>if r.is_err(){return}}
        }
    };
    restrict(req, next, revoked).await
}

#[derive(Deserialize)]
struct FileInput {
    #[serde(default)]
    content_type: String,
    name: String,
    base64: String,
}
#[derive(Deserialize)]
struct Submission {
    request_id: Uuid,
    text: String,
    #[serde(default)]
    attachments: Vec<FileInput>,
}
fn attachments(input: Vec<FileInput>) -> AccessResult<Vec<Attachment>> {
    if input.len() > 4 {
        return Err(AccessError::Bad("at most four attachments".into()));
    }
    let mut files = Vec::new();
    let mut size = 0;
    for f in input {
        if f.name.trim().is_empty()
            || f.name.len() > 120
            || matches!(f.name.as_str(), "." | "..")
            || f.name.contains(['/', '\\', ':'])
            || f.name.chars().any(char::is_control)
        {
            return Err(AccessError::Bad("invalid attachment name".into()));
        }
        let bytes = STANDARD
            .decode(&f.base64)
            .map_err(|_| AccessError::Bad("invalid attachment data".into()))?;
        let mime = f
            .content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if bytes.is_empty()
            || (mime.starts_with("image/")
                && !crate::InputImage::from_bytes(&bytes).is_ok_and(|image| image.mime_type() == mime))
        {
            return Err(AccessError::Bad("empty file or unsupported image".into()));
        }
        let mime = if mime.is_empty()
            || mime.len() > 100
            || !mime.is_ascii()
            || mime.chars().any(char::is_control)
        {
            "application/octet-stream".to_owned()
        } else {
            mime
        };
        crate::chat::Attachment::validate(&f.name, &mime, delivery::kind(&mime), bytes.len() as u64)
            .map_err(|e| AccessError::Bad(e.to_string()))?;
        size += bytes.len();
        if size > MESSAGE_BYTES {
            return Err(AccessError::Bad("attachments exceed 10 MiB".into()));
        }
        files.push(Attachment {
            id: Uuid::new_v4(),
            name: f.name,
            size: bytes.len(),
            base64: STANDARD.encode(bytes),
            mime: Some(mime),
        });
    }
    Ok(files)
}
async fn submit(
    State(app): State<Workbench>,
    Extension(who): Extension<Principal>,
    Json(p): Json<Submission>,
) -> AccessResult<Json<Value>> {
    if p.text.chars().count() > 4000 || (p.text.trim().is_empty() && p.attachments.is_empty()) {
        return Err(AccessError::Bad(
            "supply attachments or up to 4000 characters of message text".into(),
        ));
    }
    let host = app.clone();
    update(app, move |db| {
        let s = db
            .shares
            .get_mut(&who.share)
            .filter(|s| s.active(&host))
            .ok_or(AccessError::Invalid)?;
        let files = attachments(p.attachments)?;

        if let Some(old) = s
            .proposals
            .values()
            .find(|v| v.author == who.guest && v.key == p.request_id)
        {
            retention::check_signature(&old.signature, &p.text, &files)?;
            return Ok(old.view(s));
        }
        if let Some((id, old)) = s
            .retired
            .iter()
            .find(|(_, v)| v.author == who.guest && v.key == p.request_id)
        {
            return old.replay(*id, s, &p.text, files);
        }
        let pending: Vec<_> = s
            .proposals
            .values()
            .filter(|v| matches!(v.status, Decision::Pending | Decision::Sending))
            .collect();
        let size: usize = pending
            .iter()
            .flat_map(|p| &p.files)
            .chain(&files)
            .map(|f| f.size)
            .sum();
        if pending.len() >= 3 || size > SHARE_PENDING_BYTES {
            return Err(AccessError::Conflict(
                "share has reached its pending proposal or attachment limit".into(),
            ));
        }
        let signature = retention::signature(&p.text, &files, true);
        let item = Proposal {
            author: who.guest,
            key: p.request_id,
            id: Uuid::new_v4(),
            text: p.text,
            files,
            status: Decision::Pending,
            created_ms: now(),
            sequence: s.revision + 1,
            signature,
            delivery: None,
        };
        let view = item.view(s);
        s.proposals.insert(item.id, item);
        s.revision += 1;
        Ok(view)
    })
    .await
}

async fn attached(
    State(app): State<Workbench>,
    Extension(who): Extension<Principal>,
    Path((_, id)): Path<(Uuid, Uuid)>,
) -> AccessResult<Response> {
    let (bytes, mime) = tokio::task::spawn_blocking(move || files::read_attachment(&app, who, id))
        .await
        .map_err(|e| Error::Closed(e.to_string()))??;
    let (mime, disposition) = match mime.as_deref() {
        Some(m @ ("image/png" | "image/jpeg" | "image/webp" | "image/gif")) => (m, "inline"),
        _ => ("application/octet-stream", "attachment"),
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disposition),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        bytes,
    )
        .into_response())
}

fn visible(app: &Workbench, who: Principal) -> AccessResult<Value> {
    let (project, chat, revision, proposals) = {
        let db = app.0.access.registry.lock().unwrap();
        let share = db
            .shares
            .get(&who.share)
            .filter(|s| s.active(app))
            .ok_or(AccessError::Invalid)?;
        (
            share.project,
            share.chat,
            share.revision,
            share
                .proposals
                .values()
                .filter(|p| p.author == who.guest)
                .map(|p| p.view(share))
                .collect::<Vec<_>>(),
        )
    };
    app.binding(project, chat)?;
    let snap = app.0.chats.feed_snapshot(chat)?;
    let mut files = files::chat_files(app, &snap.history);
    let turns: Vec<_> = snap
        .history
        .turns
        .into_iter()
        .map(|t| {
            let messages: Vec<_> = t
                .items
                .into_iter()
                .filter_map(|i| match i.item {
                    Item::Message {
                        role: role @ (Role::User | Role::Assistant),
                        text,
                        ..
                    } if i.source.path.is_empty() => Some(json!({"role":role,"text":text})),
                    _ => None,
                })
                .collect();
            let mut turn = json!({"id":t.id,"status":t.status,"messages":messages});
            if let Some(attached) = files.remove(&t.id) {
                turn["attachments"] = json!(attached.iter().map(files::Visible::view).collect::<Vec<_>>());
            }
            turn
        })
        .collect();
    Ok(
        json!({"share":who.share,"chat":chat,"cursor":format!("{}@{revision}",snap.cursor),"turns":turns,"proposals":proposals}),
    )
}
async fn snapshot(
    State(app): State<Workbench>,
    Extension(who): Extension<Principal>,
) -> AccessResult<Json<Value>> {
    tokio::task::spawn_blocking(move || visible(&app, who))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
        .map(Json)
}
#[derive(Deserialize, Default)]
struct After {
    after: Option<String>,
}
async fn events(
    State(app): State<Workbench>,
    Extension(who): Extension<Principal>,
    Query(q): Query<After>,
    headers: HeaderMap,
) -> AccessResult<Response> {
    let after = q.after.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned)
    });
    if let Some(id) = &after {
        let (cursor, revision) = id.split_once('@').ok_or(Error::InvalidCursor)?;
        let cursor: ChatCursor = cursor.parse()?;
        let revision: u64 = revision.parse().map_err(|_| Error::InvalidCursor)?;
        let (chat, head) = {
            let db = app.0.access.registry.lock().unwrap();
            let s = db.shares.get(&who.share).ok_or(AccessError::Missing)?;
            (s.chat, s.revision)
        };
        if revision > head {
            return Err(Error::InvalidCursor.into());
        }
        app.0.chats.follow(chat, Some(cursor))?;
    }
    let updates = app.0.access.changed.subscribe();
    let stream = stream::unfold(
        (app, updates, after),
        move |(app, mut updates, mut last)| async move {
            loop {
                let work = app.clone();
                let value = tokio::task::spawn_blocking(move || visible(&work, who))
                    .await
                    .ok()?
                    .ok()?;
                let cursor = value["cursor"].as_str()?.to_owned();
                if last.as_ref() != Some(&cursor) {
                    let event = Event::default()
                        .event("snapshot")
                        .id(&cursor)
                        .json_data(value)
                        .ok()?;
                    last = Some(cursor);
                    return Some((Ok::<_, Infallible>(event), (app, updates, last)));
                }
                tokio::select! {r=updates.changed()=>if r.is_err(){return None}, _=tokio::time::sleep(Duration::from_millis(500))=>{}}
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
async fn proposals(
    State(app): State<Workbench>,
    Path(id): Path<Uuid>,
) -> AccessResult<Json<Value>> {
    let db = app.0.access.registry.lock().unwrap();
    let share = db.shares.get(&id).ok_or(AccessError::Missing)?;
    Ok(Json(json!(
        share
            .proposals
            .values()
            .map(|p| p.view(share))
            .collect::<Vec<_>>()
    )))
}
#[derive(Deserialize)]
struct Review {
    accept: bool,
}
async fn review(
    State(app): State<Workbench>,
    Path((id, msg)): Path<(Uuid, Uuid)>,
    Json(r): Json<Review>,
) -> AccessResult<Json<Value>> {
    let host = app.clone();
    let Json((view, sending)) = update(app.clone(), move |db| {
        let s = db.shares.get_mut(&id).ok_or(AccessError::Missing)?;
        if let Some(old) = s.retired.get(&msg) {
            if (r.accept && old.status == Decision::Accepted)
                || (!r.accept && old.status == Decision::Dismissed)
            {
                return Ok((old.receipt(msg, s), false));
            }
            return Err(AccessError::Conflict("proposal is already decided".into()));
        }
        let active = s.active(&host);
        let p = s.proposals.get_mut(&msg).ok_or(AccessError::Missing)?;
        match (r.accept, p.status) {
            (true, Decision::Accepted | Decision::Sending) | (false, Decision::Dismissed) => {}
            (false, Decision::Pending) => {
                p.status = Decision::Dismissed;
                s.revision += 1;
            }
            (true, Decision::Pending) if active => {
                p.status = Decision::Sending;
                s.revision += 1;
            }
            _ => {
                return Err(AccessError::Conflict(
                    "proposal is already decided or the share is inactive".into(),
                ));
            }
        };
        let p = &s.proposals[&msg];
        Ok((p.view(s), p.status == Decision::Sending))
    })
    .await?;
    if !sending {
        return Ok(Json(view));
    }
    app.wake_queue();
    // An immediate native receipt is useful to callers. A busy or offline runtime leaves a
    // durable, inspectable decision; dispatch does not depend on this HTTP connection.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let work = app.clone();
        let result: AccessResult<(Value, bool)> = tokio::task::spawn_blocking(move || {
            delivery::reconcile(&work, id, msg)?;
            let db = work.0.access.registry.lock().unwrap();
            let s = db.shares.get(&id).ok_or(AccessError::Missing)?;
            if let Some(p) = s.proposals.get(&msg) {
                return Ok((
                    p.view(s),
                    p.status == Decision::Sending && p.delivery != Some(InputStatus::Uncertain),
                ));
            }
            Ok((
                s.retired
                    .get(&msg)
                    .ok_or(AccessError::Missing)?
                    .receipt(msg, s),
                false,
            ))
        })
        .await
        .map_err(|e| Error::Closed(e.to_string()))?;
        let (view, waiting) = result?;
        if !waiting || tokio::time::Instant::now() >= deadline {
            return Ok(Json(view));
        }
        tokio::time::sleep(Duration::from_millis(125)).await;
    }
}

fn attachment_path(msg: Uuid, f: &Attachment) -> PathBuf {
    PathBuf::from(".kar-attachments")
        .join(msg.to_string())
        .join(format!("{}-{}", f.id, f.name))
}

fn record_delivery(app: &Workbench, id: Uuid, msg: Uuid, status: InputStatus) -> AccessResult<()> {
    let decision = match status {
        InputStatus::Sent | InputStatus::Duplicate => Decision::Accepted,
        InputStatus::Refused
        | InputStatus::Cancelled
        | InputStatus::Withdrawn
        | InputStatus::TooLarge => Decision::Dismissed,
        InputStatus::Pending | InputStatus::Sending | InputStatus::Uncertain => Decision::Sending,
    };
    let current = {
        let db = app.0.access.registry.lock().unwrap();
        db.shares
            .get(&id)
            .and_then(|s| s.proposals.get(&msg))
            .is_some_and(|p| p.status == Decision::Sending && p.delivery != Some(status))
    };
    if current {
        app.0.access.update(|db| {
            let s = db.shares.get_mut(&id).ok_or(AccessError::Missing)?;
            if let Some(p) = s
                .proposals
                .get_mut(&msg)
                .filter(|p| p.status == Decision::Sending && p.delivery != Some(status))
            {
                p.status = decision;
                p.delivery = Some(status);
                s.revision += 1;
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub(in crate::workbench) fn reconcile_guest_inputs(app: &Workbench) {
    let (mut sending, cleanup) = {
        let db = app.0.access.registry.lock().unwrap();
        let sending: Vec<_> = db
            .shares
            .iter()
            .flat_map(|(id, s)| {
                s.proposals
                    .values()
                    .filter(|p| p.status == Decision::Sending)
                    .map(move |p| (p.created_ms, p.sequence, *id, p.id))
            })
            .collect();
        (sending, db.shares.values().any(needs_maintenance))
    };
    if cleanup && let Err(e) = app.0.access.update(|_| Ok(())) {
        eprintln!("Kar guest share retention: {e:?}");
    }
    sending.sort_unstable();
    for (_, _, share, id) in sending {
        if let Err(e) = delivery::reconcile(app, share, id) {
            eprintln!("Kar guest delivery {id}: {e:?}");
        }
    }
}

pub(super) fn admin() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/chats/{chat}/shares", post(create))
        .route("/access/shares/{id}", delete(revoke_share))
        .route("/access/shares/{id}/proposals", get(proposals))
        .route("/access/shares/{id}/proposals/{msg}", put(review))
}
pub(super) fn public(app: Workbench) -> Router<Workbench> {
    Router::new()
        .route("/guest/{share}/snapshot", get(snapshot))
        .route("/guest/{share}/events", get(events))
        .route(
            "/guest/{share}/proposals",
            post(submit).layer(DefaultBodyLimit::max(15 * 1024 * 1024)),
        )
        .route("/guest/{share}/attachments/{id}", get(attached))
        .route_layer(middleware::from_fn_with_state(app, guest_auth))
        .route("/guest/{share}/redeem", post(redeem))
}
