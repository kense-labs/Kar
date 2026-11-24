//! Cloud media never forwards a browser credential or exposes the upstream bearer.
use super::super::screen::View;
use super::*;
use axum::{
    body::Body,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::header,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, protocol::Role},
};

struct Endpoint {
    url: url::Url,
    auth: Auth,
    protocol: Option<String>,
}
fn endpoint(l: &Lease, input: bool) -> Result<Endpoint> {
    if l.stopping || !l.ready {
        return Err(bad("cloud lease is inactive"));
    }
    if l.host.kind == Provider::Simlock {
        let path = if input { "input" } else { "stream" };
        let url = safe_url(&format!(
            "{}/v1/leases/{}/{path}",
            l.host.gateway()?,
            segment(l.id())
        ))?;
        return Ok(Endpoint {
            url,
            auth: l.auth.clone(),
            protocol: None,
        });
    }
    let origin = l
        .preview
        .as_deref()
        .ok_or_else(|| bad("Expo preview is unavailable"))?;
    let device = l
        .remote_device
        .as_deref()
        .ok_or_else(|| bad("Expo preview has no target"))?;
    let token = l
        .preview_token
        .as_deref()
        .ok_or_else(|| bad("Expo preview token is unavailable"))?;
    let (mut url, protocol) = match (l.platform.as_str(), input) {
        ("ios", true) => (safe_url(&format!("{origin}/helper/ws"))?, "serve-sim"),
        ("ios", false) => (
            safe_url(&format!("{origin}/helper/{}/stream.avcc", segment(device)))?,
            "serve-sim",
        ),
        ("android", true) => (
            safe_url(&format!("{origin}/vendor/serve-emu/ws"))?,
            "serve-emu",
        ),
        _ => {
            return Err(Error::Unsupported(
                "Expo Android streams video over its input WebSocket".into(),
            ));
        }
    };
    if input {
        url.query_pairs_mut().append_pair("device", device);
        if l.platform == "android" {
            url.query_pairs_mut().append_pair("frame-meta", "1");
        }
    }
    Ok(Endpoint {
        url,
        auth: Auth {
            header: "authorization".into(),
            value: format!("Bearer {token}"),
        },
        protocol: Some(format!("{protocol}.token.{token}")),
    })
}
async fn target(
    app: &Workbench,
    q: &View,
    input: bool,
) -> Result<(Endpoint, Uuid, watch::Receiver<Value>)> {
    app.binding(q.project, q.chat)?;
    if app.archived(q.chat) {
        return Err(bad("device view is archived"));
    }
    let svc = &app.0.devices;
    let s = svc.live.lock().await;
    let host = q.host_id.as_deref().unwrap_or("local");
    let view = s
        .sessions
        .iter()
        .find(|v| {
            v.project == q.project
                && v.chat == q.chat
                && v.host_id == host
                && v.device_id == q.device_id
                && s.settings.enabled
        })
        .ok_or_else(|| bad("no open device view for this chat"))?;
    Ok((
        endpoint(&svc.cloud.find(host, &q.device_id)?, input)?,
        view.view_id,
        svc.events.subscribe(),
    ))
}
pub(in crate::workbench::devices) async fn video(
    State(app): State<Workbench>,
    Query(q): Query<View>,
) -> Reply<Response> {
    let (upstream, id, changes) = target(&app, &q, false).await?;
    let request = upstream
        .auth
        .apply(app.0.devices.cloud.media.get(upstream.url));
    let response = tokio::time::timeout(Duration::from_secs(12), request.send())
        .await
        .map_err(|_| bad("cloud video handshake timed out"))?
        .map_err(|_| bad("cloud video unavailable"))?;
    if !response.status().is_success() {
        return Err(bad("cloud video was refused").into());
    }
    let ty = header::HeaderValue::from_static("application/octet-stream");
    let body = futures_util::stream::unfold(
        (response, changes, app, q),
        move |(mut response, mut changes, app, q)| async move {
            loop {
                if !screen::active(&app, id, &q).await {
                    return None;
                }
                tokio::select! {
                    changed=changes.changed()=>{if changed.is_err(){return None;}},
                    bytes=response.chunk()=>{let bytes=bytes.ok()??;return Some((Ok::<_,Infallible>(bytes),(response,changes,app,q)));}
                }
            }
        },
    );
    Ok((
        [
            (header::CONTENT_TYPE, ty),
            (
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            ),
        ],
        Body::from_stream(body),
    )
        .into_response())
}
pub(in crate::workbench::devices) async fn input(
    State(app): State<Workbench>,
    Query(q): Query<View>,
    ws: WebSocketUpgrade,
) -> Reply<Response> {
    let (target, id, changes) = target(&app, &q, true).await?;
    // This client uses the same rustls transport and roots as cloud discovery.
    let key = tungstenite::handshake::client::generate_key();
    let mut request = target
        .auth
        .apply(app.0.devices.cloud.media.get(target.url))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", &key);
    if let Some(protocol) = &target.protocol {
        request = request.header("sec-websocket-protocol", protocol);
    }
    let r = tokio::time::timeout(Duration::from_secs(12), request.send())
        .await
        .map_err(|_| bad("cloud input handshake timed out"))?
        .map_err(|_| bad("cloud input unavailable"))?;
    let expected = tungstenite::handshake::derive_accept_key(key.as_bytes());
    if r.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS
        || r.headers()
            .get("sec-websocket-accept")
            .and_then(|s| s.to_str().ok())
            != Some(expected.as_str())
        || r.headers()
            .get("upgrade")
            .and_then(|s| s.to_str().ok())
            .is_none_or(|s| !s.eq_ignore_ascii_case("websocket"))
        || r.headers()
            .get("sec-websocket-protocol")
            .is_some_and(|s| s.to_str().ok() != target.protocol.as_deref())
    {
        return Err(bad("cloud device refused the WebSocket handshake").into());
    }
    let transport = r
        .upgrade()
        .await
        .map_err(|_| bad("cloud input socket unavailable"))?;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_frame_size(Some(8 << 20))
        .max_message_size(Some(8 << 20));
    let remote = WebSocketStream::from_raw_socket(transport, Role::Client, Some(config)).await;
    Ok(ws
        .max_message_size(8 << 20)
        .max_frame_size(8 << 20)
        .on_upgrade(move |local| relay(local, remote, app, id, q, changes))
        .into_response())
}
async fn relay(
    mut local: WebSocket,
    mut remote: WebSocketStream<reqwest::Upgraded>,
    app: Workbench,
    id: Uuid,
    q: View,
    mut changes: watch::Receiver<Value>,
) {
    use tungstenite::Message as Up;
    loop {
        if !screen::active(&app, id, &q).await {
            break;
        }
        tokio::select! {
            changed=changes.changed()=>if changed.is_err(){break;},
            msg=local.recv()=>{let Some(Ok(msg))=msg else{break;};let msg=match msg{Message::Binary(b)=>Up::Binary(b),Message::Text(t)=>Up::Text(t.to_string().into()),Message::Close(_)=>break,_=>continue};if remote.send(msg).await.is_err(){break;}},
            msg=remote.next()=>{let Some(Ok(msg))=msg else{break;};let msg=match msg{Up::Binary(b)=>Message::Binary(b),Up::Text(t)=>Message::Text(t.to_string().into()),Up::Close(_)=>break,_=>continue};if local.send(msg).await.is_err(){break;}},
            else=>break,
        }
    }
}
