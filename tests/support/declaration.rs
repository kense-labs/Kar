use kar::{Capabilities, Runtime, SessionCapabilityDeclaration, SessionOptions};

pub async fn for_installation(rt: &Runtime, opts: &SessionOptions) -> SessionCapabilityDeclaration {
    let installation = rt
        .probe_installation(&opts.installation_options())
        .await
        .unwrap();
    rt.session_capabilities(&installation)
        .expect("available native installation")
}

pub fn check(declared: &SessionCapabilityDeclaration, actual: Capabilities) {
    for (name, known, observed) in [
        ("steering", declared.steering, actual.steering),
        ("image_input", declared.image_input, actual.image_input),
        ("stop_task", declared.stop_task, actual.stop_task),
        (
            "manual_compaction",
            declared.manual_compaction.map(|c| c.is_supported()),
            actual.manual_compaction,
        ),
        (
            "native_session_file",
            declared.native_session_file,
            actual.native_session_file,
        ),
    ] {
        if let Some(expected) = known {
            assert_eq!(
                expected, observed,
                "pre-open {name} for {:?}",
                actual.protocol
            );
        }
    }
    if let Some(transports) = declared.mcp_transports {
        assert_eq!(
            Some(transports),
            actual.mcp_transports,
            "pre-open MCP transports for {:?}",
            actual.protocol
        );
    }
    if let Some(startup) = declared.mcp_startup {
        assert_eq!(Some(startup), actual.mcp_startup);
    }
}
