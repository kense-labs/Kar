use super::super::http::ApiError;
use super::{
    audio::{self, Options},
    *,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/api/voice/transcribe", post(transcribe))
        .route("/api/voice/models/prepare", post(prepare))
        .route("/ws/voice", get(ws))
        .layer(DefaultBodyLimit::max(
            audio::PCM_MAX + audio::OPTIONS_MAX + 4,
        ))
}
struct Recording {
    app: Workbench,
    m: &'static Model,
    path: PathBuf,
    cfg: Settings,
    _gate: OwnedSemaphorePermit,
}
impl Recording {
    async fn start(app: Workbench) -> Result<Self> {
        let gate = app.0.voice.exclusive()?;
        app.0.voice.devices().await?;
        let cfg = app.0.voice.config();
        let m = model(&cfg.model)?;
        let path = app.0.voice.path(m, &cfg);
        let cp = path.clone();
        let a = app.clone();
        let valid = tokio::task::spawn_blocking(move || a.0.voice.verify(m, &cp))
            .await
            .map_err(|e| Error::Closed(e.to_string()))??;
        if !valid {
            return Err(Error::Refused(
                "speech model is missing or failed SHA-256 verification".into(),
            ));
        }
        Ok(Self {
            app,
            m,
            path,
            cfg,
            _gate: gate,
        })
    }
    async fn decode(&self, options: &Options, data: &[u8]) -> Result<String> {
        self.app
            .0
            .voice
            .recognize(self.m, &self.path, &self.cfg, options, data)
            .await
    }
}
async fn prepare(
    State(app): State<Workbench>,
) -> std::result::Result<axum::http::StatusCode, ApiError> {
    let r = Recording::start(app).await?;
    r.app.0.voice.check_load(r.m, &r.path, &r.cfg).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
async fn transcribe(
    State(app): State<Workbench>,
    body: Bytes,
) -> std::result::Result<Json<Value>, ApiError> {
    let (opts, pcm) = audio::split(&body)?;
    let r = Recording::start(app).await?;
    Ok(Json(json!({"text":r.decode(&opts,pcm).await?})))
}
async fn ws(
    State(app): State<Workbench>,
    upgrade: WebSocketUpgrade,
    lease: Option<axum::Extension<super::super::access::DeviceLease>>,
) -> Response {
    upgrade
        .max_message_size(audio::OPTIONS_MAX + 1)
        .max_frame_size(audio::OPTIONS_MAX + 1)
        .on_upgrade(move |socket| async move {
            let run = tokio::time::timeout(Duration::from_secs(600), session(app, socket));
            if let Some(axum::Extension(device)) = lease {
                tokio::select! {_=run=>{},_=device.revoked()=>{}}
            } else {
                let _ = run.await;
            }
        })
        .into_response()
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Command {
    Start { options: Options },
    Finish,
}
async fn session(app: Workbench, mut ws: WebSocket) {
    let mut state: Option<(Recording, Options)> = None;
    let mut data = Vec::new();
    let mut revision = 0;
    loop {
        let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(15), ws.recv()).await
        else {
            return;
        };
        let result: Result<(Value, bool)> = async {
            match frame {
                Message::Text(t) => {
                    if t.len() > audio::OPTIONS_MAX {
                        return Err(Error::Refused("Invalid speech stream command.".into()));
                    }
                    let cmd = serde_json::from_str::<Command>(&t)
                        .map_err(|_| Error::Refused("Invalid speech stream command.".into()))?;
                    match cmd {
                        Command::Start { options } if state.is_none() => {
                            options.validate()?;
                            state = Some((Recording::start(app.clone()).await?, options));
                            Ok((json!({"type":"ready"}), false))
                        }
                        Command::Start { .. } => {
                            Err(Error::Refused("Speech stream already started.".into()))
                        }
                        Command::Finish => {
                            let (s, opts) = state.as_ref().ok_or_else(|| {
                                Error::Refused(
                                    "Start speech transcription before finishing.".into(),
                                )
                            })?;
                            let text = if data.is_empty() {
                                String::new()
                            } else {
                                s.decode(opts, &data).await?
                            };
                            Ok((json!({"type":"finished","text":text}), true))
                        }
                    }
                }
                Message::Binary(chunk) => {
                    if state.is_none() {
                        return Err(Error::Refused(
                            "Start speech transcription before sending audio.".into(),
                        ));
                    }
                    if chunk.len() > audio::CHUNK || data.len() + chunk.len() > audio::PCM_MAX {
                        return Err(Error::Refused("Speech audio chunk is too large.".into()));
                    }
                    audio::validate(&chunk)?;
                    data.extend_from_slice(&chunk);
                    revision += 1;
                    Ok((
                        json!({"type":"update","revision":revision,"text":null}),
                        false,
                    ))
                }
                _ => Ok((Value::Null, false)),
            }
        }
        .await;
        let (message, close) = match result {
            Ok((v, done)) => (v, done.then_some(1000)),
            Err(e) => (json!({"type":"error","message":e.to_string()}), Some(1008)),
        };
        if !message.is_null()
            && ws
                .send(Message::Text(message.to_string().into()))
                .await
                .is_err()
        {
            return;
        }
        if let Some(code) = close {
            let _ = ws
                .send(Message::Close(Some(CloseFrame {
                    code,
                    reason: if code == 1000 {
                        "".into()
                    } else {
                        "Speech stream ended".into()
                    },
                })))
                .await;
            return;
        }
    }
}
