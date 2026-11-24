use axum::{
    Router,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use kar::chat::{ChatStore, InputStatus, Item, Role, TurnStatus};
use std::{path::Path, process::Output, time::Duration};
use uuid::Uuid;

#[path = "cursor_agent.rs"]
mod agent;

// Cursor's Connect unary responses are ordinary protobuf messages. Both model catalog
// methods carry ModelDetails in field 1, with "offline" as its model_id.
fn varint(mut value: usize, bytes: &mut Vec<u8>) {
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn fields(fields: &[(usize, &[u8])]) -> Vec<u8> {
    let mut message = Vec::new();
    for (number, bytes) in fields {
        varint((number << 3) | 2, &mut message);
        varint(bytes.len(), &mut message);
        message.extend_from_slice(bytes);
    }
    message
}

fn model_catalog() -> Vec<u8> {
    let parameter = |id: &[u8], label: &[u8], kind, options: &[&[u8]]| {
        let mut values = Vec::new();
        for value in options {
            values.extend(fields(&[(1, &fields(&[(1, value)]))]));
        }
        fields(&[(1, id), (2, label), (4, &fields(&[(kind, &values)]))])
    };
    let variant = |id: &[u8], label: &[u8], effort: &[u8], default: bool| {
        let mut variant = fields(&[
            (1, &fields(&[(1, b"thinking"), (2, b"true")])),
            (1, &fields(&[(1, b"reasoning_effort"), (2, effort)])),
            (2, label),
            (9, id),
        ]);
        if default {
            variant.extend([5 << 3, 1]);
        }
        variant
    };
    // AvailableModelsResponse.AvailableModel fields 29/30 carry definitions and variants.
    let model = fields(&[
        (1, b"offline"),
        (17, b"Local model"),
        (
            29,
            &parameter(b"thinking", b"Thinking", 1, &[b"false", b"true"]),
        ),
        (
            29,
            &parameter(
                b"reasoning_effort",
                b"Reasoning effort",
                2,
                &[b"low", b"high"],
            ),
        ),
        (30, &variant(b"offline-low", b"Local low", b"low", true)),
        (30, &variant(b"offline-high", b"Local high", b"high", false)),
    ]);
    fields(&[(2, &model)])
}

async fn metadata(uri: Uri) -> Response {
    if uri.path() == "/aiserver.v1.AiService/AvailableModels" {
        return (
            [(header::CONTENT_TYPE, "application/proto")],
            model_catalog(),
        )
            .into_response();
    }
    let body: &[u8] = match uri.path() {
        "/aiserver.v1.AiService/GetDefaultModelForCli"
        | "/aiserver.v1.AiService/GetUsableModels" => b"\x0a\x09\x0a\x07offline",
        "/aiserver.v1.ServerConfigService/GetServerConfig"
        | "/aiserver.v1.DashboardService/GetUserPrivacyMode"
        | "/aiserver.v1.DashboardService/GetManagedSkills"
        | "/aiserver.v1.DashboardService/GetTeamReposOrEmptyIfNotInTeam"
        | "/aiserver.v1.DashboardService/GetTeamAdminSettingsOrEmptyIfNotInTeam" => b"",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, "application/proto")], body).into_response()
}

fn command(dir: &Path, endpoint: &str, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.current_dir(dir)
        .env_clear()
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("APPDATA", dir)
        .env("LOCALAPPDATA", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("KAR_CHAT_DIR", dir.join("chats"))
        .env("CURSOR_API_KEY", "kar-loopback-test")
        .env("CURSOR_API_ENDPOINT", endpoint)
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(args)
        .kill_on_drop(true);
    for key in ["PATH", "SystemRoot", "TMPDIR", "TMP", "TEMP"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd
}

async fn cli(dir: &Path, endpoint: &str, args: &[&str]) -> Output {
    tokio::time::timeout(
        Duration::from_secs(25),
        command(dir, endpoint, args).output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
#[ignore = "requires Cursor CLI with ACP; uses a loopback metadata service and a local slash command"]
async fn actual_cursor_missing_session_preserves_history_and_queue() {
    let program = std::env::var("KAR_CURSOR_BIN").unwrap_or_else(|_| "cursor-agent".into());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let workspace = dir.join("work");
    std::fs::create_dir(&workspace).unwrap();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(socket, Router::new().fallback(metadata))
            .await
            .unwrap()
    });
    let slash = "/copy-request-id";
    let first = cli(
        dir,
        &endpoint,
        &[
            "chat", "new", "cursor", slash, "--cwd", "work", "--bin", &program,
        ],
    )
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let store = ChatStore::open(dir.join("chats")).unwrap();
    let info = store.list().unwrap().pop().unwrap();
    let request = Uuid::new_v4();
    store.enqueue(info.id, request, "still queued").unwrap();
    // A slash command can complete before Cursor creates its conversation database.
    // Exercise the same missing-store case on builds that create it eagerly.
    let db = dir
        .join("cursor/acp-sessions")
        .join(&info.native_id)
        .join("store.db");
    if db.is_file() {
        std::fs::rename(&db, dir.join("saved-test-store.db")).unwrap();
    }
    for _ in 0..2 {
        let out = cli(
            dir,
            &endpoint,
            &["chat", "resume", slash, "--chat", &info.id.to_string()],
        )
        .await;
        assert!(!out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            format!("kar: Session \"{}\" not found", info.native_id)
        );
    }
    let history = store.history(info.id).unwrap();
    assert_eq!(history.chat.native_id, info.native_id);
    assert_eq!(history.chat.workspace, workspace.canonicalize().unwrap());
    assert_eq!(history.turns.len(), 1);
    let turn = &history.turns[0];
    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(turn.items.iter().any(|item| matches!(&item.item, Item::Message{role:Role::Assistant,text,..} if text.contains("No request ID found"))));
    let queue = store.queued_inputs(info.id).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, request);
    assert_eq!(queue[0].status, InputStatus::Pending);
    assert_eq!(store.list().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
#[ignore = "requires Cursor CLI with ACP and a parameterized model picker"]
async fn actual_cursor_parameterized_models_and_legacy_variants() {
    let program = std::env::var("KAR_CURSOR_BIN").unwrap_or_else(|_| "cursor-agent".into());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(socket, Router::new().fallback(metadata))
            .await
            .unwrap()
    });
    let result = cli(dir, &endpoint, &["models", "cursor", "--bin", &program]).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let models: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let model = &models[0];
    assert_eq!(model["id"], "offline", "{models}");
    assert_eq!(model["displayName"], "Local model");
    assert_eq!(model["effortLevels"], serde_json::json!(["low", "high"]));
    assert_eq!(model["defaultEffort"], "low");
    assert!(
        !dir.join("cursor/acp-sessions").exists(),
        "catalog created a Cursor session"
    );

    for (selector, effort, expected_picker) in [
        ("offline", Some("high"), true),
        ("offline", None, true),
        ("offline-low", None, false),
    ] {
        let mut args = vec![
            "run",
            "cursor",
            "/copy-request-id",
            "--json",
            "--bin",
            &program,
            "--model",
            selector,
        ];
        if let Some(effort) = effort {
            args.extend(["--effort", effort]);
        }
        let out = cli(dir, &endpoint, &args).await;
        assert!(
            out.status.success(),
            "{}: {}",
            selector,
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: Vec<serde_json::Value> = out
            .stdout
            .split(|b| *b == b'\n')
            .filter(|line| line.first() == Some(&b'{'))
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert!(rows.iter().any(|r| r["native"]["method"] == "initialize"
            && r.pointer("/native/params/clientCapabilities/_meta/parameterizedModelPicker")
                == Some(&serde_json::Value::Bool(expected_picker))));
        assert!(
            rows.iter()
                .flat_map(|r| r["events"].as_array().unwrap())
                .any(|event| event["kind"] == "model" && event["model"] == selector)
        );
        if let Some(value) = effort {
            assert!(
                rows.iter()
                    .any(|r| r["native"]["method"] == "session/set_config_option"
                        && r["native"]["params"]["configId"] == "reasoning_effort"
                        && r["native"]["params"]["value"] == value)
            );
            assert!(
                rows.iter()
                    .flat_map(|r| r["events"].as_array().unwrap())
                    .any(|event| event["kind"] == "effort" && event["effort"] == value)
            );
        }
    }
    let invalid = cli(
        dir,
        &endpoint,
        &[
            "open", "cursor", "--model", "offline", "--effort", "invalid", "--bin", &program,
        ],
    )
    .await;
    assert!(!invalid.status.success());
    server.abort();
}
