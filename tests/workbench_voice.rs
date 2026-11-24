//! Voice tests enter through a running Kar workbench; external inference is opt-in.
#[path = "support/workbench_server.rs"]
mod server;

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::Client;
use serde_json::{Value, json};
use std::{ffi::OsStr, path::Path};
use tokio::{net::TcpListener, process::Child};

struct Instance {
    _dir: tempfile::TempDir,
    child: Child,
    url: String,
    client: Client,
}

impl Instance {
    async fn start(env: &[(&str, &OsStr)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let state = dir.path().join("state");
        let native = dir.path().join("native");
        let (child, url) = if env.is_empty() {
            server::serve(&state, &root, &native).await
        } else {
            server::serve_with_env(&state, &root, &native, env).await
        };
        Self {
            _dir: dir,
            child,
            url: format!("{url}/api/voice"),
            client: server::client(),
        }
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{path}", self.url))
    }

    async fn key(&self, provider: &str, key: &str) {
        assert_eq!(
            self.post(&format!("/cloud/{provider}/key"))
                .json(&json!({"key":key}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
    }
}

fn frame(samples: &[i16]) -> Vec<u8> {
    let opts = json!({"speechLanguage":"en","speechTranslateToEnglish":false,
        "speechHints":["Kar"],"speechCustomWords":[],"speechRemoveFillerWords":false});
    let opts = serde_json::to_vec(&opts).unwrap();
    let mut b = (opts.len() as u32).to_le_bytes().to_vec();
    b.extend(opts);
    for &s in samples {
        let scale = if s < 0 { 32768.0 } else { 32767.0 };
        b.extend((s as f32 / scale).to_le_bytes());
    }
    b
}

async fn cloud_response(
    State(key): State<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    if headers["authorization"] != format!("Bearer {key}") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"rejected test key"})),
        );
    }
    let samples: Vec<u8> = [-1234_i16, 1234]
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    if headers["content-type"] == "application/json" {
        let object: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(object["model"], "lab/asr");
        assert_eq!(object["provider"]["zdr"], true);
        let wave = STANDARD
            .decode(object["input_audio"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(&wave[44..], samples);
    } else {
        let riff = body.windows(4).position(|s| s == b"RIFF").unwrap();
        assert_eq!(&body[riff + 44..riff + 48], samples);
        let fields = std::str::from_utf8(&body[..riff]).unwrap();
        assert!(fields.contains("filename=\"dictation.wav\""));
        assert!(fields.contains(if uri.path() == "/stt" {
            "grok-voice-transcribe-2.0"
        } else {
            "gpt-transcribe"
        }));
    }
    (
        StatusCode::OK,
        Json(json!({"text":"fixture provider response"})),
    )
}

#[tokio::test]
async fn cloud_protocol_and_errors_through_workbench() {
    let key = format!("voice-{}", uuid::Uuid::new_v4());
    let peer = Router::new()
        .route("/audio/transcriptions", post(cloud_response))
        .route("/stt", post(cloud_response))
        .route(
            "/models",
            get(|| async { Json(json!({"data":[{"id":"lab/asr","name":"Lab"}]})) }),
        )
        .with_state(key.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, peer).await.unwrap() });
    let ep = OsStr::new(&endpoint);
    let mut app = Instance::start(&[
        (
            "KAR_VOICE_ENGINE",
            OsStr::new("/missing/kar-voice-test-engine"),
        ),
        ("KAR_VOICE_OPENAI_BASE", ep),
        ("KAR_VOICE_XAI_BASE", ep),
        ("KAR_VOICE_OPENROUTER_BASE", ep),
    ])
    .await;
    let unauthenticated = Client::new()
        .get(format!("{}/status", app.url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let status = server::json(app.client.get(format!("{}/status", app.url))).await;
    assert_eq!(status["supported"], false);
    let body = frame(&[-1234, 1234]);
    for (route, data) in [("/transcribe", vec![0]), ("/transcribe", body.clone())] {
        let r = app.post(route).body(data).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(r.json::<Value>().await.unwrap()["error"].is_string());
    }
    for provider in ["openai", "xai", "openrouter"] {
        let route = format!("/cloud/{provider}/transcribe?model=lab/asr");
        let missing = app.post(&route).body(body.clone()).send().await.unwrap();
        assert_eq!(
            missing.json::<Value>().await.unwrap()["reason"],
            "missing-key"
        );
        app.key(provider, &key).await;
        let value = server::json(app.post(&route).body(body.clone())).await;
        assert_eq!(value["text"], "fixture provider response");
        app.key(provider, "revoked-test-key").await;
        let refused = app.post(&route).body(body.clone()).send().await.unwrap();
        assert_eq!(
            refused.json::<Value>().await.unwrap()["reason"],
            "rejected-key"
        );
    }
    app.child.kill().await.unwrap();
    server.abort();
}

fn audio() -> Vec<u8> {
    // Real fixture may have LIST/metadata chunks before data. Accept mono PCM16 at 16 kHz.
    let wave =
        std::fs::read(std::env::var_os("KAR_VOICE_TEST_WAV").expect("KAR_VOICE_TEST_WAV")).unwrap();
    assert_eq!(&wave[..4], b"RIFF");
    assert_eq!(&wave[8..12], b"WAVE");
    let mut bytes = &wave[12..];
    let mut format = false;
    while bytes.len() >= 8 {
        let n = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let chunk = &bytes[8..8 + n];
        if &bytes[..4] == b"fmt " {
            assert_eq!(&chunk[..8], &[1, 0, 1, 0, 0x80, 0x3e, 0, 0]);
            assert_eq!(&chunk[14..16], &[16, 0]);
            format = true;
        } else if &bytes[..4] == b"data" {
            assert!(format);
            let samples: Vec<i16> = chunk
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes(c.try_into().unwrap()))
                .collect();
            return frame(&samples);
        }
        bytes = &bytes[8 + n + n % 2..];
    }
    panic!("WAV has no PCM data")
}

fn check_transcript(response: Value) {
    let text = response["text"].as_str().expect("text");
    println!("transcript: {text}");
    let phrase = std::env::var("KAR_VOICE_TEST_EXPECT").expect("KAR_VOICE_TEST_EXPECT");
    assert!(!phrase.trim().is_empty() && text.to_lowercase().contains(&phrase.to_lowercase()));
}

#[tokio::test]
#[ignore = "needs a native transcribe-cli and a checksummed model, actual WAV and expected phrase"]
async fn installed_local_model_transcribes_audio() {
    let path = std::env::var_os("KAR_VOICE_TEST_MODEL").expect("KAR_VOICE_TEST_MODEL");
    let model = std::env::var("KAR_VOICE_TEST_MODEL_ID").expect("KAR_VOICE_TEST_MODEL_ID");
    let mut app = Instance::start(&[]).await;
    server::json(
        app.post("/models/link")
            .json(&json!({"path":Path::new(&path)})),
    )
    .await;
    server::json(app.post("/models/select").json(&json!({"modelId":model}))).await;
    assert_eq!(
        app.post("/models/prepare").send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    check_transcript(server::json(app.post("/transcribe").body(audio())).await);
    app.child.kill().await.unwrap();
}

#[tokio::test]
#[ignore = "sends the operator-selected audio to the configured vendor and may incur charges"]
async fn configured_cloud_transcribes_audio() {
    let provider = std::env::var("KAR_VOICE_TEST_PROVIDER").expect("KAR_VOICE_TEST_PROVIDER");
    assert!(matches!(provider.as_str(), "openai" | "xai" | "openrouter"));
    let key = std::env::var("KAR_VOICE_TEST_KEY").expect("KAR_VOICE_TEST_KEY");
    let mut app = Instance::start(&[]).await;
    app.key(&provider, &key).await;
    let mut send = app.post(&format!("/cloud/{provider}/transcribe"));
    if provider == "openrouter" {
        send = send.query(&[(
            "model",
            std::env::var("KAR_VOICE_TEST_MODEL_ID").expect("KAR_VOICE_TEST_MODEL_ID"),
        )]);
    }
    check_transcript(server::json(send.body(audio())).await);
    app.child.kill().await.unwrap();
}
