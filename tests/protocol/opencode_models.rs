use kar::forms::{ListingReason, ModelEntry, ModelListing};
use serde_json::{Value, json};
use std::{fs, io::Write, time::Duration};

pub(super) fn peer(case: &str, args: &[String]) {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        Some(cwd.as_os_str()),
        std::env::var_os("KAR_MODELS_EXPECT_CWD").as_deref()
    );
    assert_eq!(std::env::var("KAR_MODELS_ENV").unwrap(), "scoped");
    if args.first().is_some_and(|a| a == "serve") {
        if case == "v2_serve" {
            use std::io::{BufRead, BufReader};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            println!("opencode server listening on http://{addr}");
            let _ = std::io::stdout().flush();
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_err() || req_line.is_empty() {
                    continue;
                }
                loop {
                    let mut hdr = String::new();
                    if reader.read_line(&mut hdr).is_err() || hdr.trim().is_empty() {
                        break;
                    }
                }
                let body = if req_line.contains("/api/plugin") {
                    json!([{"id": "builtin"}]).to_string()
                } else if req_line.contains("/api/model") {
                    json!({"data": [{"id": "fast", "providerID": "project", "name": "Fast V2", "variants": {"default": {}, "high": {}, "low": {}}}]}).to_string()
                } else {
                    "{}".to_owned()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = (&stream).write_all(resp.as_bytes());
                if req_line.contains("/api/model") {
                    break;
                }
            }
            return;
        }
        std::process::exit(1);
    }
    let mut audit = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cwd.join("queries"))
        .unwrap();
    writeln!(audit, "{}", serde_json::to_string(args).unwrap()).unwrap();
    if args == ["--version"] {
        println!(
            "{}",
            if case == "v1" {
                "opencode v1.19.5"
            } else {
                "2.2.7"
            }
        );
        return;
    }
    assert_eq!(
        args,
        [
            "models",
            if case == "v1" {
                "--verbose"
            } else {
                "--standalone"
            }
        ]
    );
    match case {
        "v1" => println!(
            "project/fast\n{}",
            serde_json::to_string_pretty(
                &json!({"name":"Fast", "providerID":"project","variants":{"high":{},"low":{}}})
            )
            .unwrap()
        ),
        "v2" => println!("project/fast"),
        "empty" => (),
        "bad" => println!("daemon logging before the catalog"),
        "exit" => std::process::exit(7),
        "timeout" => std::thread::sleep(Duration::from_secs(10)),
        _ => unreachable!(),
    }
}

pub(super) async fn run() -> kar::Result<()> {
    let temp = tempfile::tempdir()?;
    let bin = std::env::current_exe()?;
    let fallback: ModelEntry = serde_json::from_value(json!({"id":"bundled"}))?;
    for case in ["v1", "v2", "v2_serve", "empty", "bad", "exit", "timeout"] {
        let work = temp.path().join(case);
        fs::create_dir(&work)?;
        let work = work.canonicalize()?;
        let mut rt = kar::runtime("opencode")?;
        rt.program = bin.clone();
        let mut opts = kar::SessionOptions::new(&work);
        opts.env
            .insert("KAR_TEST_OPENCODE_MODELS".into(), case.into());
        opts.env.insert("KAR_MODELS_ENV".into(), "scoped".into());
        opts.env.insert(
            "KAR_MODELS_EXPECT_CWD".into(),
            work.clone().into_os_string(),
        );
        if case == "timeout" {
            opts.handshake_timeout = Duration::from_secs(2);
        }
        let result = rt.form_model_listing(&opts, vec![fallback.clone()]).await;
        if case == "v2_serve" {
            let ModelListing::Live { models } = result else {
                panic!("expected live v2_serve listing: {result:?}");
            };
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "project/fast");
            assert_eq!(
                models[0].metadata.get("displayName"),
                Some(&json!("Fast V2"))
            );
            assert_eq!(
                models[0].effort_levels,
                Some(vec!["high".into(), "low".into()])
            );
            continue;
        }
        match case {
            "v1" | "v2" => {
                let ModelListing::Live { models } = result else {
                    panic!("expected live {case} listing: {result:?}");
                };
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].id, "project/fast");
                assert_eq!(models[0].effort_levels.is_some(), case == "v1");
                let cli = std::process::Command::new(env!("CARGO_BIN_EXE_kar"))
                    .args(["models", "opencode", "--bin"])
                    .arg(&bin)
                    .arg("--cwd")
                    .arg(&work)
                    .envs(&opts.env)
                    .output()?;
                assert!(
                    cli.status.success(),
                    "{}",
                    String::from_utf8_lossy(&cli.stderr)
                );
                assert_eq!(
                    serde_json::from_slice::<Vec<ModelEntry>>(&cli.stdout)?,
                    models
                );
            }
            _ => {
                let reason = match case {
                    "empty" => ListingReason::Unsupported,
                    "timeout" => ListingReason::Timeout,
                    _ => ListingReason::Failed,
                };
                assert!(
                    matches!(result,ModelListing::Fallback{reason:r,models,..} if r==reason && models==[fallback.clone()])
                );
            }
        }
        let queries: Vec<Value> = fs::read_to_string(work.join("queries"))?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        let expected = [
            json!(["--version"]),
            json!([
                "models",
                if case == "v1" {
                    "--verbose"
                } else {
                    "--standalone"
                }
            ]),
        ];
        let count = if matches!(case, "v1" | "v2") { 2 } else { 1 };
        assert_eq!(queries.len(), count * 2);
        for pair in queries.as_chunks::<2>().0 {
            assert_eq!(*pair, expected);
        }
    }
    Ok(())
}
