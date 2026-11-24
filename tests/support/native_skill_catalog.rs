use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

pub(super) fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
pub(super) fn process(bin: &Path, home: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(bin);
    c.current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .env("PI_OFFLINE", "1");
    if let Some(p) = std::env::var_os("SystemRoot") {
        c.env("SystemRoot", p);
    }
    c
}
fn run(cmd: &mut Command) -> Value {
    let o = cmd.output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}
#[test]
#[ignore = "requires KAR_PI_BIN with Pi RPC 1.4+"]
fn pi_skills_and_commands() {
    let program = std::env::var_os("KAR_PI_BIN").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("project");
    let profile = home.join("native-profile");
    write(
        &cwd.join(".pi/skills/local-fixture/SKILL.md"),
        "---\nname: local-fixture\ndescription: Project fixture\n---\nprivate skill body\n",
    );
    write(
        &profile.join("skills/user-fixture/SKILL.md"),
        "---\nname: user-fixture\ndescription: User fixture\n---\nprivate skill body\n",
    );
    write(
        &profile.join("prompts/summarize-fixture.md"),
        "---\ndescription: User template\n---\nprivate template body\n",
    );
    let trust_file = profile.join("trust.json");
    let query = |what: &str| {
        let mut cmd = process(Path::new(env!("CARGO_BIN_EXE_kar")), &home, &cwd);
        run(cmd
            .env("KAR_PI_AGENT_DIR", &profile)
            .args([what, "pi", "--cwd"])
            .arg(&cwd)
            .arg("--bin")
            .arg(&program))
    };
    for trusted in [false, true] {
        let trust =
            json!({(fs::canonicalize(&cwd).unwrap().to_str().unwrap()):trusted}).to_string();
        write(&trust_file, &trust);
        let skills = query("skills");
        assert_eq!(skills["kind"], "ok", "{skills}");
        let items = skills["items"].as_array().unwrap();
        let entry = |name: &str| items.iter().find(|r| r["name"] == name);
        assert_eq!(entry("user-fixture").unwrap()["scope"], "user", "{skills}");
        assert_eq!(entry("local-fixture").is_some(), trusted, "{skills}");
        if trusted {
            assert_eq!(entry("local-fixture").unwrap()["scope"], "project");
        }
        assert_eq!(skills["partial"], false);
        let commands = query("commands");
        assert_eq!(commands["kind"], "ok", "{commands}");
        assert!(
            commands["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["name"] == "summarize-fixture"
                    && r["source"] == "prompt"
                    && r["description"] == "User template")
        );
        assert!(!commands.to_string().contains("skill:local-fixture"));
        assert!(!format!("{skills}{commands}").contains("private "));
        assert_eq!(fs::read_to_string(&trust_file).unwrap(), trust);
    }
    let sessions = profile.join("sessions");
    assert!(walkdir_count(&sessions) == 0);
}

#[test]
#[ignore = "requires KAR_PI_BIN with Pi npm package, its examples and a Node host supporting native addons"]
fn pi_registered_tools() {
    let program = std::path::PathBuf::from(std::env::var_os("KAR_PI_BIN").unwrap());
    check_pi_registry(&program, None);
}

#[test]
#[ignore = "requires standalone KAR_PI_BIN, a matching KAR_PI_SDK_DIR, and Node on PATH"]
fn pi_standalone_registered_tools() {
    let program = std::path::PathBuf::from(std::env::var_os("KAR_PI_BIN").unwrap());
    let sdk = std::path::PathBuf::from(std::env::var_os("KAR_PI_SDK_DIR").unwrap());
    check_pi_registry(&program, Some(&sdk));
}

fn check_pi_registry(program: &Path, sdk: Option<&Path>) {
    let cli = fs::canonicalize(sdk.unwrap_or(program)).unwrap();
    let examples = cli
        .ancestors()
        .map(|dir| dir.join("examples/extensions"))
        .find(|dir| dir.join("dynamic-tools.ts").is_file())
        .expect("vendor extension examples");
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("project");
    let profile = home.join("native-profile");
    for (tools, sample) in [
        (profile.join("extensions"), "hello.ts"),
        (cwd.join(".pi/extensions"), "dynamic-tools.ts"),
    ] {
        fs::create_dir_all(&tools).unwrap();
        fs::copy(examples.join(sample), tools.join(sample)).unwrap();
    }
    let settings = r#"{"defaultTools":["read","hello"]}"#;
    // Exercise a snapshot larger than the native host's nonblocking pipe buffer.
    let description = if sdk.is_some() {
        let path = profile.join("extensions/hello.ts");
        let example = fs::read_to_string(&path).unwrap();
        let large = "测试参数".repeat(24000);
        write(&path, &example.replace("Name to greet", &large));
        Some(large)
    } else {
        None
    };
    write(&profile.join("settings.json"), settings);
    let trust_file = profile.join("trust.json");
    for trusted in [false, true] {
        let trust =
            json!({(fs::canonicalize(&cwd).unwrap().to_str().unwrap()):trusted}).to_string();
        write(&trust_file, &trust);
        let mut query = process(Path::new(env!("CARGO_BIN_EXE_kar")), &home, &cwd);
        query
            .env("KAR_PI_AGENT_DIR", &profile)
            .args(["tools", "pi", "--cwd"])
            .arg(&cwd)
            .arg("--bin")
            .arg(program);
        if let Some(dir) = sdk {
            assert_eq!(run(&mut query)["code"], "native_query_unavailable");
            query.env("KAR_PI_SDK_DIR", dir);
        }
        let result = run(&mut query);
        assert_eq!(result["kind"], "ok", "{result}");
        assert_eq!(result["view"], "registered");
        assert_eq!(result["partial"], false);
        let rows = result["items"].as_array().unwrap();
        let entry = |name: &str| rows.iter().find(|r| r["name"] == name);
        assert_eq!(entry("read").unwrap()["active"], true);
        let bash = entry("bash").unwrap();
        assert_eq!(bash["active"], false);
        assert_eq!(
            bash["inputSchema"]["properties"]["command"]["type"],
            "string"
        );
        let hello = entry("hello").unwrap();
        assert_eq!(hello["active"], true);
        assert_eq!(hello["description"], "A simple greeting tool");
        assert_eq!(hello["inputSchema"]["required"], json!(["name"]));
        if let Some(text) = &description {
            assert_eq!(
                hello["inputSchema"]["properties"]["name"]["description"].as_str(),
                Some(text.as_str())
            );
        }
        let dynamic = entry("echo_session");
        assert_eq!(dynamic.is_some(), trusted, "{result}");
        if let Some(dynamic) = dynamic {
            assert_eq!(dynamic["inputSchema"]["required"], json!(["message"]));
        }
        assert_eq!(fs::read_to_string(&trust_file).unwrap(), trust);
        assert_eq!(
            fs::read_to_string(profile.join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(walkdir_count(&profile.join("sessions")), 0);
    }
}

fn walkdir_count(path: &Path) -> usize {
    let Ok(dir) = fs::read_dir(path) else {
        return 0;
    };
    dir.flatten()
        .map(|e| {
            if e.path().is_dir() {
                walkdir_count(&e.path())
            } else {
                1
            }
        })
        .sum()
}

#[test]
#[ignore = "requires KAR_ANTIGRAVITY_BIN; real catalog in isolated HOME"]
fn antigravity_skills() {
    let program = std::env::var_os("KAR_ANTIGRAVITY_BIN").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let cwd = home.join("workspace");
    fs::create_dir_all(&cwd).unwrap();
    let make = |bin: &Path| process(bin, &home, &cwd);
    let version = make(Path::new(&program)).arg("--version").output().unwrap();
    let version = String::from_utf8(version.stdout).unwrap();
    let file_reader = cfg!(any(target_os = "macos", target_os = "linux"))
        && matches!(version.trim(), "1.5.10" | "1.5.11");
    let query = |signed_in: bool| {
        let mut cmd = make(Path::new(env!("CARGO_BIN_EXE_kar")));
        if signed_in {
            cmd.env("GEMINI_API_KEY", "aimock")
                .env("GOOGLE_GEMINI_BASE_URL", "http://127.0.0.1:9");
        }
        run(cmd
            .args(["skills", "antigravity", "--timeout-ms", "30000", "--bin"])
            .arg(&program)
            .arg("--cwd")
            .arg(&cwd))
    };
    let initial = query(false);
    if file_reader {
        assert_eq!(initial["items"], json!([]), "{initial}");
        assert_eq!(initial["partial"], true);
        assert!(!home.join(".gemini").exists());
    } else {
        assert_eq!(initial["code"], "not_authenticated", "{initial}");
    }
    write(
        &home.join(".gemini/antigravity-cli/settings.json"),
        r#"{"modelProvider":"gemini","setupComplete":true,"onboardingComplete":true}"#,
    );
    write(
        &home.join(".gemini/antigravity-acp/settings.json"),
        r#"{"auth":{"type":"gemini-api-key"}}"#,
    );
    write(
        &cwd.join(".agents/skills/my-manual/SKILL.md"),
        "---\nname: my-manual\ndescription: Fixture skill\ndisable-model-invocation: true\n---\nprivate skill body\n",
    );
    write(
        &cwd.join(".agents/skills/hidden/SKILL.md"),
        "---\nname: hidden-fixture\ndescription: hidden\ndisable-slash-command: true\n---\nprivate skill body\n",
    );
    write(
        &cwd.join(".agents/plugins/native/plugin.json"),
        r#"{"name":"Sample plugin"}"#,
    );
    write(
        &cwd.join(".agents/plugins/native/skills/child/SKILL.md"),
        "---\nname: child\ndescription: Plugin fixture\n---\nprivate plugin body\n",
    );
    let native = run(make(Path::new(&program))
        .env("GEMINI_API_KEY", "aimock")
        .env("GOOGLE_GEMINI_BASE_URL", "http://127.0.0.1:9")
        .args(["--output-format", "json", "--print=/skills"]));
    assert_eq!(native["num_turns"], 0);
    assert_eq!(native["conversation_id"], "");
    let result = query(true);
    assert_eq!(result["kind"], "ok", "{result}");
    let mut expected = Vec::new();
    for row in native["command"]["data"]["skills"].as_array().unwrap() {
        if file_reader
            && row["builtin"] == true
            && row["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/native_swarm/SKILL.md"))
        {
            continue;
        }
        let mut entry = json!({"name":row["name"],"userInvocable":true});
        for key in ["path", "description", "plugin"] {
            if let Some(v) = row.get(key) {
                entry[key] = v.clone();
            }
        }
        if row["builtin"] == true {
            entry["scope"] = "builtin".into();
        }
        if let Some(v) = row["model_invocable"].as_bool() {
            entry["disableModelInvocation"] = (!v).into();
        }
        expected.push(entry);
    }
    assert!(
        expected
            .iter()
            .any(|r| r["name"] == "my-manual" && r["disableModelInvocation"] == true)
    );
    assert!(expected.iter().any(|r| r["name"] == "native:child"));
    assert_eq!(result["partial"], file_reader);
    assert_eq!(result["items"], json!(expected));
    assert!(!result.to_string().contains("private skill body"));
    assert_eq!(
        walkdir_count(&home.join(".gemini/antigravity-cli/conversations")),
        0
    );
}
