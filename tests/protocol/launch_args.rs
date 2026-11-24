use super::*;

pub(super) async fn run() -> kar::Result<()> {
    for protocol in [Protocol::Codex, Protocol::Claude, Protocol::Acp] {
        let (temp, runtime, mut opts) = rig(protocol, ApprovalMode::Deny);
        let capture = temp.path().join("argv.json");
        opts.env
            .insert("KAR_TEST_SESSION_ARGV".into(), capture.clone().into());
        opts.launch_args = vec!["--fixture-label".into(), "one argument with spaces".into()];
        let session = runtime.open(opts.clone()).await?;
        let args: Vec<String> = serde_json::from_slice(&std::fs::read(&capture)?)?;
        let pos = args
            .windows(2)
            .position(|a| a == opts.launch_args)
            .expect("literal argv pair");
        assert_eq!(
            args.iter()
                .filter(|a| a.as_str() == "--fixture-label")
                .count(),
            1
        );
        if protocol == Protocol::Codex {
            assert_eq!(&args[pos + 2..], ["--listen", "stdio://"]);
        }
        session.close().await?;
        opts.launch_args.clear();
        let second = runtime.open(opts).await?;
        let args: Vec<String> = serde_json::from_slice(&std::fs::read(capture)?)?;
        assert!(!args.iter().any(|a| a == "--fixture-label"));
        second.close().await?;
    }
    let (temp, pi, mut opts) = rig(Protocol::Pi, ApprovalMode::Deny);
    let capture = temp.path().join("argv.json");
    opts.env
        .insert("KAR_TEST_SESSION_ARGV".into(), capture.clone().into());
    for allow in [false, true] {
        opts.settings.pi_allow_project_package_install = allow;
        pi.open(opts.clone()).await?.close().await?;
        let argv: Vec<String> = serde_json::from_slice(&std::fs::read(&capture)?)?;
        assert_eq!(argv.iter().any(|a| a == "--offline"), !allow, "{argv:?}");
    }
    for (id, protocol, flag) in [
        ("codex", Protocol::Codex, "--listen=private-marker"),
        ("claude", Protocol::Claude, "-rprivate-marker"),
        ("codebuddy", Protocol::Acp, "--systemPrompt=private-marker"),
        ("kimi", Protocol::KimiKap, "--host=private-marker"),
        ("qoder", Protocol::Qoder, "--remoteSession=private-marker"),
        ("factory", Protocol::Factory, "-sprivate-marker"),
        ("pi", Protocol::Pi, "--fixture-label"),
        ("dsh", Protocol::Dsh, "--fixture-label"),
    ] {
        let (temp, mut rt, mut opts) = rig(protocol, ApprovalMode::Deny);
        rt.id = id.into();
        let capture = temp.path().join("argv.json");
        opts.env
            .insert("KAR_TEST_SESSION_ARGV".into(), capture.clone().into());
        opts.launch_args.push(flag.into());
        let result = rt.open(opts).await;
        assert!(
            matches!(result,Err(Error::Unsupported(ref e)) if !e.contains("private-marker")),
            "{id} should reject an owned flag"
        );
        assert!(!capture.exists(), "{id} started a session before refusal");
    }
    Ok(())
}
