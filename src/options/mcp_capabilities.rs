use crate::{McpFirstModelRequest, McpStartup, McpTransport, Protocol};
use semver::Version;
use serde_json::Value;

pub(crate) const BOTH: &[McpTransport] = &[McpTransport::Stdio, McpTransport::Http];
const STDIO: &[McpTransport] = &[McpTransport::Stdio];

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct McpSupport {
    pub transports: Option<&'static [McpTransport]>,
    pub startup: Option<McpStartup>,
}
pub(crate) fn release(raw: &str) -> Option<Version> {
    raw.split_whitespace()
        .find_map(|v| Version::parse(v.trim_start_matches('v')).ok())
}

impl McpSupport {
    pub const PER_SERVER: McpStartup = McpStartup::PerServer {
        first_model_request: McpFirstModelRequest::MayStartPending,
    };

    pub fn new(protocol: Protocol, runtime: &str, version: Option<&str>) -> Self {
        Self::from_release(protocol, runtime, version.and_then(release).as_ref())
    }

    pub(crate) fn from_release(
        protocol: Protocol,
        runtime: &str,
        release: Option<&Version>,
    ) -> Self {
        let at_least =
            |major, minor, patch| release.is_some_and(|v| v >= &Version::new(major, minor, patch));
        let transports = match protocol {
            Protocol::Codex | Protocol::Claude | Protocol::Qoder | Protocol::KimiKap => Some(BOTH),
            Protocol::Factory if at_least(1, 12, 1) => Some(BOTH),
            Protocol::Dsh => Some(BOTH),
            Protocol::Cybion | Protocol::Morph => Some([].as_slice()),
            Protocol::Qwen if at_least(0, 26, 0) => Some(BOTH),
            Protocol::Qwen if release.is_some() => Some(STDIO),
            Protocol::Acp => match runtime {
                "dsh" | "cursor" | "antigravity" => Some(BOTH),
                "grok" if at_least(1, 0, 46) => Some(BOTH),
                "codebuddy" if at_least(2, 178, 1) => Some(BOTH),
                "kimi" if at_least(2, 1, 1) => Some(BOTH),
                "opencode"
                    if release.as_ref().is_some_and(|v| match v.major {
                        1 => at_least(1, 18, 30),
                        2 => at_least(2, 0, 26),
                        _ => false,
                    }) =>
                {
                    Some(BOTH)
                }
                _ => None,
            },
            // Pi RPC confirms its SDK during registration.
            _ => None,
        };
        let startup =
            (protocol == Protocol::Codex && at_least(0, 169, 0)).then_some(Self::PER_SERVER);
        Self {
            transports,
            startup,
        }
    }

    pub fn acp(runtime: &str, version: Option<&str>, hello: &Value) -> Self {
        let native = hello.pointer("/agentInfo/version").and_then(Value::as_str);
        let version = version
            .and_then(release)
            .or_else(|| native.and_then(release));
        let mut support = Self::from_release(Protocol::Acp, runtime, version.as_ref());
        if support.transports == Some(BOTH)
            && hello.pointer("/agentCapabilities/mcpCapabilities/http") != Some(&Value::Bool(true))
        {
            support.transports = Some(STDIO);
        }
        support
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transport_declarations_respect_provider_version_and_negotiated_http() {
        let hello = |v: &str, http: bool| json!({"agentInfo":{"version":v},"agentCapabilities":{"mcpCapabilities":{"http":http}}});
        for (id, old, new) in [
            ("grok", "1.0.45", "1.0.46"),
            ("codebuddy", "2.178.0", "2.178.1"),
            ("opencode", "1.18.29", "1.18.30"),
            ("opencode", "2.0.25", "2.0.26"),
            ("kimi", "2.1.0", "2.1.1"),
        ] {
            assert_eq!(
                McpSupport::acp(id, None, &hello(old, true)).transports,
                None
            );
            assert_eq!(
                McpSupport::acp(id, None, &hello(new, true)).transports,
                Some(BOTH)
            );
            assert_eq!(
                McpSupport::acp(id, Some(new), &hello("unknown", false)).transports,
                Some(STDIO)
            );
        }
        assert_eq!(
            McpSupport::acp("devin", None, &hello("1.0.0", true)).transports,
            None
        );
        for id in ["dsh", "cursor"] {
            assert_eq!(
                McpSupport::acp(id, None, &hello("1.0.0", true)).transports,
                Some(BOTH)
            );
            assert_eq!(
                McpSupport::acp(id, None, &hello("1.0.0", false)).transports,
                Some(STDIO)
            );
        }
        assert_eq!(
            McpSupport::new(Protocol::Dsh, "dsh", None).transports,
            Some(BOTH)
        );
        assert_eq!(
            McpSupport::new(Protocol::Acp, "cursor", None).transports,
            Some(BOTH)
        );
        assert_eq!(
            McpSupport::new(Protocol::Qwen, "qwen", Some("0.25.0")).transports,
            Some(STDIO)
        );
        assert_eq!(
            McpSupport::new(Protocol::Qwen, "qwen", Some("0.26.0")).transports,
            Some(BOTH)
        );
        assert_eq!(
            McpSupport::new(Protocol::Qwen, "qwen", None).transports,
            None
        );
        assert_eq!(
            McpSupport::new(Protocol::Codex, "codex", Some("codex-cli 0.169.0")).startup,
            Some(McpSupport::PER_SERVER)
        );
        assert!(
            McpSupport::new(Protocol::Codex, "codex", Some("codex-cli 0.168.0"))
                .startup
                .is_none()
        );
    }
}
