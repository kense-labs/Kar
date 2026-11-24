use kar::{
    Result,
    inventory::{InventoryOptions, InventoryResult, UnavailableCode, UnsupportedCode, View},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, Write},
    time::Duration,
};

pub(super) fn peer(mode: &str, args: &[String]) {
    if args.iter().any(|a| a == "--version") {
        println!("1.6.0");
        return;
    }
    assert_eq!(args[1..5], ["--mode", "rpc", "--no-session", "--extension"]);
    assert!(std::path::Path::new(&args[5]).is_file());
    assert!(
        std::env::var("PI_CODING_AGENT_DIR")
            .unwrap()
            .ends_with("profile")
    );
    if mode == "loader" {
        eprintln!("Unknown file extension for pi-tools.node");
        return;
    }
    let mut input = io::stdin().lock().lines();
    let read = |lines: &mut std::io::Lines<std::io::StdinLock<'_>>| -> Value {
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
    };
    let req = read(&mut input);
    assert_eq!(req["type"], "get_state");
    if mode == "late_loader" {
        println!(
            "{}",
            json!({"id":req["id"],"type":"response","command":"get_state","success":true})
        );
        io::stdout().flush().unwrap();
        assert!(input.next().is_none());
        eprintln!("Unknown file extension for pi-tools.node");
        return;
    }
    if mode == "stderr_limit" {
        let _ = io::stderr().lock().write_all(&vec![b'x'; 1024 * 1024 + 1]);
        return;
    }
    println!("KAR-PI-TOOLS ready");
    println!(
        "{}",
        json!({"type":"extension_ui_request","id":"registry-dialog","method":"select"})
    );
    io::stdout().flush().unwrap();
    assert_eq!(
        read(&mut input),
        json!({"type":"extension_ui_response","id":"registry-dialog","cancelled":true})
    );
    println!(
        "{}",
        json!({"id":req["id"],"type":"response","command":"get_state","success":true,"data":{}})
    );
    io::stdout().flush().unwrap();
    assert!(input.next().is_none());
    if mode == "hang" {
        fs::write("reader-pid", std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    let data = match mode {
        "mixed" => {
            json!({"tools":[{"name":"inactive","description":"inactive","parameters":{"type":"object"},"sourceInfo":{"private":"private details"}},{"name":"added_on_start","description":"active"}],"active":["added_on_start"]})
        }
        "empty" => json!({"tools":[],"active":[]}),
        "bad_active" => json!({"tools":[],"active":[55]}),
        "bad_tool" => json!({"tools":[{"description":"no name"}],"active":[]}),
        "missing_list" => json!({"active":[]}),
        _ => unreachable!(),
    };
    println!("KAR-PI-TOOLS {data}");
}
pub(super) async fn run() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let rt = kar::runtime("pi")?;
    let mut opts = InventoryOptions::default();
    opts.installation.cwd = Some(temp.path().into());
    opts.installation
        .env
        .insert("KAR_PI_BIN".into(), Some(std::env::current_exe()?.into()));
    opts.installation
        .env
        .insert("KAR_PI_AGENT_DIR".into(), Some("profile".into()));
    opts.installation.env.insert("NODE_OPTIONS".into(), None);
    opts.installation.env.insert("KAR_PI_SDK_DIR".into(), None);
    for mode in [
        "mixed",
        "empty",
        "bad_active",
        "bad_tool",
        "missing_list",
        "loader",
        "late_loader",
        "stderr_limit",
        "hang",
    ] {
        opts.installation
            .env
            .insert("KAR_TEST_PI_REGISTRY".into(), Some(mode.into()));
        opts.timeout = Some(Duration::from_secs(if mode == "hang" { 2 } else { 6 }));
        let result = rt.tools(&opts).await?;
        match (mode, result) {
            (
                "mixed",
                InventoryResult::Ok {
                    view: View::Registered,
                    items,
                    partial: false,
                    ..
                },
            ) => {
                assert_eq!(
                    serde_json::to_value(items)?,
                    json!([
                        {"name":"inactive","description":"inactive","active":false,"inputSchema":{"type":"object"}},
                        {"name":"added_on_start","description":"active","active":true}
                    ])
                );
            }
            (
                "empty",
                InventoryResult::Ok {
                    items,
                    partial: false,
                    ..
                },
            ) => assert!(items.is_empty()),
            (
                "loader" | "late_loader",
                InventoryResult::Unsupported {
                    code: UnsupportedCode::NativeQueryUnavailable,
                    ..
                },
            )
            | (
                "bad_active" | "bad_tool" | "missing_list" | "stderr_limit",
                InventoryResult::Unavailable {
                    code: UnavailableCode::QueryFailed,
                    ..
                },
            ) => {}
            (
                "hang",
                InventoryResult::Unavailable {
                    code: UnavailableCode::Timeout,
                    ..
                },
            ) => {
                let pid = fs::read_to_string(temp.path().join("reader-pid"))?;
                #[cfg(unix)]
                assert_eq!(
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.parse().unwrap()), None),
                    Err(nix::errno::Errno::ESRCH)
                );
                #[cfg(not(unix))]
                assert!(pid.parse::<u32>().is_ok());
            }
            (_, result) => panic!("unexpected {mode} response {result:?}"),
        }
    }
    // A bad companion never runs either CLI's extensions.
    let root = temp.path().join("sdk");
    fs::create_dir_all(root.join("dist/core"))?;
    for path in ["dist/cli.js", "dist/index.js", "dist/core/agent-session.js"] {
        fs::write(root.join(path), "")?;
    }
    opts.installation
        .env
        .insert("KAR_PI_SDK_DIR".into(), Some(root.clone().into()));
    let package = root.join("package.json");
    for (name, version, expected) in [
        (
            "unrelated",
            "1.6.0",
            UnsupportedCode::InstallationUnsupported,
        ),
        (
            "@earendil-works/pi-coding-agent",
            "garbled",
            UnsupportedCode::InstallationUnsupported,
        ),
        (
            "@earendil-works/pi-coding-agent",
            "1.6.0-beta.1",
            UnsupportedCode::VersionUnsupported,
        ),
    ] {
        fs::write(
            &package,
            json!({"name":name,"version":version,"bin":{"pi":"dist/cli.js"}}).to_string(),
        )?;
        let result = rt.tools(&opts).await?;
        let InventoryResult::Unsupported {
            code,
            installed_version,
            required_version,
            ..
        } = result
        else {
            panic!("unexpected companion result {result:?}")
        };
        assert_eq!(code, expected);
        if expected == UnsupportedCode::VersionUnsupported {
            assert_eq!(installed_version.as_deref(), Some(version));
            assert_eq!(required_version.as_deref(), Some("1.6.0"));
        }
    }
    fs::write(
        package,
        json!({"name":"@earendil-works/pi-coding-agent","version":"1.6.0","bin":{"pi":"dist/cli.js"}}).to_string(),
    )?;
    opts.installation
        .env
        .insert("NODE_OPTIONS".into(), Some("--no-addons".into()));
    assert!(matches!(
        rt.tools(&opts).await?,
        InventoryResult::Unsupported {
            code: UnsupportedCode::NativeQueryUnavailable,
            ..
        }
    ));
    Ok(())
}
