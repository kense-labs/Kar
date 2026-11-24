//! Exercise the public CLI across process boundaries, including a crash after native submission.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{self, BufRead, Read, Write},
        path::PathBuf,
        process::Stdio,
        time::Duration,
    };
    use tokio::process::Command;

    fn mark(s: &str) {
        let p = PathBuf::from(std::env::var_os("HOME").unwrap()).join("events");
        writeln!(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .unwrap(),
            "{s}"
        )
        .unwrap();
    }
    fn draw(s: &str) {
        print!("\x1b[2J\x1b[H{}", s.replace('\n', "\r\n"));
        io::stdout().flush().unwrap();
    }
    fn prompt(echo: &str, input: &str) {
        draw(&format!(
            "Claude Code v2.1.330\n{echo}\n──────────\n❯\u{a0}{input}\n──────────\n? for shortcuts"
        ));
    }
    fn quota(case: &str) {
        mark("usage");
        for req in io::stdin()
            .lock()
            .lines()
            .map(|s| serde_json::from_str::<Value>(&s.unwrap()).unwrap())
        {
            let response = match req["request"]["subtype"].as_str().unwrap() {
                "initialize" => json!({"account":{"email":"test@example.invalid"}}),
                "get_usage" => {
                    json!({"rate_limits_available":case!="unread","rate_limits":{"five_hour":{"utilization":if case=="capped"{100}else{20}}}})
                }
                _ => panic!("unexpected control request"),
            };
            println!(
                "{}",
                json!({"type":"control_response","response":{"request_id":req["request_id"],"subtype":"success","response":response}})
            );
        }
    }
    pub fn peer(case: &str) {
        let args: Vec<_> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("--help") => return println!("Claude Code --safe-mode"),
            Some("--version") => return println!("2.1.330 (Claude Code)"),
            Some("-p") => return quota(case),
            None => {}
            _ => panic!("unexpected arguments"),
        }
        assert_eq!(fs::read_dir(".").unwrap().count(), 0);
        assert!(std::env::var_os("CLAUDECODE").is_none());
        use nix::sys::termios::*;
        let mut t = tcgetattr(io::stdin()).unwrap();
        cfmakeraw(&mut t);
        tcsetattr(io::stdin(), SetArg::TCSANOW, &t).unwrap();
        mark("open");
        let mut stdin = io::stdin().lock();
        if case == "new" || case == "unexpected" {
            for (name, s) in [
                (
                    "theme",
                    include_str!("../src/account/quota/claude_reset/screens/theme.txt"),
                ),
                (
                    "security",
                    include_str!("../src/account/quota/claude_reset/screens/security.txt"),
                ),
            ] {
                if case == "unexpected" {
                    draw(&format!("{s}\nAdditional unrecognised choice"));
                } else {
                    draw(s);
                }
                let mut key = [0];
                if stdin.read_exact(&mut key).is_err() {
                    return;
                }
                assert_eq!(key, b"\r".as_slice());
                mark(name);
            }
            let trust = |a, b| {
                format!(
                    "Quick safety check: Is this a project you created or one you trust?\n{a}No, exit\n{b}Yes, I trust this folder\nEnter to confirm · Esc to cancel"
                )
            };
            draw(&trust("❯ ", ""));
            let mut down = [0; 3];
            stdin.read_exact(&mut down).unwrap();
            assert_eq!(&down, b"\x1b[B");
            draw(&trust("", "❯ "));
            let mut enter = [0];
            stdin.read_exact(&mut enter).unwrap();
            assert_eq!(&enter, b"\r");
            mark("trust");
        }
        prompt("", "");
        loop {
            let mut buf = [0];
            let mut cmd = String::new();
            while stdin.read_exact(&mut buf).is_ok() {
                if buf[0] == 13 {
                    break;
                }
                assert!((32..127).contains(&buf[0]));
                cmd.push(buf[0] as char);
                prompt("", &cmd);
            }
            let Some(kind) = cmd.strip_prefix("/limit-reset ") else {
                return;
            };
            assert!(["5-hour", "weekly", "fable", "full"].contains(&kind));
            mark(&cmd);
            if case == "crash" {
                std::thread::sleep(Duration::from_secs(10));
                return;
            }
            if case == "legacy" {
                prompt(
                    &format!("❯ {cmd}\n  ⎿  5-hour limit reset · next reset available Friday"),
                    "",
                );
                continue;
            }
            let title = if kind == "fable" { "Fable" } else { kind };
            let effect = match kind {
                "full" => "5-hour and weekly",
                _ => title,
            };
            let count = if kind == "5-hour" { "five-hour" } else { title };
            let extra = if case == "changed" {
                " Also approves a different action."
            } else {
                ""
            };
            draw(&format!(
                "Use a {title} reset?\nRefills your {effect} limit now · your weekly reset day stays Nov 14, 5pm{extra}\n2 {count} resets left · use by Nov 19, 4:59pm\n❯ Yes, use my {title} reset\nNo, keep it\nEnter to confirm · Esc to cancel"
            ));
            if stdin.read_exact(&mut buf).is_err() {
                return;
            }
            let text = match buf[0] {
                27 => {
                    mark("escape");
                    format!(
                        "{} reset kept · run /limit-reset later",
                        match kind {
                            "weekly" => "Weekly",
                            "full" => "Full",
                            _ => title,
                        }
                    )
                }
                13 => {
                    mark("confirm");
                    assert_eq!(kind, "weekly");
                    "Weekly limits reset · your weekly reset day stays Friday".into()
                }
                _ => panic!("unexpected dialog action"),
            };
            prompt(&format!("❯ {cmd}\n  ⎿  {text}"), "");
        }
    }
    struct Profile {
        dir: tempfile::TempDir,
    }
    impl Profile {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join(".claude.json"),
                json!({"cachedGrowthBookFeatures":{"tengu_cedar_ember":{"enabled":true}}})
                    .to_string(),
            )
            .unwrap();
            Self { dir }
        }
        fn cli(&self, case: &str, action: &str, args: &[&str]) -> Command {
            let h = self.dir.path();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
            cmd.env_clear()
                .env("HOME", h)
                .env("CLAUDE_CONFIG_DIR", h)
                .env("CLAUDECODE", "parent")
                .env("KAR_CLAUDE_RESET_PEER", case)
                .args(["account", "claude", action, "--bin"])
                .arg(std::env::current_exe().unwrap())
                .arg("--cwd")
                .arg(h)
                .args(args)
                .kill_on_drop(true);
            cmd
        }
        async fn call(&self, case: &str, action: &str, args: &[&str]) -> Value {
            let o = self.cli(case, action, args).output().await.unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            serde_json::from_slice(&o.stdout).unwrap()
        }
        fn events(&self) -> Vec<String> {
            fs::read_to_string(self.dir.path().join("events"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }
    const REDEEM: &str = "redeem-limit-reset";
    const INSPECT: &str = "inspect-limit-resets";
    const KEY: &[&str] = &["--reset-id", "weekly", "--idempotency-key", "one-attempt"];
    async fn spend() {
        let p = Profile::new();
        let (a, b) = tokio::join!(
            p.call("success", REDEEM, KEY),
            p.call("success", REDEEM, KEY)
        );
        assert_eq!(a["kind"], "reset", "{a} / {:?}", p.events());
        assert_eq!(a, b);
        assert_eq!(
            p.events(),
            ["open", "usage", "/limit-reset weekly", "confirm"]
        );
        assert_eq!(p.call("success", REDEEM, KEY).await, a);
        let o = p
            .cli(
                "success",
                REDEEM,
                &["--reset-id", "full", "--idempotency-key", "one-attempt"],
            )
            .output()
            .await
            .unwrap();
        assert!(!o.status.success());
        assert_eq!(p.events().len(), 4);
    }
    async fn inspect() {
        let p = Profile::new();
        let v = p.call("success", INSPECT, &["--include-five-hour"]).await;
        assert_eq!(v["kind"], "inspected", "{v} / {:?}", p.events());
        for r in v["resets"].as_array().unwrap() {
            assert_eq!(r["state"], "available");
            assert_eq!(r["resetsLeft"], 2);
        }
        let events = p.events();
        assert_eq!(
            events,
            [
                "open",
                "usage",
                "/limit-reset weekly",
                "escape",
                "/limit-reset fable",
                "escape",
                "/limit-reset full",
                "escape",
                "/limit-reset 5-hour",
                "escape"
            ]
        );
    }
    async fn risk() {
        let p = Profile::new();
        fs::write(p.dir.path().join(".claude.json"), "{}").unwrap();
        assert_eq!(
            p.call("capped", REDEEM, KEY).await["reason"],
            "consent_required"
        );
        assert!(p.events().is_empty());
        assert_eq!(
            p.call("capped", INSPECT, &[]).await["reason"],
            "consent_required"
        );
        assert_eq!(
            p.call("unread", INSPECT, &[]).await["reason"],
            "usage_unread"
        );
        assert!(!p.events().iter().any(|s| s.starts_with("/limit-reset")));
        let r = p
            .call(
                "legacy",
                REDEEM,
                &[KEY, &["--five-hour-limit-consent"]].concat(),
            )
            .await;
        assert_eq!(r, json!({"kind":"unknown","fiveHourSpent":true}));
        let r = p
            .call("legacy", INSPECT, &["--five-hour-limit-consent"])
            .await;
        assert_eq!(r["program"], "legacy");
        assert_eq!(r["resets"][0]["state"], "unknown");
        assert_eq!(r["resets"][1]["state"], "not_offered");
    }
    async fn changed() {
        let p = Profile::new();
        assert_eq!(p.call("changed", REDEEM, KEY).await["kind"], "unknown");
        assert_eq!(
            p.events(),
            ["open", "usage", "/limit-reset weekly", "escape"]
        );
    }
    async fn crash() {
        let p = Profile::new();
        let mut child = p
            .cli("crash", REDEEM, KEY)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(16), async {
            while !p.events().iter().any(|s| s == "/limit-reset weekly") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        child.kill().await.unwrap();
        let before = p.events();
        assert_eq!(p.call("crash", REDEEM, KEY).await["kind"], "unknown");
        assert_eq!(p.events(), before);
    }
    async fn onboarding() {
        let p = Profile::new();
        let r = p.call("unexpected", REDEEM, KEY).await;
        assert_eq!(r["reason"], "startup_failed");
        assert_eq!(r["startup"]["screen"], "onboarding");
        assert_eq!(p.events(), ["open"]);
        let r = p.call("new", REDEEM, KEY).await;
        assert_eq!(
            r,
            json!({"kind":"reset","onboardingCompleted":true}),
            "{r} / {:?}",
            p.events()
        );
        assert_eq!(
            p.events(),
            [
                "open",
                "open",
                "theme",
                "security",
                "trust",
                "usage",
                "/limit-reset weekly",
                "confirm"
            ]
        );
    }
    async fn concurrent_sdk() {
        let p = Profile::new();
        let h = p.dir.path();
        let mut rt = kar::runtime("claude").unwrap();
        rt.program = std::env::current_exe().unwrap();
        let mut options = kar::account::LimitResetOptions::new("shared");
        options.installation.cwd = Some(h.into());
        options.reset_id = Some("weekly".into());
        options.installation.env = std::env::vars_os().map(|(k, _)| (k, None)).collect();
        for key in ["HOME", "CLAUDE_CONFIG_DIR"] {
            options
                .installation
                .env
                .insert(key.into(), Some(h.as_os_str().into()));
        }
        options
            .installation
            .env
            .insert("KAR_CLAUDE_RESET_PEER".into(), Some("success".into()));
        let mut other = options.clone();
        other.reset_id = Some("five_hour".into());
        let (many, conflict) = tokio::join!(
            futures_util::future::join_all((0..7).map(|_| rt.redeem_limit_reset(&options))),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                rt.redeem_limit_reset(&other).await
            }
        );
        assert!(conflict.is_err());
        for result in many {
            assert_eq!(result.unwrap(), kar::account::LimitResetResult::Reset);
        }
        assert_eq!(
            p.events(),
            ["open", "usage", "/limit-reset weekly", "confirm"]
        );
    }
    pub async fn suite() {
        tokio::join!(
            spend(),
            inspect(),
            risk(),
            changed(),
            crash(),
            onboarding(),
            concurrent_sdk()
        );
    }
}
#[tokio::main]
async fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if let Ok(case) = std::env::var("KAR_CLAUDE_RESET_PEER") {
            return unix::peer(&case);
        }
        unix::suite().await;
        println!(
            "Claude reset inspection, spend, retry, changed screens and process interruption passed"
        );
    }
}
