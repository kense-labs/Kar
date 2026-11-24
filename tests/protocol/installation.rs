#[cfg(unix)]
use kar::{InstallationIssue, InstallationOptions, InstallationSnapshot, Protocol, Runtime};
#[cfg(unix)]
use std::path::PathBuf;
use std::{fs, path::Path};

pub(super) fn peer(args: &[String]) {
    let dir = Path::new(&args[0]).parent().unwrap();
    let file = if args.get(1).is_some_and(|s| s == "--version") {
        "version"
    } else {
        "help"
    };
    fs::write(dir.join(format!("called-{file}")), b"").unwrap();
    print!("{}", fs::read_to_string(dir.join(file)).unwrap());
}
#[cfg(unix)]
pub(super) async fn exercise() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let source = std::env::current_exe().unwrap();
    fn install(dir: &Path, name: &str, source: &Path, ver: &str, help: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join(name);
        fs::copy(source, &bin).unwrap();
        fs::write(dir.join("version"), ver).unwrap();
        fs::write(dir.join("help"), help).unwrap();
        bin
    }
    // An existing launcher with an unavailable interpreter is a different state
    // from a missing executable or a vendor binary below its supported version.
    let broken = root.join("kar-test-provider");
    fs::write(&broken, b"#!/usr/bin/env kar-unavailable-interpreter\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&broken,fs::Permissions::from_mode(0o755)).unwrap();
    let host_runtime = Runtime::new("codex", &broken, &[], Protocol::Codex);
    assert!(matches!(host_runtime.probe_installation(&InstallationOptions::default()).await.unwrap(),
        InstallationSnapshot::Failed { code: InstallationIssue::DependencyMissing, reason, .. } if reason.contains("kar-unavailable-interpreter")));
    let primary = install(
        &root.join("first"),
        "claude",
        &source,
        "2.1.330 (Claude Code)",
        "",
    );
    let shadow = install(
        &root.join("second"),
        "claude",
        &source,
        "2.1.300 (Claude Code)",
        "",
    );
    let fallback = install(
        &home.join(".local/bin"),
        "claude",
        &source,
        "2.1.290 (Claude Code)",
        "",
    );
    fs::create_dir(root.join("alias")).unwrap();
    symlink(&primary, root.join("alias/claude")).unwrap();
    symlink("second", root.join("link-to-second")).unwrap();
    let mut opts = InstallationOptions {
        cwd: Some(root.clone()),
        ..Default::default()
    };
    opts.env
        .insert("HOME".into(), Some(home.clone().into_os_string()));
    opts.env.insert(
        "PATH".into(),
        Some("first:alias:first:second:link-to-second".into()),
    );
    opts.env
        .insert("KAR_TEST_INSTALLATION".into(), Some("1".into()));
    opts.env.insert("KAR_CLAUDE_BIN".into(), None);
    let rt = kar::runtime("claude").unwrap();
    let result = rt.probe_installation(&opts).await.unwrap();
    assert!(
        matches!(result,InstallationSnapshot::Available{command:Some(ref p),ref shadowed,..} if *p==primary && shadowed.as_slice()==std::slice::from_ref(&shadow))
    );
    assert!(
        !root.join("second/called-version").exists()
            && !home.join(".local/bin/called-version").exists()
    );
    let expected = rt.session_capabilities(&result).unwrap();
    assert_eq!(expected.side_question, Some(true));
    let cli_report = |bin: &Path| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_kar"))
            .args(["list", "claude", "--cwd"])
            .arg(&root)
            .arg("--bin")
            .arg(bin)
            .env("KAR_TEST_INSTALLATION", "1")
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let found = cli_report(&primary);
    assert_eq!(
        found["session_capabilities"],
        serde_json::to_value(expected).unwrap()
    );
    let missing = cli_report(&root.join("absent"));
    assert_eq!(missing["installation"]["kind"], "missing");
    assert!(missing.get("session_capabilities").is_none());
    let marker = root.join("first/called-version");
    fs::remove_file(&marker).unwrap();
    rt.probe_installation(&opts).await.unwrap();
    assert!(
        !marker.exists(),
        "unchanged executable and environment should reuse the probe"
    );
    opts.env
        .insert("KAR_EXTRA_PROBE_ENV".into(), Some("other".into()));
    rt.probe_installation(&opts).await.unwrap();
    assert!(
        marker.exists(),
        "a different child environment needs its own probe"
    );
    opts.env.remove(std::ffi::OsStr::new("KAR_EXTRA_PROBE_ENV"));
    opts.env
        .insert("KAR_CLAUDE_BIN".into(), Some("claude".into()));
    assert!(
        matches!(rt.probe_installation(&opts).await.unwrap(),InstallationSnapshot::Available{shadowed,..} if shadowed.is_empty())
    );
    opts.env.insert(
        "KAR_CLAUDE_BIN".into(),
        Some(root.join("missing").into_os_string()),
    );
    assert!(matches!(
        rt.probe_installation(&opts).await.unwrap(),
        InstallationSnapshot::Missing
    ));
    opts.env.insert("KAR_CLAUDE_BIN".into(), None);
    fs::write(root.join("first/version"), "1.2.3 (different program)").unwrap();
    fs::File::open(&primary)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    let snapshot = rt.probe_installation(&opts).await.unwrap();
    assert_eq!(snapshot.command(), Some(fallback.as_path()));
    assert!(!root.join("second/called-version").exists());
    opts.env.insert("PATH".into(), Some("".into()));
    fs::remove_file(&fallback).unwrap();
    assert!(matches!(
        rt.probe_installation(&opts).await.unwrap(),
        InstallationSnapshot::Missing
    ));
    opts.env.insert("PATH".into(), Some("first".into()));
    assert!(matches!(
        rt.probe_installation(&opts).await.unwrap(),
        InstallationSnapshot::Failed {
            code: InstallationIssue::UnrecognizedExecutable,
            ..
        }
    ));

    let q = Runtime::new("qoder", "qodercli", &[], Protocol::Qoder);
    opts.env.insert("KAR_QODER_BIN".into(), None);
    install(
        &root.join("first"),
        "qodercli",
        &source,
        "fixture",
        "A different application",
    );
    let older = install(
        &home.join(".local/bin"),
        "qodercli",
        &source,
        "Qoder CLI 1.1.77",
        "Usage: qodercli",
    );
    assert!(
        matches!(q.probe_installation(&opts).await.unwrap(),InstallationSnapshot::Failed{
        command,code:InstallationIssue::VersionUnsupported,installed_version:Some(ref v),..} if command==older && v=="1.1.77")
    );
    fs::write(home.join(".local/bin/version"), "Qoder CLI 1.1.78").unwrap();
    assert_eq!(
        q.probe_installation(&opts).await.unwrap().command(),
        Some(older.as_path())
    );
}
