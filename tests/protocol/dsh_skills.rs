use kar::{
    Result,
    inventory::{InventoryOptions, InventoryResult, UnavailableCode, UnsupportedCode},
};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, Write},
    time::Duration,
};

pub(super) fn peer(mode: &str, args: &[String]) {
    if args.iter().any(|s| s == "--help") {
        println!("Usage: dsh --profile");
        return;
    }
    if args.iter().any(|s| s == "--version") {
        println!("0.2.1-rc.5");
        return;
    }
    assert_eq!(&args[1..4], ["--profile", "acp", "--patch"]);
    assert!(
        env::var("NODE_OPTIONS")
            .unwrap()
            .contains("--experimental-addon-modules")
    );
    let patch: Value = serde_json::from_slice(&fs::read(&args[4]).unwrap()).unwrap();
    assert_eq!(patch[0], json!({"id":"acp","disabled":true}));
    let entry = &patch[1]["insert"][0];
    let addon = url::Url::parse(entry["name"].as_str().unwrap())
        .unwrap()
        .to_file_path()
        .unwrap();
    assert!(addon.is_file());
    assert_eq!(
        fs::canonicalize(entry["config"]["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(env::current_dir().unwrap()).unwrap()
    );
    println!("OAR-DSH-SKILLS loaded");
    let packet = match mode {
        "partial" => {
            json!({"complete":false,"skills":[{"name":"virtual","source":"runtime","invocation":{"modelInvocable":false,"userInvocable":false},"body":"private instructions"}]})
        }
        "bad" => json!({"skills":[]}),
        "missing" => json!({"error":"native_query_unavailable"}),
        "hang" => {
            io::stdout().flush().unwrap();
            fs::write("probe-pid", std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(120));
            return;
        }
        _ => unreachable!(),
    };
    println!("OAR-DSH-SKILLS {packet}");
}

pub(super) async fn run() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut opts = InventoryOptions::default();
    opts.installation.cwd = Some(temp.path().to_owned());
    opts.installation
        .env
        .insert("DSH_HOME".into(), Some(temp.path().join("home").into()));
    opts.installation.env.insert("NODE_OPTIONS".into(), None);
    opts.installation
        .env
        .insert("OAR_DSH_BIN".into(), Some(env::current_exe()?.into()));
    let rt = kar::runtime("dsh")?;
    for mode in ["partial", "bad", "missing", "hang"] {
        opts.installation
            .env
            .insert("OAR_TEST_DSH_SKILLS".into(), Some(mode.into()));
        opts.timeout = Some(if mode == "hang" {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(8)
        });
        let result = rt.skills(&opts).await?;
        match (mode, result) {
            (
                "partial",
                InventoryResult::Ok {
                    items,
                    partial: true,
                    ..
                },
            ) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].name, "virtual");
                assert!(items[0].path.is_none());
                assert_eq!(items[0].disable_model_invocation, Some(true));
                assert_eq!(items[0].user_invocable, Some(false));
                assert!(!serde_json::to_string(&items)?.contains("private instructions"));
            }
            (
                "bad",
                InventoryResult::Unavailable {
                    code: UnavailableCode::QueryFailed,
                    ..
                },
            )
            | (
                "missing",
                InventoryResult::Unsupported {
                    code: UnsupportedCode::NativeQueryUnavailable,
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
                let pid = fs::read_to_string(temp.path().join("probe-pid"))?;
                #[cfg(unix)]
                assert_eq!(
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.parse().unwrap()), None),
                    Err(nix::errno::Errno::ESRCH)
                );
                #[cfg(not(unix))]
                assert!(pid.parse::<u32>().is_ok());
            }
            (_, other) => panic!("unexpected {mode} result {other:?}"),
        }
    }
    Ok(())
}
