use super::Case;

fn scenario(target: &str, test: &str, args: &[&str], requires: &str) -> Case {
    Case {
        runtime: "workbench".into(),
        target: target.into(),
        test: test.into(),
        environment: Vec::new(),
        requires: requires.into(),
        program: "cargo".into(),
        args: ["test", "--locked"]
            .into_iter()
            .chain(args.iter().copied())
            .map(String::from)
            .collect(),
        libtest: false,
        switches: Default::default(),
    }
}

pub(super) fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut local = scenario(
        "workbench_acceptance",
        "local-workbench",
        &[
            "--test",
            "workbench_git",
            "--test",
            "workbench_terminal_preview",
            "--test",
            "workbench_coediting",
            "--test",
            "workbench_remote_access",
            "--test",
            "workbench_workflows",
            "--test",
            "workbench_forge",
            "--test",
            "workbench_issue_accounts",
            "--test",
            "workbench_mcp",
            "--test",
            "workbench_handoff",
            "--test",
            "workbench_agent_sessions",
            "--test",
            "workbench_acceptance",
        ],
        "macOS, actual Kar, local Git repositories, PTY and Kantor files; provider and hosting protocol fixtures",
    );
    local.switches.insert("KAR_HANDOFF_LIVE".into(), "0".into());
    cases.push(local);
    for (mode, requires) in [
        ("workbench-suggestions", "two independent subscribers, Claude push and Codex sampling protocol fixtures"),
        ("workbench-shell","two independent HTTP clients, persisted directory cursor, activity, snooze expiry and restart"),
        (
            "workbench-native-child-work",
            "isolated Antigravity ACP protocol fixture, SQLite/protobuf records and detached inbox; historical forks and rejection of unrelated batches",
        ),
        (
            "workbench-usage",
            "isolated HOME, native-format usage records and a local HTTP price source",
        ),
        (
            "workbench-observability",
            "macOS system CPU, memory, isolated child PTY signals and two HTTP subscribers",
        ),
        (
            "workbench-projects",
            "two HTTP clients and scoped projects with a Codex protocol fixture",
        ),
        (
            "workbench-devices",
            "device protocol fixture, simulated Xcode tools and chat peer on local HTTP",
        ),
        (
            "workbench-queue",
            "workbench restart with durable inputs, native protocol fixture",
        ),
        (
            "workbench-fork",
            "actual local Git and a Codex protocol fixture across HTTP restarts",
        ),
        (
            "fork-with-id",
            "durable fork crash boundaries using the native protocol fixture",
        ),
        (
            "workbench-requests",
            "concurrent workbench answers and reused native request IDs after restart, protocol fixture",
        ),
    ] {
        cases.push(scenario(
            "chat_restart",
            mode,
            &["--test", "chat_restart", "--", mode],
            requires,
        ));
    }
    cases.push(scenario("kar-documents","office-engine-and-store",&["--manifest-path","crates/kar-documents/Cargo.toml"],
        "actual .docx, .xlsx and .pptx fixtures, pinned Kantor engines, local document store and HTTP clients"));
    for (target, test, requires, env) in [
        (
            "workbench_coediting",
            "native_document_edit::actual_codex_merges_a_native_file_edit_with_a_concurrent_owner_save",
            "installed Codex, loopback model, actual file tool in a private checkout",
            &["KAR_CODEX_BIN"][..],
        ),
        (
            "workbench_terminal_preview",
            "chrome_loads_project_preview",
            "installed macOS Google Chrome (default Applications path or KAR_TEST_CHROME), real PTY and local HTTP",
            &["KAR_TEST_CHROME"][..],
        ),
    ] {
        let mut case = scenario(
            target,
            test,
            &[
                "--test",
                target,
                test,
                "--",
                "--exact",
                "--ignored",
                "--nocapture",
            ],
            requires,
        );
        case.libtest = true;
        case.environment = env.iter().map(|s| s.to_string()).collect();
        cases.push(case);
    }
    let mut cloud = scenario(
        "lib",
        "cloud-device-protocol",
        &["--lib", "workbench::devices::cloud::tests"],
        "local HTTP cloud provider protocol fixture",
    );
    cloud.libtest = true;
    cases.push(cloud);
    for (test, conditions) in [
        (
            "codex_tiers_require_the_same_model_session_and_native_turn",
            "native-format usage attribution across model, session and native turn",
        ),
        (
            "usage_skips_large_prompt_and_keeps_an_unterminated_complete_row",
            "offline usage scan with a large prompt and an unterminated final row",
        ),
        (
            "kar_usage_is_scoped_by_open_child_and_handoff",
            "Kar usage from parent, child and handed-off controller streams",
        ),
        (
            "cursor_account_usage_counts_paid_occurrences_across_overlapping_pages",
            "local Cursor Dashboard protocol peer with overlapping billing pages",
        ),
    ] {
        let mut usage = scenario("lib", test, &["--lib", test], conditions);
        usage.libtest = true;
        cases.push(usage);
    }
    for (test, env, conditions) in [
        (
            "workbench-native-child-work-live",
            &[
                "KAR_CHILD_WORK_URL",
                "KAR_WORKBENCH_TOKEN",
                "KAR_CHILD_WORK_PROJECT",
                "KAR_CHILD_WORK_CHAT",
                "KAR_CHILD_WORK_TURN",
                "KAR_CHILD_WORK_TOOL",
                "KAR_CHILD_WORK_EXPECTED",
            ][..],
            "read-only HTTP on an existing loopback Kar workbench with recorded native child IDs (comma separated); the provider is not invoked",
        ),
        (
            "workbench-usage-live",
            &["KAR_USAGE_LIVE_PROVIDER", "KAR_USAGE_LIVE_SINCE_DAY"][..],
            "read existing provider usage; Cursor requires CURSOR_API_KEY, optional KAR_USAGE_LIVE_UNTIL_DAY and KAR_USAGE_LIVE_EXPECT_TOKENS",
        ),
        (
            "workbench-simlock-live",
            &[
                "KAR_SIMLOCK_LIVE_GATEWAY",
                "KAR_SIMLOCK_LIVE_TOKEN",
                "KAR_SIMLOCK_LIVE_CATALOG",
            ][..],
            "macOS, dedicated SimLock gateway, credential and device catalog; real cloud device, protocol chat fixture",
        ),
        (
            "workbench-expo-live",
            &[
                "KAR_EXPO_LIVE_ALLOW_BILLING",
                "KAR_EXPO_LIVE_APP",
                "EXPO_TOKEN",
            ][..],
            "macOS, explicitly enabled billable Expo device, dedicated app and authenticated account; real cloud device, protocol chat fixture",
        ),
    ] {
        let mut cloud = scenario(
            "chat_restart",
            test,
            &["--test", "chat_restart", "--", test],
            conditions,
        );
        cloud.environment = env.iter().map(|key| key.to_string()).collect();
        cases.push(cloud);
    }
    let mut devices = scenario(
        "chat_restart",
        "workbench-devices-live",
        &["--test", "chat_restart", "--", "workbench-devices-live"],
        "macOS, installed Xcode and a dedicated booted iOS simulator UDID for real device state, settings and screenshot; chat is a protocol fixture",
    );
    devices.environment = vec!["KAR_DEVICE_LIVE_ID".into()];
    cases.push(devices);
    let mut live = scenario(
        "workbench_handoff",
        "installed-codex-qwen",
        &["--test", "workbench_handoff"],
        "installed Codex >=0.143.0 and Qwen, authenticated models and permission to read the disposable workspace and fork the recorded Codex turn",
    );
    live.environment = ["KAR_CODEX_BIN", "KAR_QWEN_BIN"].map(String::from).into();
    live.switches.insert("KAR_HANDOFF_LIVE".into(), "1".into());
    live.switches
        .insert("KAR_HANDOFF_FORK_LIVE".into(), "1".into());
    cases.push(live);
    let mut hosted = scenario(
        "workbench_forge",
        "hosted-github",
        &["--test", "workbench_forge", "--", "hosted-github"],
        "explicitly selected disposable GitHub repository, authorized push and PR credentials; leaves its test branch and closed PR",
    );
    hosted.environment = ["KAR_FORGE_TEST_REPO", "GH_TOKEN"].map(String::from).into();
    cases.push(hosted);
    for mode in ["hosted-issue-actions", "hosted-review-actions"] {
        let mut case = scenario(
            "workbench_forge",
            mode,
            &["--test", "workbench_forge", "--", mode],
            "explicit disposable native forge/issue team, selected credentials and expected success, permission or unsupported; creates marked tracker objects or reviews",
        );
        case.environment = [
            "KAR_FORGE_ACTION_DRIVER",
            "KAR_FORGE_ACTION_SCOPE",
            "KAR_FORGE_ACTION_REMOTE",
            "KAR_FORGE_ACTION_EXPECT",
        ]
        .map(String::from)
        .into();
        if mode == "hosted-review-actions" {
            case.environment.push("KAR_FORGE_ACTION_PR".into());
        }
        cases.push(case);
    }
    let mut chain = scenario(
        "workbench_acceptance",
        "hosted-codex-qwen",
        &["--test", "workbench_acceptance", "--", "hosted-codex-qwen"],
        "macOS, dedicated GitHub repo and existing read-only source PR, Git push credentials and signed-in Codex/Qwen, native document tools and PTY; retains its own branch and closes its marked PR",
    );
    chain.environment = [
        "KAR_FORGE_TEST_REPO",
        "KAR_FORGE_SOURCE_PR",
        "GH_TOKEN",
        "KAR_CODEX_BIN",
        "KAR_QWEN_BIN",
    ]
    .map(String::from)
    .into();
    cases.push(chain);
    cases
}
