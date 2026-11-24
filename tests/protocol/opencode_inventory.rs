use super::*;
use axum::http::request::Parts;
use kar::inventory::InventoryOptions;

pub(super) fn version(case: &str) -> &'static str {
    match case {
        "skills_floor" => "2.2.6",
        "skills_legacy" | "skills_invalid_json" | "skills_exit" => "1.19.6",
        "skills_partial" => "2.2.7",
        "skills_rc" => "2.2.7-rc.1",
        "skills_custom" => "2.3.0+custom",
        _ => "2.3.2",
    }
}

pub(super) fn serve(case: &str, parts: &Parts) -> Response {
    let dir = std::env::current_dir().unwrap();
    let encoded = parts.headers["x-opencode-directory"].to_str().unwrap();
    let dir_header = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .unwrap();
    assert_eq!(
        fs::canonicalize(Path::new(dir_header.as_ref())).unwrap(),
        fs::canonicalize(&dir).unwrap()
    );
    assert_eq!(parts.method.as_str(), "GET");
    let data = match parts.uri.path() {
        "/api/provider" => {
            fs::write("activated", "").unwrap();
            json!([])
        }
        "/api/plugin" => {
            assert!(Path::new("activated").is_file());
            fs::write("plugins-read", "").unwrap();
            json!([{"state":{"status": if case == "skills_unknown_plugin" { "pending" } else { "failed" }}, "error":SECRET}])
        }
        "/api/skill" => {
            assert!(Path::new("plugins-read").is_file());
            json!([{"name":"builtin","path":"skill://report/SKILL.md","content":SECRET},{"name":"third party","path":"skill://plugin/readme","autoinvoke":false}])
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(json!({"data":data})).into_response()
}

pub(super) fn legacy(case: &str) {
    eprintln!(
        "Failed to load plugin {SECRET} https://localhost/private/#{SECRET} sk-abcdefghijklmnopqr. password=example-private-error-value"
    );
    match case {
        "skills_invalid_json" => print!("["),
        "skills_exit" => std::process::exit(7),
        "skills_legacy" => println!(
            "{}",
            json!([{"name":"file","location":"<built-in>", "content":"x".repeat(512*1024)}])
        ),
        _ => panic!("Unexpected OpenCode legacy catalog"),
    }
}

pub(super) async fn exercise() {
    for case in [
        "skills_floor",
        "skills_partial",
        "skills_unknown_plugin",
        "skills_unverified",
        "skills_rc",
        "skills_custom",
        "skills_legacy",
        "skills_invalid_json",
        "skills_exit",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path().join("工作区 space");
        fs::create_dir(&h).unwrap();
        let mut options = options(&h, case);
        options
            .installation
            .env
            .insert("FIXTURE_TOKEN".into(), Some(SECRET.into()));
        let result = kar::runtime("opencode")
            .unwrap()
            .skills(&InventoryOptions {
                installation: options.installation,
                timeout: Some(options.timeout),
            })
            .await
            .unwrap();
        let value = serde_json::to_value(result).unwrap();
        assert!(!value.to_string().contains(SECRET));
        match case {
            "skills_floor" => {
                assert_eq!(value["code"], "version_unsupported");
                assert!(!h.join("address").exists());
            }
            "skills_unknown_plugin" => assert_eq!(value["code"], "query_failed"),
            "skills_partial" | "skills_unverified" | "skills_rc" | "skills_custom" => {
                assert_eq!(value["partial"], true, "{value}");
                assert_eq!(
                    value["items"][0].get("scope").is_some(),
                    case == "skills_partial"
                );
                assert!(value["items"][1].get("scope").is_none());
                assert_eq!(value["items"][1]["disableModelInvocation"], true);
            }
            "skills_legacy" => {
                assert_eq!(value["items"][0]["scope"], "builtin");
                assert!(value.to_string().len() < 2000);
            }
            "skills_invalid_json" | "skills_exit" => {
                assert_eq!(value["code"], "query_failed");
                let reason = value["reason"].as_str().unwrap();
                assert!(reason.contains("Failed to load plugin"));
                for private in [
                    "localhost/private",
                    "sk-abcdefghijklmnopqr",
                    "example-private-error-value",
                ] {
                    assert!(!reason.contains(private));
                }
                assert!(
                    value["reason"]
                        .as_str()
                        .unwrap()
                        .contains(if case == "skills_exit" {
                            "exit 7"
                        } else {
                            "1 output bytes"
                        })
                );
            }
            _ => unreachable!(),
        }
        if h.join("address").is_file() {
            let addr: SocketAddr = fs::read_to_string(h.join("address"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(tokio::net::TcpStream::connect(addr).await.is_err());
        }
    }
}
