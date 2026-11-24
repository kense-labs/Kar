use super::{
    audio::{self, Options},
    *,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as Param, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::time::{Duration, Instant};
static SLUG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^[a-z\d][a-z\d.-]*/[a-z\d][a-z\d._-]*$").unwrap());
fn model_id(s: &str) -> bool {
    (3..=160).contains(&s.len())
        && !s.to_ascii_lowercase().starts_with("openrouter/")
        && SLUG.is_match(s)
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/api/voice/cloud/keys", get(keys))
        .route("/api/voice/cloud/openrouter/models", get(catalog))
        .route("/api/voice/cloud/{provider}/key", post(set))
        .route("/api/voice/cloud/{provider}/transcribe", post(transcribe))
        .layer(DefaultBodyLimit::max(
            audio::OPTIONS_MAX + audio::PCM_MAX + 4,
        ))
}
struct CloudError(String, &'static str);
impl IntoResponse for CloudError {
    fn into_response(self) -> Response {
        let status = match self.1 {
            "invalid_audio" | "invalid_key" => StatusCode::BAD_REQUEST,
            "unknown-provider" => StatusCode::NOT_FOUND,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        (
            status,
            Json(json!({"error":self.1,"provider":self.0,"reason":self.1})),
        )
            .into_response()
    }
}
fn known(p: &str) -> std::result::Result<(), CloudError> {
    if matches!(p, "openai" | "xai" | "openrouter") {
        Ok(())
    } else {
        Err(CloudError(p.into(), "unknown-provider"))
    }
}
type Reply = std::result::Result<Json<Value>, CloudError>;
async fn keys(State(app): State<Workbench>) -> Reply {
    let mut map = json!({});
    for p in ["openai", "xai", "openrouter"] {
        map[p] = json!(
            app.0
                .voice
                .credential(p)
                .map_err(|_| CloudError(p.into(), "unreadable-key"))?
                .is_some()
        );
    }
    Ok(Json(map))
}
#[derive(Deserialize)]
struct Key {
    key: Value,
}
async fn set(
    State(app): State<Workbench>,
    Param(p): Param<String>,
    Json(key): Json<Key>,
) -> std::result::Result<StatusCode, CloudError> {
    known(&p)?;
    app.0
        .voice
        .put_key(&p, key.key)
        .map_err(|_| CloudError(p, "invalid_key"))?;
    Ok(StatusCode::NO_CONTENT)
}
fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(115))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}
fn endpoint(p: &str, tail: &str) -> String {
    let default = match p {
        "openai" => "https://api.openai.com/v1",
        "xai" => "https://api.x.ai/v1",
        _ => "https://openrouter.ai/api/v1",
    };
    let base = std::env::var(format!("KAR_VOICE_{}_BASE", p.to_ascii_uppercase()))
        .unwrap_or_else(|_| default.into());
    format!("{}/{}", base.trim_end_matches('/'), tail)
}
async fn response(mut r: reqwest::Response) -> Option<Value> {
    if r.content_length().is_some_and(|n| n > 4 << 20) {
        return None;
    }
    let mut data = vec![];
    while let Some(chunk) = r.chunk().await.ok()? {
        if data.len() + chunk.len() > 4 << 20 {
            return None;
        }
        data.extend(chunk);
    }
    serde_json::from_slice(&data).ok()
}
async fn entries(svc: &Service) -> std::result::Result<Value, CloudError> {
    let err = || CloudError("openrouter".into(), "unavailable");
    let mut cache = svc.cloud_models.lock().await;
    if let Some((time, val)) = cache
        .as_ref()
        .filter(|(t, _)| t.elapsed() < Duration::from_secs(600))
    {
        let _ = time;
        return Ok(val.clone());
    }
    let r = client()
        .map_err(|_| err())?
        .get(endpoint("openrouter", "models"))
        .query(&[("output_modalities", "transcription"), ("zdr", "true")])
        .send()
        .await
        .map_err(|_| err())?;
    if !r.status().is_success() {
        return Err(err());
    }
    let v = response(r).await.ok_or_else(err)?;
    let rows = v["data"]
        .as_array()
        .ok_or_else(err)?
        .iter()
        .filter_map(|o| {
            let id = o["id"].as_str().filter(|s| model_id(s))?;
            let name = o["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            Some(json!({"id":id,"name":name}))
        })
        .collect::<Vec<_>>();
    let v = json!({"models":rows});
    *cache = Some((Instant::now(), v.clone()));
    Ok(v)
}
async fn catalog(State(app): State<Workbench>) -> Reply {
    Ok(Json(entries(&app.0.voice).await?))
}
#[derive(Deserialize)]
struct QueryModel {
    model: Option<String>,
}
async fn transcribe(
    State(app): State<Workbench>,
    Param(p): Param<String>,
    Query(q): Query<QueryModel>,
    body: Bytes,
) -> Reply {
    known(&p)?;
    let err = |why| CloudError(p.clone(), why);
    let (opts, pcm) = audio::split(&body).map_err(|_| err("invalid_audio"))?;
    let svc = &app.0.voice;
    let cred = svc
        .credential(&p)
        .map_err(|_| err("unreadable-key"))?
        .ok_or_else(|| err("missing-key"))?;
    let lang=if "ar bg ca cs da de el en es fa fi fr hi hr hu id it ja ko mk ms nl pl pt ro ru sk sv th tr uk ur vi zh".split_whitespace().any(|s|s==opts.speech_language){opts.speech_language.as_str()}else{"auto"};
    let client = client().map_err(|_| err("unavailable"))?;
    let wave = audio::wave(pcm);
    let req = if p == "openrouter" {
        let m = q
            .model
            .as_deref()
            .filter(|s| model_id(s))
            .ok_or_else(|| err("model-unavailable"))?;
        let list = entries(svc).await?;
        if !list["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == m)
        {
            return Err(err("model-unavailable"));
        }
        let mut payload = json!({"model":m,"input_audio":{"format":"wav","data":STANDARD.encode(wave)},"provider":{"zdr":true}});
        if lang != "auto" {
            payload["language"] = json!(lang);
        }
        client
            .post(endpoint(&p, "audio/transcriptions"))
            .json(&payload)
    } else {
        let (payload, ctype) = multipart(&p, &opts, lang, &wave);
        client
            .post(endpoint(
                &p,
                if p == "openai" {
                    "audio/transcriptions"
                } else {
                    "stt"
                },
            ))
            .header("content-type", ctype)
            .body(payload)
    };
    let key = svc
        .resolve(&p, &cred)
        .await
        .map_err(|_| err("unreadable-key"))?;
    let current = svc
        .credential(&p)
        .map_err(|_| err("unreadable-key"))?
        .ok_or_else(|| err("missing-key"))?;
    if cred != current {
        return Err(err("key-changed"));
    }
    let r = req
        .bearer_auth(key.as_str())
        .send()
        .await
        .map_err(|_| err("unavailable"))?;
    if !r.status().is_success() {
        return Err(err(match r.status().as_u16() {
            401..=403 => "rejected-key",
            429 => "rate-limit",
            404 if p == "openrouter" => "model-unavailable",
            _ => "unavailable",
        }));
    }
    let r = response(r).await.ok_or_else(|| err("unavailable"))?;
    Ok(Json(
        json!({"text":opts.corrected(r["text"].as_str().ok_or_else(||err("unavailable"))?,lang)}),
    ))
}
fn multipart(p: &str, o: &Options, lang: &str, wav: &[u8]) -> (Vec<u8>, String) {
    let boundary = format!("kar-{}", uuid::Uuid::new_v4().simple());
    let mut body = Vec::new();
    let mut field = |name: &str, val: &str| {
        write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n"
        )
        .expect("Vec write");
    };
    let xai = p == "xai";
    field(
        "model",
        if xai {
            "grok-voice-transcribe-2.0"
        } else {
            "gpt-transcribe"
        },
    );
    if lang != "auto" {
        field(if xai { "language" } else { "languages[]" }, lang);
    }
    for w in o.recognition_words() {
        if xai || !w.contains(['<', '>', '\r', '\n']) {
            field(if xai { "keyterm" } else { "keywords[]" }, w);
        }
    }
    if xai {
        if lang != "auto" {
            field("format", "true");
        }
        field(
            "filler_words",
            if !o.speech_remove_filler_words
                || o.speech_correction_word
                    .as_deref()
                    .is_some_and(|s| !s.is_empty())
            {
                "true"
            } else {
                "false"
            },
        );
        if !o.speech_silence_filtering {
            field("vad_threshold", "0");
        }
    }
    write!(body,"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"dictation.wav\"\r\nContent-Type: audio/wav\r\n\r\n").expect("Vec write");
    body.extend(wav);
    write!(body, "\r\n--{boundary}--\r\n").expect("Vec write");
    (body, format!("multipart/form-data; boundary={boundary}"))
}
