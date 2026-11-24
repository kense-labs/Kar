use super::ManualCompactionCapability as Compact;
use super::{
    CompactionEvents, ContextUsageCapability, SessionCapabilityDeclaration, SubagentResultSource,
    SubagentResults,
};
use crate::{
    InstallationSnapshot, Protocol, Runtime, TaskType,
    options::{availability, mcp_capabilities::McpSupport},
};
use semver::Version;

impl Runtime {
    /// Native resume semantics declared by this adapter, available without probing an installation.
    pub fn resumed_session_options(&self) -> Option<super::ResumedSessionOptions> {
        native_resume_options(self.protocol, &self.id)
    }

    /// Pure lookup on the result of `probe_installation`. Missing and failed installations return None.
    pub fn session_capabilities(
        &self,
        installation: &InstallationSnapshot,
    ) -> Option<SessionCapabilityDeclaration> {
        let InstallationSnapshot::Available { version, .. } = installation else {
            return None;
        };
        let version = version
            .as_deref()
            .and_then(crate::options::mcp_capabilities::release);
        let id = self.id.as_str();
        let report = |protocol| declaration(protocol, id, version.as_ref());
        let mut declaration = match self.protocol {
            Protocol::KimiKap => match &version {
                Some(v) if !crate::kimi::kap_candidate(v) => report(Protocol::Acp),
                // Some candidate installations expose only ACP. The capability handshake decides.
                _ => report(Protocol::KimiKap).intersection(report(Protocol::Acp)),
            },
            Protocol::Dsh => match &version {
                Some(v) if !crate::dsh::uses_web(v) => report(Protocol::Acp),
                Some(_) => report(Protocol::Dsh),
                None => report(Protocol::Dsh).intersection(report(Protocol::Acp)),
            },
            p => report(p),
        };
        // Runtime::fork has no Kimi route even when a new session negotiates ACP.
        if self.protocol == Protocol::KimiKap {
            declaration
                .refused_session_options
                .insert("fork", "native session fork with the requested scope");
        }
        Some(declaration)
    }
}

impl SessionCapabilityDeclaration {
    fn intersection(mut self, other: Self) -> Self {
        fn common<T: PartialEq>(a: Option<T>, b: Option<T>) -> Option<T> {
            a.filter(|v| Some(v) == b.as_ref())
        }
        self.refused_session_options
            .retain(|k, _| other.refused_session_options.contains_key(k));
        Self {
            resumed_session_options: common(
                self.resumed_session_options,
                other.resumed_session_options,
            ),
            steering: common(self.steering, other.steering),
            image_input: common(self.image_input, other.image_input),
            stop_task: common(self.stop_task, other.stop_task),
            manual_compaction: common(self.manual_compaction, other.manual_compaction),
            compaction_events: common(self.compaction_events, other.compaction_events),
            context_usage: common(self.context_usage, other.context_usage),
            side_question: common(self.side_question, other.side_question),
            file_changes: common(self.file_changes, other.file_changes),
            native_session_file: common(self.native_session_file, other.native_session_file),
            mcp_transports: common(self.mcp_transports, other.mcp_transports),
            mcp_startup: common(self.mcp_startup, other.mcp_startup),
            task_event_types: common(self.task_event_types, other.task_event_types),
            subagent_results: match (self.subagent_results, other.subagent_results) {
                (Some(a), Some(b)) => {
                    let r = SubagentResults {
                        foreground: common(a.foreground, b.foreground),
                        background: common(a.background, b.background),
                    };
                    (r != SubagentResults::default()).then_some(r)
                }
                _ => None,
            },
            refused_session_options: self.refused_session_options,
        }
    }
}

fn native_resume_options(protocol: Protocol, id: &str) -> Option<super::ResumedSessionOptions> {
    use Protocol::*;
    let adapter = match protocol {
        Codex => "codex",
        Claude => "claude",
        Pi => "pi",
        Qwen => "qwen",
        Dsh => "dsh",
        Acp if id == "dsh" => "dsh",
        _ => return None,
    };
    super::ResumedSessionOptions::for_runtime(adapter)
}

fn declaration(
    protocol: Protocol,
    id: &str,
    version: Option<&Version>,
) -> SessionCapabilityDeclaration {
    use Protocol::*;
    use SubagentResultSource::*;
    use TaskType::*;
    let at_least = |a, b, c| version.is_some_and(|v| v >= &Version::new(a, b, c));
    let major = version.map(|v| v.major);
    let mcp = McpSupport::from_release(protocol, id, version);
    let mut d = SessionCapabilityDeclaration {
        resumed_session_options: native_resume_options(protocol, id),
        steering: Some(!matches!(protocol, Qwen | Acp)),
        image_input: match protocol {
            Acp if id != "grok" => None,
            p => Some(!matches!(p, Qoder | Qwen | Morph)),
        },
        stop_task: Some(matches!(protocol, Codex | Claude | Qoder | KimiKap | Dsh)),
        manual_compaction: Some(
            if matches!(
                protocol,
                Codex | Claude | Pi | Qoder | KimiKap | Dsh | Factory
            ) {
                Compact::IDLE_ONLY
            } else {
                Compact::Unsupported {
                    reason: "no verified native manual compaction control on this adapter",
                }
            },
        ),
        mcp_transports: mcp.transports,
        mcp_startup: mcp.startup,
        refused_session_options: availability::refused_settings(protocol, id),
        ..Default::default()
    };
    if id == "dsh"
        && let Some(reason) = crate::dsh::directory_refusal(version)
    {
        d.refused_session_options
            .insert("additional_directories", reason);
    }
    if let Some(reason) = availability::credential_refusal(protocol, id) {
        d.refused_session_options
            .insert("mcp_server_credentials", reason);
    }
    if let Some(reason) = crate::options::launch_args::refusal(protocol, id) {
        d.refused_session_options.insert("launch_args", reason);
    }
    const COUNTS: ContextUsageCapability = ContextUsageCapability {
        tokens: true,
        context_window: true,
    };
    const TOKENS: ContextUsageCapability = ContextUsageCapability {
        tokens: true,
        context_window: false,
    };
    const NEITHER: ContextUsageCapability = ContextUsageCapability {
        tokens: false,
        context_window: false,
    };
    const LIFECYCLE: CompactionEvents = CompactionEvents {
        started: true,
        ended: true,
    };
    let results = |foreground, background| {
        Some(SubagentResults {
            foreground,
            background,
        })
    };
    match protocol {
        Codex => {
            d.file_changes = at_least(0, 160, 1).then_some(true);
            d.native_session_file = at_least(0, 169, 0).then_some(true);
            d.context_usage = at_least(0, 151, 0).then_some(COUNTS);
            if at_least(0, 158, 0) {
                d.task_event_types = Some(&[Agent]);
            }
            if at_least(0, 169, 0) {
                d.subagent_results = results(None, Some(ChildText));
            }
        }
        Claude => {
            d.context_usage = Some(COUNTS);
            if at_least(2, 1, 318) {
                d.task_event_types = Some(&[Agent, Shell, Workflow]);
            } else if at_least(2, 1, 292) {
                d.task_event_types = Some(&[Workflow]);
            }
            if at_least(2, 1, 326) {
                d.side_question = Some(true);
                d.subagent_results = results(Some(ParentToolResult), Some(ChildText));
            }
            if version.is_some() && !at_least(2, 1, 274) {
                d.refused_session_options.insert(
                    "model_requests",
                    "model_requests requires Claude Code 2.1.274 or later",
                );
            }
        }
        Qwen => {
            d.context_usage = at_least(0, 26, 0).then_some(COUNTS);
            if at_least(0, 26, 8) {
                d.task_event_types = Some(&[Agent]);
            }
            if at_least(0, 27, 4) {
                d.subagent_results = results(Some(ParentToolResult), None);
            }
            if version.is_some() && !at_least(0, 26, 0) {
                d.refused_session_options.insert(
                    "native_memory_default",
                    "native_memory_default requires verified Qwen Code 0.26.0 or later",
                );
            }
        }
        Qoder => {
            d.context_usage = Some(NEITHER);
            if at_least(1, 1, 83) {
                d.task_event_types = Some(&[Agent]);
            }
        }
        Factory | Pi => {
            d.context_usage = Some(COUNTS);
            d.task_event_types = Some(&[]);
            if protocol == Pi || at_least(1, 17, 2) {
                d.native_session_file = Some(true);
            }
        }
        Dsh | KimiKap => {
            d.compaction_events = Some(LIFECYCLE);
            d.context_usage = Some(COUNTS);
            d.task_event_types = Some(&[Agent, Shell]);
            if protocol == Dsh {
                d.native_session_file = Some(true);
                d.subagent_results = results(None, Some(ChildText));
            } else {
                d.subagent_results = results(Some(ParentToolResult), Some(ChildText));
            }
        }
        Acp => match id {
            "kimi" => {
                d.task_event_types = Some(&[]);
                d.context_usage = Some(COUNTS);
                if at_least(2, 6, 3) {
                    d.subagent_results = results(Some(ParentToolResult), None);
                }
            }
            "grok" => {
                d.context_usage = Some(TOKENS);
                d.steering = Some(true);
                d.manual_compaction = Some(Compact::IDLE_ONLY);
                if at_least(1, 0, 70) {
                    d.task_event_types = Some(&[Agent]);
                    d.subagent_results = results(Some(ParentToolResult), Some(TaskOutput));
                }
            }
            "antigravity" => {
                d.task_event_types = Some(&[]);
                d.steering = None;
                d.context_usage = Some(COUNTS);
            }
            "devin" => d.task_event_types = Some(&[]),
            "codebuddy" => {
                d.task_event_types = Some(&[]);
                d.context_usage = Some(NEITHER);
            }
            "dsh" => {
                d.native_session_file = Some(false);
                if version.is_some_and(|v| v < &Version::parse("0.2.1-rc.5").unwrap()) {
                    d.refused_session_options.insert(
                        "session_log_upload",
                        "session_log_upload requires DeepSeek Harness 0.2.1-rc.5 or later",
                    );
                }
            }
            "opencode" => {
                d.steering = (major == Some(1)).then_some(true);
                d.manual_compaction = match major {
                    Some(1) => Some(Compact::Unsupported {
                        reason: "OpenCode v1 does not report structured completion of manual compaction",
                    }),
                    Some(2) => Some(Compact::IDLE_ONLY),
                    _ => None,
                };
                if matches!(major, Some(1 | 2)) {
                    d.context_usage = Some(COUNTS);
                }
                if major == Some(2) && at_least(2, 3, 0) {
                    d.task_event_types = Some(&[Agent]);
                    d.subagent_results = results(None, Some(ChildText));
                } else if matches!(major, Some(1 | 2)) {
                    d.task_event_types = Some(&[]);
                    if at_least(2, 2, 7) || (major == Some(1) && at_least(1, 19, 5)) {
                        d.subagent_results = results(Some(ParentToolResult), None);
                    }
                }
                if major == Some(2) && !at_least(2, 2, 0) {
                    d.refused_session_options.insert(
                        "system_prompt",
                        "OpenCode v2 base instructions require 2.2.0 or later",
                    );
                }
                if major == Some(2) && !at_least(2, 0, 30) {
                    d.refused_session_options.insert(
                        "append_system_prompt",
                        "OpenCode v2 append requires 2.0.30 or later",
                    );
                }
            }
            _ => {}
        },
        Morph | Cybion => {
            d.task_event_types = Some(&[]);
            let reason = protocol
                .child_environment_refusal()
                .expect("remote worker owns env");
            d.refused_session_options.insert("env", reason);
            d.refused_session_options.insert("mcp_servers", reason);
        }
    }
    if matches!(protocol, Qwen | Qoder | Factory | Morph | Cybion | KimiKap) {
        d.refused_session_options
            .insert("fork", "native session fork with the requested scope");
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstallationIssue, InstallationSource};
    use serde_json::{Value, json};

    fn available(version: Option<&str>) -> InstallationSnapshot {
        InstallationSnapshot::Available {
            via: InstallationSource::Executable,
            command: Some("/unused/agent".into()),
            version: version.map(str::to_owned),
            shadowed: vec![],
        }
    }
    fn report(id: &str, version: Option<&str>) -> Value {
        let rt = crate::runtime(id).unwrap();
        serde_json::to_value(rt.session_capabilities(&available(version)).unwrap()).unwrap()
    }

    #[test]
    fn declares_provided_and_omitted_semantics_for_each_native_prompt() {
        let fresh = json!({"provided":"immediate","omitted":"default"});
        let saved = json!({"provided":"immediate","omitted":"saved"});
        let compact = json!({"provided":"next_compaction","omitted":"saved_until_compaction"});
        for (id, base, append) in [
            ("qwen", &fresh, &fresh),
            ("pi", &fresh, &fresh),
            ("dsh", &fresh, &fresh),
            ("codex", &saved, &compact),
            ("claude", &compact, &compact),
        ] {
            let rt = crate::runtime(id).unwrap();
            let expected = json!({"system_prompt":base,"append_system_prompt":append});
            assert_eq!(
                serde_json::to_value(rt.resumed_session_options()).unwrap(),
                expected
            );
            assert_eq!(report(id, None)["resumed_session_options"], expected);
        }
        assert!(
            crate::runtime("factory")
                .unwrap()
                .resumed_session_options()
                .is_none()
        );
        assert!(
            report("factory", None)
                .get("resumed_session_options")
                .is_none()
        );
        let browser = serde_json::to_value(SessionCapabilityDeclaration::pi_durable()).unwrap();
        assert_eq!(
            browser["resumed_session_options"],
            json!({"append_system_prompt":saved})
        );
    }

    #[test]
    fn missing_and_failed_installations_do_not_publish_declarations() {
        let rt = crate::runtime("claude").unwrap();
        assert!(
            rt.session_capabilities(&InstallationSnapshot::Missing)
                .is_none()
        );
        let failed = InstallationSnapshot::Failed {
            command: "/unused/agent".into(),
            code: InstallationIssue::VersionUnsupported,
            reason: String::new(),
            installed_version: Some("2.1.330".into()),
            required_version: None,
        };
        assert!(rt.session_capabilities(&failed).is_none());
    }
    #[test]
    fn known_versions_and_unknown_versions_keep_distinct_evidence() {
        let old = report("codex", Some("codex-cli 0.160.0"));
        assert!(old.get("file_changes").is_none());
        let new = report("codex", Some("codex-cli 0.171.0-alpha.13"));
        assert_eq!(new["file_changes"], true);
        assert_eq!(new["mcp_startup"]["kind"], "per_server");
        assert_eq!(new["subagent_results"]["background"], "child_text");
        assert!(
            report("claude", Some("2.1.325 (Claude Code)"))
                .get("side_question")
                .is_none()
        );
        assert_eq!(
            report("claude", Some("2.1.326 (Claude Code)"))["side_question"],
            true
        );
        let old = report("qwen", Some("0.25.9"));
        assert_eq!(old["mcp_transports"], json!(["stdio"]));
        assert!(old["refused_session_options"]["native_memory_default"].is_string());
        let new = report("qwen", Some("v0.27.4"));
        assert_eq!(new["mcp_transports"], json!(["stdio", "http"]));
        assert!(
            new["refused_session_options"]
                .get("native_memory_default")
                .is_none()
        );
        assert!(report("opencode", None).get("steering").is_none());
        assert_eq!(
            report("opencode", Some("opencode 1.19.5"))["steering"],
            true
        );
        assert!(report("opencode", Some("2.3.2")).get("steering").is_none());
        assert_eq!(report("morph", None)["mcp_transports"], json!([]));
        assert!(report("pi", None).get("mcp_transports").is_none());
    }
    #[test]
    fn fallback_transports_publish_only_what_both_paths_establish() {
        let legacy = report("kimi", Some("kimi 2.7.2"));
        assert_eq!(legacy["stop_task"], false);
        assert!(legacy["refused_session_options"]["append_system_prompt"].is_string());
        let new = report("kimi", Some("2.7.2-botiverse.1"));
        for key in ["stop_task", "steering", "manual_compaction", "image_input"] {
            assert!(new.get(key).is_none(), "{key}");
        }
        assert_eq!(
            new["subagent_results"],
            json!({"foreground":"parent_tool_result"})
        );
        assert!(
            new["refused_session_options"]
                .get("append_system_prompt")
                .is_none()
        );
        assert_eq!(new["mcp_transports"], json!(["stdio", "http"]));
        let old = report("dsh", Some("0.2.1-rc.5"));
        let web = report("dsh", Some("0.2.2-rc.5"));
        assert!(old["refused_session_options"]["additional_directories"].is_string());
        assert_eq!(
            web["refused_session_options"]["additional_directories"].is_null(),
            cfg!(any(target_os = "macos", target_os = "linux"))
        );
        for dsh in [&old, &web] {
            assert!(dsh["refused_session_options"].get("name").is_none());
            assert!(
                dsh["refused_session_options"]
                    .get("system_prompt")
                    .is_none()
            );
            assert!(
                dsh["refused_session_options"]
                    .get("append_system_prompt")
                    .is_none()
            );
        }
        assert_eq!(old["native_session_file"], false);
        assert_eq!(web["native_session_file"], true);
        assert_eq!(old["stop_task"], false);
        assert_eq!(web["stop_task"], true);
        assert!(report("dsh", None).get("stop_task").is_none());
    }
}
