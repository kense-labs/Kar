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
