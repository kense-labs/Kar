use super::native_skill_catalog::{process, write};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn fixture(home: &Path, cwd: &Path) {
    let skill = |dir: &Path, yaml: &str| {
        write(
            &dir.join("SKILL.md"),
            &format!("---\n{yaml}\n---\nPRIVATE SKILL BODY\n"),
        )
    };
    for (scope, root) in [("user", home), ("project", cwd)] {
        for brand in ["cursor", "codex", "grok", "agents", "claude"] {
            skill(
                &root.join(format!(".{brand}/skills/{scope}-{brand}")),
                &format!("description: {scope}-{brand}"),
            );
        }
    }
    for (dir, h) in [
        (".cursor/rules/inside-rules", "description: rules"),
        (".cursor/skills/shared", "description: first"),
        (".claude/skills/shared", "description: second"),
        (".cursor/skills/invalid", "name: [bad"),
        (".claude/skills/invalid", "description: excluded duplicate"),
        (
            ".cursor/skills/surface",
            "description: not for the sdk\nmetadata: {surfaces: 'ide, cli'}",
        ),
        (
            ".agents/skills/surface",
            "description: included in sdk\nmetadata: {surfaces: [ide, sdk]}",
        ),
        (
            ".cursor/skills/rule",
            "description: always\nalwaysApply: true",
        ),
        (".cursor/skills/disabled", "disable-model-invocation: true"),
        (
            ".cursor/skills/other",
            "name: yaml-name\ndescription: real name is the directory name",
        ),
        (".cursor/skills/.hidden", "description: hidden"),
        (
            ".codex/skills/openai-docs",
            "description: SDK excludes this name",
        ),
        ("nested/.cursor/skills/nested", "description: nested"),
        ("skip/.agents/skills/ignored", "description: ignored"),
        (
            "node_modules/tool/.agents/skills/excluded",
            "description: excluded",
        ),
        (
            ".hidden/.agents/skills/hidden-parent",
            "description: scanned by ripgrep",
        ),
    ] {
        skill(&cwd.join(dir), h);
    }
    skill(
        &cwd.join(".cursor/skills/long"),
        &format!("description: \"{}😀end\"", "a".repeat(1535)),
    );
    skill(
        &home.join(".cursor/skills-cursor/managed"),
        "description: excluded managed skill",
    );
    write(&cwd.join(".gitignore"), "skip/\n");
    #[cfg(unix)]
    {
        let outside = cwd.parent().unwrap().join("outside");
        skill(&outside, "description: external");
        std::os::unix::fs::symlink(outside, cwd.join(".agents/skills/external")).unwrap();
        std::os::unix::fs::symlink(
            home.join(".cursor/skills-cursor/managed"),
            cwd.join(".agents/skills/managed"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            cwd.join(".cursor/skills/project-cursor"),
            cwd.join(".agents/skills/alias"),
        )
        .unwrap();
    }
}

fn read(cwd: &Path, home: &Path, rg: Option<&Path>) -> Value {
    let mut c = process(Path::new(env!("CARGO_BIN_EXE_kar")), home, cwd);
    if let Some(rg) = rg {
        c.env("CURSOR_RIPGREP_PATH", rg);
    }
    let out = c
        .args(["skills", "cursor", "--cwd"])
        .arg(cwd)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn cursor_files_and_failed_nested_scanner() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("project");
    fixture(&home, &cwd);
    let result = read(&cwd, &home, None);
    assert_eq!(result["kind"], "ok");
    assert_eq!(result["partial"], true);
    let items = result["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .take(5)
            .all(|r| r["scope"] == "user" && r["source"] == "user")
    );
    assert!(items[5..].iter().all(|r| r["scope"] == "project"));
    for excluded in [
        "invalid",
        "rule",
        "nested",
        "external",
        "managed",
        "openai-docs",
        "alias",
        "excluded",
    ] {
        assert!(!items.iter().any(|r| r["name"] == excluded), "{excluded}");
    }
    assert!(items.iter().any(|r| r["name"] == "disabled"
        && r["disableModelInvocation"] == true
        && r["description"] == ""));
    assert!(
        items
            .iter()
            .any(|r| r["name"] == "surface" && r["description"] == "included in sdk")
    );
    assert!(
        items
            .iter()
            .any(|r| r["name"] == "shared" && r["description"] == "first")
    );
    assert!(
        items
            .iter()
            .any(|r| r["name"] == "long" && r["description"].as_str().unwrap().len() == 1535)
    );
    assert!(!result.to_string().contains("PRIVATE SKILL BODY"));
    fs::remove_dir_all(cwd.join(".cursor/skills/invalid")).unwrap();
    assert_eq!(read(&cwd, &home, None)["partial"], false);
    gix::init(&cwd).unwrap();
    let missing = read(&cwd, &home, Some(&cwd.join("absent")));
    assert_eq!(missing["partial"], true);
    assert!(
        missing["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "project-cursor")
    );
}
#[test]
#[ignore = "uses the upstream Cursor SDK observer, Node, tsx and the SDK's ripgrep"]
fn cursor_sdk_matches_native_discovery() {
    let node = PathBuf::from(std::env::var_os("KAR_CURSOR_OBSERVER_NODE").unwrap());
    let tsx = std::env::var_os("KAR_CURSOR_OBSERVER_TSX").unwrap();
    let observer = std::env::var_os("KAR_CURSOR_SDK_OBSERVER").unwrap();
    let rg = PathBuf::from(std::env::var_os("CURSOR_RIPGREP_PATH").unwrap());
    for git in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("home");
        let cwd = base.join("project");
        let copy = base.join("observer");
        let report = base.join("result.json");
        fs::create_dir(&copy).unwrap();
        fixture(&home, &cwd);
        if git {
            gix::init(&cwd).unwrap();
        }
        let out = process(&node, &home, &cwd)
            .args(["--preserve-symlinks", "--import"])
            .arg(&tsx)
            .arg(&observer)
            .arg(&cwd)
            .arg(&copy)
            .arg(&report)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let report: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(report["version"], "1.4.0");
        let result = read(&cwd, &home, Some(&rg));
        let rows = report["skills"].as_array().unwrap();
        let comparable = |row: &Value| {
            json!([
                fs::canonicalize(row["path"].as_str().unwrap()).unwrap(),
                row["description"],
                row["disableModelInvocation"]
            ])
            .to_string()
        };
        let mut expected = rows
            .iter()
            .filter(|r| r["invalid"] == false)
            .map(comparable)
            .collect::<Vec<_>>();
        let mut actual = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(comparable)
            .collect::<Vec<_>>();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected, "git={git}");
        assert_eq!(result["partial"], rows.iter().any(|r| r["invalid"] == true));
    }
}
