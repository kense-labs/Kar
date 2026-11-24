use std::process::{Command, Output};
fn kar(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kar"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn release_line_planning_requires_an_explicit_supported_choice() {
    let unspecified = kar(&["install", "opencode", "--plan"]);
    assert!(!unspecified.status.success());
    let why = String::from_utf8(unspecified.stderr).unwrap();
    assert!(why.contains("v1") && why.contains("v2"));
    for (line, url, package) in [
        ("v1", "https://opencode.ai/install", "opencode-ai"),
        ("v2", "https://opencode.ai/v2/install", "@opencode/cli"),
    ] {
        let plan = kar(&["install", "opencode", "--plan", "--line", line]);
        assert!(plan.status.success());
        let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
        if cfg!(windows) {
            assert_eq!(plan["package"], package);
        } else {
            assert_eq!(plan["url"], url);
        }
    }
    assert!(
        !kar(&["install", "opencode", "--plan", "--line", "v3"])
            .status
            .success()
    );
    assert!(
        !kar(&["install", "claude", "--plan", "--line", "v1"])
            .status
            .success()
    );
    assert!(
        !kar(&[
            "install",
            "opencode",
            "--plan",
            "--line",
            "v1",
            "--timeout-ms",
            "0"
        ])
        .status
        .success()
    );
}

fn installation(runtime: &str) -> serde_json::Value {
    let result = kar(&["list", runtime]);
    assert!(result.status.success());
    let row: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        row["installation"]["kind"], "available",
        "selected native installation"
    );
    row["installation"].clone()
}

fn native_check(runtime: &str) {
    let before = installation(runtime);
    let result = kar(&["update", runtime, "--check", "--timeout-ms", "30000"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let row: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let check = &row["check"];
    assert_eq!(row["id"], runtime);
    assert_eq!(
        check["kind"], "ok",
        "requires a vendor-managed copy and access to the native release source: {check}"
    );
    assert!(
        before["version"]
            .as_str()
            .unwrap()
            .contains(check["installed"].as_str().unwrap())
    );
    assert!(semver::Version::parse(check["latest"].as_str().unwrap()).is_ok());
    assert!(check["update_available"].is_boolean());
    assert!(check["source"].as_str().is_some_and(|s| !s.is_empty()));
    let after = installation(runtime);
    assert_eq!(after["command"], before["command"]);
    assert_eq!(after["version"], before["version"]);
}

macro_rules! live_update_check {
    ($runtime:ident) => {
        #[test]
        #[ignore = "requires a vendor-managed installation and access to the vendor's release source; read-only"]
        fn $runtime() {
            native_check(stringify!($runtime));
        }
    };
}
live_update_check!(antigravity);
live_update_check!(claude);
live_update_check!(codex);
live_update_check!(devin);
live_update_check!(factory);
live_update_check!(grok);
live_update_check!(kimi);

#[test]
fn account_batch_keeps_each_provider_identity_and_requires_a_target_for_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("absent-provider");
    for subcommand in ["status", "usage"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kar"));
        command
            .args(["account", subcommand, "--json", "--timeout-ms", "3000"])
            .env("HOME", tmp.path())
            .current_dir(tmp.path());
        for rt in kar::runtimes()
            .into_iter()
            .chain([kar::runtime("morph").unwrap()])
        {
            command.env(format!("KAR_{}_BIN", rt.id.to_ascii_uppercase()), &missing);
        }
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: Vec<serde_json::Value> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let ids: std::collections::BTreeSet<_> =
            rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), rows.len());
        for name in ["codex", "claude", "grok", "pi"] {
            let row = rows.iter().find(|r| r["id"] == name).unwrap();
            assert_eq!(row["installation"]["kind"], "missing");
        }
    }
    for operation in ["login", "logout"] {
        assert!(!kar(&["account", operation]).status.success());
    }
    for operation in ["login", "logout", "status"] {
        assert!(
            !kar(&["account", "codex", operation, "--timeout-ms", "0"])
                .status
                .success()
        );
    }
}

#[test]
fn inherited_mcp_depth_refuses_a_child_before_opening_its_runtime() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    for (depth, inherited, requested, limit) in [(0, 3, 0, 0), (1, 1, 4, 1), (2, 4, 2, 2)] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kar"))
            .args([
                "mcp",
                "--runtimes",
                "codex",
                "--max-depth",
                &requested.to_string(),
            ])
            .env("KAR_AGENT_DEPTH", depth.to_string())
            .env("KAR_AGENT_MAX_DEPTH", inherited.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        for (id,(method,params)) in [
            ("initialize",serde_json::json!({"protocolVersion":"2025-11-25"})),
            ("tools/call",serde_json::json!({"name":"run","arguments":{"runtime":"codex","task":"must not run"}})),
        ].into_iter().enumerate() {
            writeln!(input,"{}",serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).unwrap();
            input.flush().unwrap();
            let reply:serde_json::Value=serde_json::from_str(&output.next().unwrap().unwrap()).unwrap();
            if id==1 {
                assert_eq!(reply["result"]["isError"],true);
                assert_eq!(reply["result"]["content"][0]["text"],format!("subagents may nest at most {limit} deep"));
            }
        }
        drop(input);
        assert!(child.wait().unwrap().success());
    }
}
