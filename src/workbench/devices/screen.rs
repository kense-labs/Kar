//! One demand-driven native capture loop per device, shared by all authorized views.
use super::*;
use axum::body::{Body, Bytes};
use std::{collections::HashMap, sync::Mutex, time::Duration};

#[derive(Clone)]
enum Frame {
    Initial,
    Png(Bytes),
    End,
}
#[derive(Default)]
pub(super) struct Screens(Mutex<HashMap<(String, String), watch::Sender<Frame>>>);
impl Screens {
    fn subscribe(&self, app: &Workbench, device: Device) -> watch::Receiver<Frame> {
        let mut feeds = self.0.lock().unwrap();
        let key = (device.host_id.clone(), device.id.clone());
        if let Some(tx) = feeds.get(&key) {
            return tx.subscribe();
        }
        let (tx, rx) = watch::channel(Frame::Initial);
        feeds.insert(key.clone(), tx.clone());
        let weak = std::sync::Arc::downgrade(&app.0);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                {
                    let mut feeds = inner.devices.screens.0.lock().unwrap();
                    if tx.receiver_count() == 0 {
                        feeds.remove(&key);
                        return;
                    }
                }
                let visible = {
                    let live = inner.devices.live.lock().await;
                    live.settings.enabled
                        && live
                            .sessions
                            .iter()
                            .any(|s| s.device_id == device.id && s.host_id == device.host_id)
                };
                if !visible {
                    break;
                }
                let capture = inner.devices.capture(&device).await;
                drop(inner);
                match capture {
                    Ok(png) => {
                        tx.send_replace(Frame::Png(Bytes::from(png)));
                    }
                    Err(_) => break,
                }
                tokio::select! {_=tx.closed()=>{},_=tokio::time::sleep(Duration::from_millis(500))=>{}}
            }
            if let Some(inner) = weak.upgrade() {
                inner.devices.screens.0.lock().unwrap().remove(&key);
            }
            tx.send_replace(Frame::End);
        });
        rx
    }
}
#[derive(Deserialize)]
pub(super) struct View {
    pub(super) project: Uuid,
    pub(super) chat: Uuid,
    pub(super) device_id: String,
    pub(super) host_id: Option<String>,
}
pub(super) async fn stream(State(app): State<Workbench>, Query(q): Query<View>) -> Reply<Response> {
    app.binding(q.project, q.chat)?;
    let svc = &app.0.devices;
    let live = svc.live.lock().await;
    let host = q.host_id.as_deref().unwrap_or("local");
    let claim = live
        .sessions
        .iter()
        .find(|v| {
            v.host_id == host
                && v.project == q.project
                && v.chat == q.chat
                && v.device_id == q.device_id
                && !app.archived(q.chat)
        })
        .ok_or_else(super::super::missing)?;
    let id = claim.view_id;
    let d = live.device(host, &q.device_id)?;
    if !d.booted {
        return Err(Error::Refused("boot the device before opening its screen".into()).into());
    }
    let frame = svc.screens.subscribe(&app, d);
    let changed = svc.events.subscribe();
    drop(live);
    let data = futures_util::stream::unfold(
        (frame, changed, app, q, false),
        move |(mut frames, mut states, app, q, mut consumed)| async move {
            loop {
                if consumed {
                    tokio::select! {
                        r=frames.changed()=>{if r.is_err(){return None;}}
                        r=states.changed()=>{
                            if r.is_err() || !active(&app,id,&q).await{return None;}
                            continue;
                        }
                    }
                }
                let value = frames.borrow_and_update().clone();
                consumed = true;
                match value {
                    Frame::Initial => continue,
                    Frame::End => return None,
                    Frame::Png(bytes) => {
                        if !active(&app, id, &q).await {
                            return None;
                        }
                        let mut part = Vec::with_capacity(bytes.len() + 96);
                        part.extend_from_slice(format!("--kar-device\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",bytes.len()).as_bytes());
                        part.extend_from_slice(&bytes);
                        part.extend_from_slice(b"\r\n");
                        return Some((
                            Ok::<_, Infallible>(Bytes::from(part)),
                            (frames, states, app, q, true),
                        ));
                    }
                }
            }
        },
    );
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "multipart/x-mixed-replace; boundary=kar-device",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(data),
    )
        .into_response())
}
pub(super) async fn active(app: &Workbench, id: Uuid, q: &View) -> bool {
    if app.archived(q.chat) || app.binding(q.project, q.chat).is_err() {
        return false;
    }
    let s = app.0.devices.live.lock().await;
    s.settings.enabled && s.sessions.iter().any(|claim| claim.view_id == id)
}
