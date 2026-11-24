//! Plan or execute the compiled macOS provider acceptance scenarios. Source
//! contracts and evidence classification are reported by backend-coverage.rs.
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};
#[path = "backend-suite/workbench.rs"]
mod workbench;

#[derive(Parser)]
struct Args {
    /// Compare the compiled macOS ignored test names with the checked-in matrix.
    #[arg(long)]
    audit_names: bool,
    /// Run whole scenarios. Omit to print the plan.
    #[arg(long)]
    execute: bool,
    /// Exact runtime lane, e.g. codex, pi, kimi-peer, or kar.
    #[arg(long)]
    runtime: Option<String>,
    /// Exact suite target or substring of the fully qualified test name.
    #[arg(long)]
    filter: Option<String>,
    /// Create a new evidence directory; existing directories are preserved.
    #[arg(long, requires = "execute")]
    output: Option<PathBuf>,
    /// Optional exact reported version of the primary provider executable.
    #[arg(long, requires = "runtime")]
    version: Option<String>,
}

#[derive(Clone, Serialize)]
struct Case {
    runtime: String,
    target: String,
    test: String,
    environment: Vec<String>,
    requires: String,
    program: String,
    args: Vec<String>,
    libtest: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    switches: BTreeMap<String, String>,
}
fn catalog() -> Result<Vec<Case>> {
    let mut cases = Vec::new();
    let mut keys = BTreeSet::new();
    for line in include_str!("../docs/backend-macos-tests.tsv")
        .lines()
        .skip(1)
    {
        let fields = line.split('\t').collect::<Vec<_>>();
        let [rt, target, test, environment, requires] = fields.as_slice() else {
            bail!("invalid native case: {line}")
        };
        ensure!(
            keys.insert((*target, *test)),
            "duplicate {}::{}",
            target,
            test
        );
        cases.push(Case {
            runtime: rt.to_string(),
            target: target.to_string(),
            test: test.to_string(),
            environment: environment
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            requires: requires.to_string(),
            program: "cargo".into(),
            libtest: true,
            switches: Default::default(),
            args: [
                "test",
                "--locked",
                "--test",
                target,
                test,
                "--",
                "--exact",
                "--ignored",
                "--nocapture",
            ]
            .map(str::to_owned)
            .into(),
        });
    }
    let mut append = |runtime: &str,
                      target: &str,
                      test: &str,
                      env: &[&str],
                      program: &str,
                      args: &[&str],
                      requires: &str| {
        cases.push(Case {
            runtime: runtime.into(),
            target: target.into(),
            test: test.into(),
            environment: env.iter().map(|s| s.to_string()).collect(),
            requires: requires.into(),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            libtest: false,
            switches: Default::default(),
        });
    };
    for (rt, flag, key) in [
        ("claude", "claude-mcp-native", "KAR_CLAUDE_BIN"),
        ("qoder", "qoder-mcp-native", "KAR_QODER_WORKER_BIN"),
    ] {
        append(
            rt,
            "protocol",
            flag,
            &[key],
            "cargo",
            &["test", "--locked", "--test", "protocol", "--", flag],
            "installed provider, isolated profile and MCP services",
        );
    }
    for rt in kar::runtimes() {
        let binary = format!("KAR_{}_BIN", rt.id.to_ascii_uppercase());
        for public_case in [
            "installation.snapshot",
            "account-usage.snapshot",
            "session.capabilities-match-declaration",
            "session.effort-never-substituted",
            "session.effort-listed-levels-apply-across-resume",
            "session.resume-or-loud-rejection",
            "session.query-readbacks-carry-stream-seq",
            "session.framing-and-attribution",
            "session.dispose-mid-turn-settles",
            "session.images-travel-with-input",
            "session.single-active-turn",
            "session.abort-settles-and-is-idempotent",
            "session.steer-mid-turn",
            "session.queue-runs-after-turn",
            "session.withdraw-before-dispatch",
        ] {
            if public_case == "session.effort-listed-levels-apply-across-resume"
                && !matches!(
                    rt.id.as_str(),
                    "codex" | "claude" | "qwen" | "cursor" | "grok" | "factory" | "opencode"
                )
            {
                continue;
            }
            if public_case == "account-usage.snapshot" && !rt.supports_account_usage() {
                continue;
            }
            append(
                &rt.id,
                "backend_public",
                public_case,
                &[&binary],
                "cargo",
                &[
                    "run",
                    "--locked",
                    "--quiet",
                    "--example",
                    "backend_public",
                    "--",
                    "--runtime",
                    &rt.id,
                    "--case",
                    public_case,
                ],
                "installed native runtime in a disposable workspace; session turns require its configured model/account",
            );
        }
    }
    append(
        "pi-durable",
        "browser",
        "live",
        &[],
        "node",
        &["tests/browser/live.mjs"],
        "build Wasm and install pinned browser dependencies per README; record Node, Chromium and Harness versions",
    );
    append(
        "kar",
        "contracts",
        "ignored-inventory",
        &[],
        "cargo",
        &[
            "run",
            "--locked",
            "--quiet",
            "--example",
            "backend_suite",
            "--",
            "--audit-names",
        ],
        "list standard first-stage libtest harnesses and report missing or stale names",
    );
    append(
        "kar",
        "contracts",
        "source",
        &["KAR_UPSTREAM_SOURCE"],
        "cargo",
        &[
            "run",
            "--locked",
            "--quiet",
            "--example",
            "backend_coverage",
            "--",
            "--upstream",
            "{KAR_UPSTREAM_SOURCE}",
        ],
        "upstream Git checkout containing the fixed commit and Kar history for the source/evidence map",
    );
    append(
        "kar",
        "regression",
        "fmt",
        &[],
        "cargo",
        &["fmt", "--all", "--", "--check"],
        "Rust with rustfmt",
    );
    append(
        "kar",
        "regression",
        "clippy",
        &[],
        "cargo",
        &[
            "clippy",
            "--locked",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        "Rust with clippy",
    );
    append(
        "kar",
        "regression",
        "test",
        &[],
        "cargo",
        &["test", "--locked"],
        "Cargo regression including the custom CLI/protocol test harnesses",
    );
    // The ordinary cargo regression always selects the protocol handoff fixture.
    // The independently recorded installed-provider case enables real credentials explicitly.
    cases
        .last_mut()
        .expect("regression test")
        .switches
        .insert("KAR_HANDOFF_LIVE".into(), "0".into());
    cases.extend(workbench::cases());
    Ok(cases)
}
fn output_text(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).output()?;
    ensure!(
        out.status.success(),
        "{cmd} {args:?} exited with {}",
        out.status
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}
fn save(output: &mut File, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    writeln!(output)?;
    output.sync_data()?;
    Ok(())
}

async fn installation(rt: &str, path: OsString, dir: &Path) -> Result<Value> {
    let mut provider = kar::runtime(rt)?;
    provider.program = path.into();
    let temp = tempfile::tempdir_in(dir)?;
    let snapshot = provider
        .probe_installation(&kar::InstallationOptions {
            cwd: Some(temp.path().into()),
            ..Default::default()
        })
        .await?;
    Ok(serde_json::to_value(snapshot)?)
}
// Canonical program keys can be populated from Kar's ordinary native discovery. Keys
// for a particular release, daemon, or protocol are explicit test prerequisites.
async fn local_program(key: &str, dir: &Path) -> Option<OsString> {
    if key == "KAR_TEST_CHROME" {
        let installed =
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        return installed.is_file().then(|| installed.into_os_string());
    }
    let id = key.strip_prefix("KAR_")?.strip_suffix("_BIN")?;
    if id.contains('_') {
        return None;
    }
    let rt = kar::runtime(&id.to_ascii_lowercase()).ok()?;
    let cwd = tempfile::tempdir_in(dir).ok()?;
    let opts = kar::InstallationOptions {
        cwd: Some(cwd.path().into()),
        ..Default::default()
    };
    match rt.probe_installation(&opts).await.ok()? {
        kar::InstallationSnapshot::Available {
            command: Some(path),
            ..
        } => Some(path.into_os_string()),
        _ => None,
    }
}

// Custom test binaries may treat --list as a request to run a scenario. Read
// the Cargo manifest before invoking libtest, including for new test targets.
fn audit_names(matrix: &[Case]) -> Result<()> {
    let manifest: toml::Value = toml::from_str(&fs::read_to_string("Cargo.toml")?)?;
    let custom: BTreeSet<_> = manifest
        .get("test")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("harness").and_then(toml::Value::as_bool) == Some(false))
        .filter_map(|t| t.get("name").and_then(toml::Value::as_str))
        .collect();
    let meta: Value = serde_json::from_str(&output_text(
        "cargo",
        &["metadata", "--no-deps", "--locked", "--format-version", "1"],
    )?)?;
    let package = meta["packages"]
        .as_array()
        .context("cargo metadata packages")?
        .iter()
        .find(|p| p["name"] == "kar")
        .context("Kar metadata")?;
    let mut actual = BTreeSet::new();
    for t in package["targets"]
        .as_array()
        .context("cargo metadata targets")?
    {
        let name = t["name"].as_str().context("test target name")?;
        if !t["kind"]
            .as_array()
            .is_some_and(|k| k.iter().any(|v| v == "test"))
            || custom.contains(name)
        {
            continue;
        }
        let listed = output_text(
            "cargo",
            &[
                "test",
                "--locked",
                "--quiet",
                "--test",
                name,
                "--",
                "--ignored",
                "--list",
                "--format",
                "terse",
            ],
        )?;
        for case in listed
            .lines()
            .filter_map(|line| line.strip_suffix(": test"))
        {
            // This ignored test is the child HTTP server started by other terminal scenarios.
            if name == "workbench_terminal_preview" && case == "fixture_http_server" {
                continue;
            }
            actual.insert((name.to_string(), case.to_string()));
        }
    }
    let expected: BTreeSet<_> = matrix
        .iter()
        .filter(|c| c.libtest)
        .map(|c| (c.target.clone(), c.test.clone()))
        .collect();
    for (target, test) in actual.difference(&expected) {
        println!("unregistered {target}::{test}");
    }
    for (target, test) in expected.difference(&actual) {
        println!("not compiled {target}::{test}");
    }
    ensure!(
        actual == expected,
        "compiled named tests differ from the macOS inventory"
    );
    println!("{} ignored macOS cases listed", actual.len());
    Ok(())
}
async fn run() -> Result<bool> {
    let a = Args::parse();
    if a.audit_names {
        ensure!(
            cfg!(target_os = "macos"),
            "compiled inventory is scoped to macOS"
        );
        audit_names(&catalog()?)?;
        return Ok(true);
    }
    let cases: Vec<_> = catalog()?
        .into_iter()
        .filter(|c| {
            a.runtime.as_ref().is_none_or(|rt| *rt == c.runtime)
                && a.filter
                    .as_ref()
                    .is_none_or(|s| c.target == *s || c.test.contains(s))
        })
        .collect();
    ensure!(!cases.is_empty(), "no matching cases");
    if !a.execute {
        for c in cases {
            println!("{}", serde_json::to_string(&c)?);
        }
        return Ok(true);
    }
    ensure!(
        cfg!(target_os = "macos"),
        "this acceptance matrix is scoped to macOS"
    );
    let dir = a.output.as_deref().context("--execute requires --output")?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .context("create a new evidence directory")?;
    let mut report = File::create_new(dir.join("results.jsonl"))?;
    let rev = output_text("git", &["rev-parse", "HEAD"])?;
    let platform = output_text("sw_vers", &["-productVersion"])?;
    let arch = std::env::consts::ARCH;
    // Evidence is meaningful only for a reproducible source revision. Preserve
    // any working-tree differences, including planned local changes, in metadata.
    save(
        &mut report,
        &json!({"type":"run","revision":rev,"platform":platform,"arch":arch,"rust":output_text("rustc",&["-V"])?,"changes":output_text("git",&["status","--porcelain","--untracked-files=normal"])?,"selected":cases.len()}),
    )?;
    let mut probes = BTreeMap::new();
    let mut discovered = BTreeMap::new();
    let mut complete = true;
    for (i, c) in cases.iter().enumerate() {
        let mut row = json!({"type":"case","runtime":c.runtime,"target":c.target,"test":c.test,"requires":c.requires,"executable":c.program,"argv":c.args,"switches":c.switches,"revision":rev,"platform":platform,"arch":arch});
        let mut local = BTreeMap::new();
        let mut absent = Vec::new();
        for key in &c.environment {
            if std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
                continue;
            }
            if !discovered.contains_key(key) {
                discovered.insert(key.clone(), local_program(key, dir).await);
            }
            if let Some(path) = &discovered[key] {
                local.insert(key.clone(), path.clone());
            } else {
                absent.push(key);
            }
        }
        row["discovered_programs"] = json!(local);
        if !absent.is_empty() {
            row["status"] = "missing_environment".into();
            row["missing"] = json!(absent);
            save(&mut report, &row)?;
            complete = false;
            continue;
        }
        let mut binaries = BTreeMap::new();
        let mut unavailable = false;
        let mut versions_confirmed = true;
        for (n, key) in c
            .environment
            .iter()
            .filter(|k| k.ends_with("_BIN"))
            .enumerate()
        {
            let path = local
                .get(key)
                .cloned()
                .or_else(|| std::env::var_os(key))
                .expect("checked");
            let provider = key
                .trim_start_matches("KAR_")
                .split('_')
                .next()
                .context("binary variable")?
                .to_ascii_lowercase();
            let cache_key = (provider.clone(), path.clone());
            if !probes.contains_key(&cache_key) {
                let snap = installation(&provider, path.clone(), dir)
                    .await
                    .unwrap_or_else(|e| json!({"kind":"probe_error","reason":e.to_string()}));
                probes.insert(cache_key.clone(), snap);
            }
            let snap = &probes[&cache_key];
            binaries.insert(
                key,
                json!({"path":path.to_string_lossy(),"installation":snap}),
            );
            versions_confirmed &= snap["kind"] == "available" && snap["version"].is_string();
            if snap["kind"] == "missing"
                || (n == 0
                    && a.version
                        .as_ref()
                        .is_some_and(|v| snap["version"].as_str() != Some(v)))
            {
                unavailable = true;
            }
        }
        row["binaries"] = json!(binaries);
        if unavailable {
            row["status"] = "missing_or_mismatched_installation".into();
            row["required_version"] = json!(a.version);
            save(&mut report, &row)?;
            complete = false;
            continue;
        }
        let out = format!("{i:04}.stdout.log");
        let err = format!("{i:04}.stderr.log");
        row["stdout"] = json!(out);
        row["stderr"] = json!(err);
        save(&mut report, &json!({"type":"start","case":row}))?;
        let argv: Vec<OsString> = c
            .args
            .iter()
            .map(|arg| {
                if arg == "{KAR_UPSTREAM_SOURCE}" {
                    std::env::var_os("KAR_UPSTREAM_SOURCE").expect("checked source checkout")
                } else {
                    OsString::from(arg)
                }
            })
            .collect();
        let status = Command::new(&c.program)
            .args(&argv)
            .envs(&local)
            .envs(&c.switches)
            .stdin(Stdio::null())
            .stdout(File::create_new(dir.join(&out))?)
            .stderr(File::create_new(dir.join(&err))?)
            .status();
        match status {
            Ok(s) => {
                let mut counted = !c.libtest;
                if c.libtest && s.success() {
                    for l in BufReader::new(File::open(dir.join(&out))?).lines() {
                        if l?.starts_with("test result: ok. 1 passed;") {
                            counted = true;
                        }
                    }
                }
                row["exit_code"] = json!(s.code());
                row["status"] = if !s.success() {
                    "failed"
                } else if !counted {
                    "no_matching_test"
                } else if !versions_confirmed {
                    "command_succeeded_version_unconfirmed"
                } else {
                    "command_succeeded"
                }
                .into();
                complete &= s.success() && counted && versions_confirmed;
            }
            Err(e) => {
                row["status"] = "launch_failed".into();
                row["reason"] = e.to_string().into();
                complete = false;
            }
        }
        save(&mut report, &row)?;
    }
    Ok(complete)
}
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
