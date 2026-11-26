//! Kar CLI and SDK against local programs, with no vendor installation or network access.
#[cfg(unix)]
mod local {
    use serde_json::{Value, json};
    use std::{
        env, fs,
        io::{self, Read, Write},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    fn fake(name: &str) -> i32 {
        let root = PathBuf::from(env::var_os("KAR_RECEIPT_FIXTURE").unwrap());
        let args: Vec<_> = env::args().skip(1).collect();
        let mode = fs::read_to_string(root.join("mode")).unwrap();
        let version = root.join("version");
        let bin = root.join("bin");
        assert!(env::var_os("KAR_WORKBENCH_TOKEN").is_none());
        match name {
            "curl" => {
                assert_eq!(args.last().unwrap(), "https://app.factory.ai/cli");
                let i = args.iter().position(|x| x == "--output").unwrap();
                fs::write(&args[i + 1], b"local fixture only").unwrap();
                println!("download");
                eprintln!("download diagnostics");
                0
            }
            "sh" => {
                assert_eq!(fs::read(&args[0]).unwrap(), b"local fixture only");
                assert!(
                    env::var_os("KAR_WORKBENCH_TOKEN").is_none()
                        && env::var_os("npm_package_name").is_none()
                );
                assert_eq!(env::var("npm_config_prefix").unwrap(), "retained");
                assert_eq!(io::stdin().read(&mut [0; 1]).unwrap(), 0);
                println!("installer stdout");
                eprintln!("installer stderr");
                if mode == "stall" {
                    io::stdout().flush().unwrap();
                    std::thread::sleep(Duration::from_secs(30));
                }
                if mode == "install" {
                    fs::hard_link(env::current_exe().unwrap(), bin.join("droid")).unwrap();
                    fs::write(version, "1.13.0").unwrap();
                    17
                } else if mode == "fail" {
                    19
                } else {
                    0
                }
            }
            "droid" => {
                if mode == "shadow"
                    && args == ["--version"]
                    && env::args_os().next().unwrap() == bin.join("droid")
                {
                    println!("1.60.0");
                    return 0;
                }
                if args == ["exec", "--help"] {
                    println!("Usage: droid exec");
                    return 0;
                }
                if args == ["--version"] {
                    return match fs::read_to_string(version) {
                        Ok(v) => {
                            println!("{v}");
                            0
                        }
                        Err(_) => 1,
                    };
                }
                if args == ["update", "--check"] {
                    if mode == "current" {
                        println!("Already up to date!");
                    } else {
                        println!("Update available: v1.99.0");
                    }
                    return 0;
                }
                assert_eq!(args, ["update"]);
                assert!(env::var_os("npm_package_name").is_none());
                fs::write(root.join("updater_called"), b"1").unwrap();
                println!("update stdout");
                eprintln!("update stderr");
                match mode.as_str() {
                    "shadow" => {
                        fs::hard_link(env::current_exe().unwrap(), bin.join("droid")).unwrap();
                        0
                    }
                    "upgrade" => {
                        fs::write(version, "1.14.0").unwrap();
                        9
                    }
                    "broken" => {
                        fs::remove_file(version).unwrap();
                        0
                    }
                    "fail" => 18,
                    _ => 0,
                }
            }
            "opencode" => {
                if args == ["--version"] {
                    println!("opencode v2.0.42");
                } else {
                    assert_eq!(args, ["--help"]);
                    println!("OpenCode command line interface");
                }
                0
            }
            "kimi" => {
                if args == ["--version"] {
                    println!("0.42.0");
                } else {
                    assert_eq!(args, ["acp", "--help"]);
                    println!("Usage: kimi acp");
                }
                0
            }
            "dsh" => {
                if args == ["--version"] {
                    println!("dsh 0.3.0");
                } else {
                    assert_eq!(args, ["--help"]);
                    println!("Usage: dsh");
                }
                0
            }
            "npm" => {
                if args.first().is_some_and(|a| a == "install") {
                    assert_eq!(args, ["install", "--global", "@deepseek-ai/dsh"]);
                    assert!(env::var_os("npm_package_name").is_none());
                    assert_eq!(env::var("npm_config_prefix").unwrap(), "retained");
                    fs::hard_link(env::current_exe().unwrap(), bin.join("dsh")).unwrap();
                    println!("installed package");
                    return 7;
                }
                assert_eq!(&args[..3], ["prefix", "-g", "--cache"]);
                assert!(args.contains(&"--update-notifier=false".to_string()));
                assert!(Path::new(&args[3]).is_dir());
                assert!(env::var_os("npm_package_name").is_none());
                fs::write(root.join("temp_cache"), &args[3]).unwrap();
                if mode == "invalid" {
                    println!("secret-invalid-prefix");
                } else if mode == "exit" {
                    eprintln!("secret-npm-diagnostic");
                    return 23;
                } else {
                    println!("{}", root.join("prefix").display());
                }
                0
            }
            _ => panic!("unexpected mock program {name}"),
        }
    }
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        bin: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let bin = root.join("bin");
            fs::create_dir(&bin).unwrap();
            fs::create_dir(root.join("home")).unwrap();
            let f = Self {
                _tmp: tmp,
                root,
                bin,
            };
            f.mode("none");
            f
        }
        fn tool(&self, name: &str) {
            fs::hard_link(env::current_exe().unwrap(), self.bin.join(name)).unwrap();
        }
        fn mode(&self, mode: &str) {
            fs::write(self.root.join("mode"), mode).unwrap();
        }
        fn cli(&self, args: &[&str], expected: bool, extra: &[(&str, &Path)]) -> Value {
            let out = Command::new(env!("CARGO_BIN_EXE_kar"))
                .args(args)
                .env_clear()
                .current_dir(&self.root)
                .env("HOME", self.root.join("home"))
                .env("PATH", &self.bin)
                .env("KAR_RECEIPT_FIXTURE", &self.root)
                .env("KAR_WORKBENCH_TOKEN", "not-for-children")
                .env("npm_package_name", "remove")
                .env("npm_config_prefix", "retained")
                .envs(extra.iter().copied())
                .output()
                .unwrap();
            assert_eq!(
                out.status.success(),
                expected,
                "{args:?} stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        }
    }

    fn check_host() {
        let f = Fixture::new();
        let plan = |id: &str, extra: &[(&str, &Path)]| {
            f.cli(&["install", id, "--plan", "--json"], true, extra)["plan"].clone()
        };
        let missing = plan("factory", &[]);
        assert_eq!(missing["reason"], "missing_tool");
        assert!(missing["source"].is_string() && missing["steps"].is_array());
        f.tool("curl");
        f.tool("sh");
        f.tool("npm");
        f.tool("bash");
        let qwen_bin = f.root.join("custom-qwen-bin");
        let qwen = plan("qwen", &[("QWEN_INSTALL_BIN_DIR", &qwen_bin)]);
        assert_eq!(qwen["locations"][0], json!(qwen_bin));
        assert_eq!(
            qwen["steps"][1]["command"],
            json!([
                "bash",
                "<temporary installer file>",
                "--method",
                "standalone",
                "--no-modify-path"
            ])
        );
        let p = plan("codex", &[]);
        assert_eq!(p["kind"], "plan");
        assert_eq!(p["privileges"], false);
        assert_eq!(p["steps"][1]["environment"]["CODEX_NON_INTERACTIVE"], "1");
        assert!(
            p["locations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == f.root.join("home/.local/bin").to_str().unwrap())
        );
        let op = plan("opencode", &[]);
        assert_eq!(op["reason"], "line_required");
        assert!(op.get("steps").is_none());
        let unknown = f.cli(
            &[
                "install", "factory", "--plan", "--line", "unknown", "--json",
            ],
            true,
            &[],
        );
        assert_eq!(unknown["plan"]["reason"], "unknown_line");
        let pinned = f.root.join("missing-binary");
        assert_eq!(
            plan("factory", &[("KAR_FACTORY_BIN", &pinned)])["reason"],
            "configuration"
        );
        f.mode("invalid");
        let invalid = plan("dsh", &[]);
        assert_eq!(invalid["reason"], "location_unknown");
        assert!(!invalid.to_string().contains("secret-"));
        assert!(!Path::new(&fs::read_to_string(f.root.join("temp_cache")).unwrap()).exists());
        f.mode("exit");
        let r = plan("dsh", &[]);
        assert_eq!(r["reason"], "location_unknown");
        assert!(!r.to_string().contains("secret-"));
        assert!(r["detail"].as_str().unwrap().contains("23"));
        f.mode("none");
        let prefix = f.root.join("prefix");
        fs::create_dir(&prefix).unwrap();
        let ready = plan("dsh", &[]);
        assert_eq!(
            ready["locations"][2],
            json!(prefix.join("lib/node_modules/@deepseek-ai/dsh"))
        );
        if !nix::unistd::Uid::effective().is_root() {
            fs::set_permissions(&prefix, fs::Permissions::from_mode(0o555)).unwrap();
            assert_eq!(plan("dsh", &[])["reason"], "requires_privileges");
            fs::set_permissions(&prefix, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn check_install() {
        let f = Fixture::new();
        f.tool("curl");
        f.tool("sh");
        let install = |ok| f.cli(&["install", "factory", "--json"], ok, &[])["install"].clone();
        let failed = install(false);
        assert_eq!(failed["exit_code"], 0);
        assert_eq!(failed["kind"], "failed");
        f.mode("fail");
        let failed = install(false);
        assert_eq!(failed["exit_code"], 19);
        f.mode("stall");
        let timed = f.cli(
            &["install", "factory", "--json", "--timeout-ms", "1000"],
            false,
            &[],
        );
        assert!(timed["install"]["exit_code"].is_null());
        let output = timed["install"]["output"].as_str().unwrap();
        assert!(output.contains("installer stdout") && output.contains("1000 ms"));
        f.mode("install");
        let done = install(true);
        assert_eq!(done["kind"], "installed");
        assert_eq!(done["exit_code"], 17);
        assert_eq!(done["installation"]["command"], json!(f.bin.join("droid")));
        assert_eq!(
            done["output"],
            "download\ndownload diagnostics\ninstaller stdout\ninstaller stderr\n"
        );
        f.tool("npm");
        let package = f.cli(&["install", "dsh", "--json"], true, &[]);
        assert_eq!(package["install"]["kind"], "installed");
        assert_eq!(package["install"]["output"], "installed package\n");
        let repeat = install(true);
        assert_eq!(repeat["kind"], "already_installed");
        assert!(repeat.get("output").is_none());
        fs::remove_file(f.bin.join("curl")).unwrap();
        f.tool("opencode");
        let op = f.cli(
            &["install", "opencode", "--line", "v1", "--json"],
            true,
            &[],
        );
        assert_eq!(op["install"]["kind"], "already_installed");
        assert_eq!(op["install"]["line"], "v2");
        assert_eq!(install(true)["kind"], "already_installed");
    }
    fn check_update() {
        let f = Fixture::new();
        f.tool("droid");
        fs::write(f.root.join("version"), "1.13.0").unwrap();
        let update = |ok| f.cli(&["update", "factory", "--json"], ok, &[])["upgrade"].clone();
        f.tool("kimi");
        let terminal = f.cli(&["update", "kimi", "--json"], true, &[]);
        assert_eq!(terminal["upgrade"]["reason"], "requires_terminal");
        f.mode("current");
        let current = update(true);
        assert_eq!(current["kind"], "current");
        assert_eq!(current["check"]["update_available"], false);
        assert!(!f.root.join("updater_called").exists());
        f.mode("none");
        let same = update(false);
        assert_eq!(same["kind"], "unchanged");
        assert_eq!(same["version"], "1.13.0");
        f.mode("fail");
        assert_eq!(update(false)["exit_code"], 18);
        f.mode("upgrade");
        let changed = update(true);
        assert_eq!(
            changed,
            json!({"kind":"upgraded","from":"1.13.0","to":"1.14.0","exit_code":9,"output":"update stdout\nupdate stderr\n"})
        );
        let fallback = f.root.join("home/.local/bin");
        fs::create_dir_all(&fallback).unwrap();
        fs::rename(f.bin.join("droid"), fallback.join("droid")).unwrap();
        f.mode("shadow");
        let wrong = update(false);
        assert_eq!(wrong["kind"], "unchanged");
        assert_eq!(wrong["version"], "1.14.0");
        fs::remove_file(f.bin.join("droid")).unwrap();
        f.mode("broken");
        let broken = update(false);
        assert_eq!(broken["kind"], "failed");
        assert_eq!(broken["exit_code"], 0);
    }
    pub(super) fn main() {
        if env::var_os("KAR_RECEIPT_FIXTURE").is_some() {
            let path = PathBuf::from(env::args_os().next().unwrap());
            std::process::exit(fake(path.file_name().unwrap().to_str().unwrap()));
        }
        check_host();
        check_install();
        check_update();
    }
}
fn main() {
    #[cfg(unix)]
    local::main();
}
