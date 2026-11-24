//! Actual vendor catalogs in isolated workspaces and homes, without model prompts.
use serde_json::Value;
use std::{fs, path::Path, process::Command};
fn check(id: &str, skill_dir: &str) {
    let binary = std::env::var_os(format!("OAR_{}_BIN", id.to_ascii_uppercase())).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let profile = home.join("config");
    fs::create_dir_all(&profile).unwrap();
    let cwd = tmp.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let skill = if id == "grok" {
        home.join(".agents")
    } else {
        cwd.join(skill_dir)
    }
    .join("skills/inventory-fixture");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"),"---\nname: inventory-fixture\ndescription: Local inventory fixture\n---\nPrivate instruction body should stay in the skill file.\n").unwrap();
    if id == "codex" {
        let dotgit = cwd.join(".git");
        fs::create_dir_all(&dotgit).unwrap();
        fs::write(
            profile.join("config.toml"),
            format!(
                "[projects.{}]\ntrust_level = \"trusted\"\n",
                serde_json::to_string(cwd.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("XDG_CACHE_HOME", &home)
        .env("CODEX_HOME", &profile)
        .env("KIMI_CODE_HOME", &profile)
        .env("QWEN_HOME", &profile)
        .env("GROK_HOME", &profile)
        .env("CLAUDE_CONFIG_DIR", &profile)
        .env("DISABLE_AUTOUPDATER", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("FACTORY_HOME_OVERRIDE", &home)
        .env("FACTORY_DISABLE_KEYRING", "1")
        .env("TMPDIR", tmp.path())
        .env("TEMP", tmp.path())
        .env("QWEN_CODE_SYSTEM_SETTINGS_PATH", home.join("system.json"))
        .env("NO_COLOR", "1");
    if id == "opencode" {
        cmd.env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("XDG_STATE_HOME", home.join("state"));
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            cmd.env(key, "http://127.0.0.1:1");
        }
    }
    if let Some(path) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", Path::new(&path));
    }
    let expected = if id == "devin" {
        let mut native = Command::new(&binary);
        native.env_clear().current_dir(&cwd);
        for (k, v) in cmd.get_envs() {
            if let Some(value) = v {
                native.env(k, value);
            }
        }
        let output = native.args(["skills", "list", "--json"]).output().unwrap();
        assert!(output.status.success());
        let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(rows.iter().any(|r| r["provider"] == "Builtin"));
        Some(rows)
    } else {
        None
    };
    let out = cmd
        .args(["skills", id, "--bin"])
        .arg(binary)
        .arg("--cwd")
        .arg(&cwd)
        .args(["--timeout-ms", "25000"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["kind"], "ok", "{v}");
    if let Some(native) = expected {
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), native.len());
        for row in native {
            let item = items.iter().find(|r| r["name"] == row["name"]).unwrap();
            assert_eq!(item["source"], row["provider"]);
            if row["provider"] == "Builtin" {
                assert_eq!(item["scope"], "builtin");
            }
            assert!(item.get("body").is_none());
        }
        return;
    }
    let item = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "inventory-fixture")
        .unwrap_or_else(|| panic!("{id} did not return the workspace skill, metadata: {v}"));
    assert_eq!(
        item["scope"],
        if id == "grok" { "user" } else { "project" },
        "{item}"
    );
    if id == "claude" {
        assert_eq!(item["userInvocable"], true, "{item}");
        assert_eq!(item["description"], "Local inventory fixture");
    }
    assert_eq!(v["scope"]["kind"], "workspace");
    assert!(
        !String::from_utf8(out.stdout)
            .unwrap()
            .contains("Private instruction body")
    );
}
#[test]
#[ignore = "requires OAR_KIMI_BIN"]
fn kimi() {
    check("kimi", ".agents");
}
#[test]
#[ignore = "requires OAR_CODEX_BIN"]
fn codex() {
    check("codex", ".agents");
}
#[test]
#[ignore = "requires OAR_FACTORY_BIN"]
fn factory() {
    check("factory", ".factory");
}
#[test]
#[ignore = "requires OAR_QWEN_BIN"]
fn qwen() {
    check("qwen", ".qwen");
}

#[test]
#[ignore = "requires OAR_DEVIN_BIN"]
fn devin() {
    check("devin", ".agents");
}

#[test]
#[ignore = "requires OAR_CODEX_BIN; isolated profile and a local Rust stdio MCP server"]
fn codex_mcp_and_tools() {
    native_mcp("codex");
}
#[test]
#[ignore = "requires OAR_CLAUDE_BIN"]
fn claude_commands_mcp_and_tools() {
    native_mcp("claude");
}
#[test]
#[ignore = "requires OAR_GROK_BIN"]
fn grok_mcp() {
    native_mcp("grok");
}
fn native_mcp(id: &str) {
    let binary = std::env::var_os(format!("OAR_{}_BIN", id.to_ascii_uppercase())).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let config = tmp.path().join("codex");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&config).unwrap();
    let kar_bin = env!("CARGO_BIN_EXE_kar");
    match id {
        "claude" => {
            fs::write(config.join(".claude.json"),serde_json::json!({"hasCompletedOnboarding":true,"mcpServers":{"fixture":{"type":"stdio","command":kar_bin,"args":["mcp","--runtimes","codex"]}}}).to_string()).unwrap();
            let skill = workspace.join(".claude/skills/test-command");
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("SKILL.md"),"---\nname: test-command\ndescription: Run a local fixture task\nargument-hint: FILE\n---\nTest command.").unwrap();
        }
        "grok" | "codex" => {
            fs::write(
        config.join("config.toml"),
        format!(
            "[mcp_servers.fixture]\ncommand = {}\nargs = [\"mcp\",\"--runtimes\",\"codex\"]\n",
            serde_json::to_string(kar_bin).unwrap()
        ),
    )
    .unwrap();
        }
        _ => unreachable!(),
    }
    let call = |name: &str| {
        let mut cmd = Command::new(kar_bin);
        cmd.env_clear()
            .env("HOME", tmp.path())
            .env("USERPROFILE", tmp.path())
            .env("CODEX_HOME", &config)
            .env("GROK_HOME", &config)
            .env("CLAUDE_CONFIG_DIR", &config)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("DISABLE_AUTOUPDATER", "1")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        if let Some(v) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", v);
        }
        let o = cmd
            .args([name, id, "--bin"])
            .arg(&binary)
            .arg("--cwd")
            .arg(&workspace)
            .args(["--timeout-ms", "20000"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["kind"], "ok", "{v}");
        v
    };
    let servers = call("mcp-servers");
    let source = servers["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "fixture")
        .unwrap_or_else(|| panic!("{servers}"));
    if id == "grok" {
        assert_eq!(source["source"], "configToml", "{source}");
        assert!(source.get("toolCount").is_none());
        return;
    }
    if id == "claude" {
        let cmds = call("commands");
        let command = cmds["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "test-command")
            .unwrap();
        assert_eq!(command["argumentHint"], "FILE");
    }
    let tools = call("tools");
    assert_eq!(tools["view"], "mcp-only");
    let items: Vec<_> = tools["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["mcpServerId"] == "fixture")
        .collect();
    assert_eq!(source["toolCount"], items.len());
    let run = items
        .iter()
        .find(|r| r["name"] == "run")
        .unwrap_or_else(|| panic!("{tools}"));
    if id == "codex" {
        assert_eq!(run["inputSchema"]["properties"]["task"]["type"], "string");
    }
    assert_eq!(run["source"], "mcp");
    assert!(
        !config.join("sessions").exists()
            || fs::read_dir(config.join("sessions"))
                .unwrap()
                .next()
                .is_none()
    );
}

#[test]
#[ignore = "requires OAR_GROK_BIN"]
fn grok() {
    check("grok", ".grok");
}
#[test]
#[ignore = "requires OAR_CLAUDE_BIN"]
fn claude() {
    check("claude", ".claude");
}

#[test]
#[ignore = "requires OAR_OPENCODE_BIN (OpenCode 1 or 2)"]
fn opencode() {
    check("opencode", ".opencode");
}

#[path = "support/native_skill_catalog.rs"]
mod native_skill_catalog;

#[path = "support/cursor_skill_catalog.rs"]
mod cursor_skill_catalog;

#[test]
#[ignore = "requires OAR_DSH_BIN and its Node host with native addon support"]
fn isolated_dsh_global_skills_without_a_session() {
    let bin = std::env::var_os("OAR_DSH_BIN").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("workspace");
    let home = temp.path().join("home");
    let agents = home.join("agents");
    let builtin = home.join("bundled");
    fs::create_dir_all(cwd.join(".git")).unwrap();
    let roots = [
        ("project-dsh", cwd.join(".dsh/skills"), "project"),
        ("project-agents", cwd.join(".agents/skills"), "project"),
        ("user-dsh", home.join("skills"), "user"),
        ("user-agents", agents.join("skills"), "user"),
        ("bundled", builtin.clone(), "builtin"),
    ];
    for (source, dir, _) in &roots {
        let skill = dir.join(format!("fixture-{source}"));
        fs::create_dir_all(&skill).unwrap();
        let user = *source != "project-agents";
        fs::write(skill.join("SKILL.md"),format!("---\nname: fixture-{source}\ndescription: local {source}\ndisable-model-invocation: true\nuser-invocable: {user}\n---\nPRIVATE SKILL BODY\n")).unwrap();
    }
    let patch = home.join("cordis.patch.yml");
    let config =
        b"- id: deepseek-account\n  disabled: true\n- id: session-log-deepseek\n  disabled: true\n";
    fs::write(&patch, config).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.env_clear()
        .current_dir(&cwd)
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("DSH_HOME", &home)
        .env("DSH_AGENTS_HOME", &agents)
        .env("DSH_BUNDLED_SKILL_DIR", &builtin);
    if let Some(root) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", root);
    }
    let out = cmd
        .args(["skills", "dsh", "--bin"])
        .arg(&bin)
        .arg("--cwd")
        .arg(&cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["kind"], "ok", "{result}");
    assert_eq!(result["view"], "discovered");
    assert_eq!(result["partial"], false);
    let items = result["items"].as_array().unwrap();
    for (source, dir, scope) in &roots {
        let name = format!("fixture-{source}");
        let row = items
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {result}"));
        assert_eq!(row["source"], *source);
        assert_eq!(row["scope"], *scope);
        assert_eq!(row["userInvocable"], *source != "project-agents");
        assert_eq!(row["disableModelInvocation"], true);
        assert_eq!(
            fs::canonicalize(row["path"].as_str().unwrap()).unwrap(),
            fs::canonicalize(dir.join(name).join("SKILL.md")).unwrap()
        );
    }
    assert!(!result.to_string().contains("PRIVATE SKILL BODY"));
    assert_eq!(fs::read(&patch).unwrap(), config);
    assert!(
        !ignore::WalkBuilder::new(&home)
            .hidden(false)
            .build()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|x| x == "jsonl"))
    );
}
