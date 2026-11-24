//! Send at most one unacknowledged frame per viewer. Each operation uses the original preview grant.
use super::*;
use axum::{
    extract::ws::{Message, WebSocket},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) async fn open(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
    ws: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    let grant = grant(&app, p, id, device)?;
    Ok(ws
        .max_message_size(256 * 1024)
        .on_upgrade(move |socket| session(app, grant, socket)))
}
async fn session(app: Workbench, grant: Arc<super::super::preview::Grant>, mut socket: WebSocket) {
    let browsers = &app.0.terminal_preview.browsers;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending = None;
    let mut sequence = 0u64;
    while grant.active() {
        tokio::select! {
            _=tick.tick()=>{
                if pending.is_none(){
                    let Ok(frame)=browsers.screen(grant.clone()).await else{break};
                    if !grant.active(){break;}
                    sequence=sequence.saturating_add(1);
                    let message=json!({"type":"frame","sequence":sequence,"mime":"image/png"});
                    if socket.send(Message::Text(message.to_string().into())).await.is_err() ||
                        socket.send(Message::Binary(frame.into())).await.is_err(){break;}
                    pending=Some(sequence);
                }
            }
            incoming=socket.recv()=>{
                let Some(Ok(packet))=incoming else{break};
                if matches!(packet,Message::Close(_)){break;}
                let Message::Text(text)=packet else{continue};
                let Ok(v)=serde_json::from_str::<Value>(&text) else{break};
                if !grant.active(){break;}
                if v["type"]=="ack" {
                    if v["sequence"].as_u64()==pending{pending=None;}
                    continue;
                }
                let args=&v["arguments"];
                let result=match v["type"].as_str(){
                    Some("input")=>browsers.action(grant.clone(),"kar_preview_act",args).await,
                    Some("navigate")=>browsers.action(grant.clone(),"kar_preview_navigate",args).await,
                    Some("dialog")=>browsers.action(grant.clone(),"kar_preview_dialog",args).await,
                    Some("viewport")=>match serde_json::from_value(args.clone()){
                        Ok(viewport)=>browsers.control(grant.clone(),Some(viewport),Default::default()).await,
                        Err(e)=>Err(e.into()),
                    },
                    _=>Err(Error::Refused("unsupported preview stream operation".into())),
                };
                let reply=match result{Ok(value)=>json!({"type":"result","id":v["id"],"value":value}),Err(e)=>json!({"type":"error","id":v["id"],"message":e.to_string()})};
                if !grant.active() || socket.send(Message::Text(reply.to_string().into())).await.is_err(){break;}
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Upload {
    selector: String,
    files: Vec<FileInput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInput {
    name: String,
    base64: String,
}
pub(super) async fn upload(
    State(app): State<Workbench>,
    Path((p, id)): Path<(Uuid, Uuid)>,
    device: Device,
    Json(input): Json<Upload>,
) -> std::result::Result<StatusCode, ApiError> {
    use std::{fs, path::Component};
    let g = grant(&app, p, id, device)?;
    if input.files.is_empty() || input.files.len() > 16 {
        return Err(Error::Refused("choose between one and sixteen files".into()).into());
    }
    let dir = tempfile::tempdir().map_err(Error::from)?;
    let mut paths = vec![];
    for (i, f) in input.files.into_iter().enumerate() {
        if f.name.is_empty()
            || f.name.as_bytes().len() > 255
            || f.name.contains(['\\', '/', '\0'])
            || !matches!(
                std::path::Path::new(&f.name).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(Error::Refused("expected a plain upload filename".into()).into());
        }
        let sub = dir.path().join(i.to_string());
        fs::create_dir(&sub).map_err(Error::from)?;
        let path = sub.join(f.name);
        let data = STANDARD
            .decode(f.base64)
            .map_err(|_| Error::Refused("invalid upload base64".into()))?;
        fs::write(&path, data).map_err(Error::from)?;
        paths.push(path);
    }
    if !g.active() {
        return Err(Error::Closed("preview closed".into()).into());
    }
    app.0
        .terminal_preview
        .browsers
        .upload(g, &json!({"selector":input.selector,"paths":paths}), dir)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
