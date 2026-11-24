//! Display projections over ordered tool calls. Grouping never parses tool arguments.
use super::{Agent, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActionKind {
    RunCommand,
    ReadFile,
    EditFile,
    Search,
    Web,
    Fetch,
    Subagent,
    Mcp,
    Wait,
    Other,
}

impl ToolActionKind {
    pub fn classify(runtime: &str, tool: &str) -> Self {
        use ToolActionKind::*;
        let runtime = runtime.strip_suffix("-aimock").unwrap_or(runtime);
        match (runtime, tool) {
            ("claude", "Bash" | "PowerShell")
            | ("kimi" | "codebuddy" | "qoder", "Bash")
            | ("pi" | "opencode" | "dsh", "bash")
            | ("opencode" | "cursor", "shell")
            | ("codex", "commandExecution")
            | ("grok", "run_terminal_command")
            | ("antigravity", "run_command")
            | ("qwen", "run_shell_command")
            | ("factory", "Execute") => RunCommand,
            ("claude" | "kimi" | "qoder" | "codebuddy" | "factory", "Read")
            | ("pi" | "cursor", "read" | "ls")
            | ("dsh" | "opencode", "read")
            | ("grok" | "qwen" | "antigravity", "read_file") => ReadFile,
            ("claude", "Edit" | "Write" | "NotebookEdit")
            | ("kimi", "Edit" | "Write")
            | ("factory", "Create" | "Edit")
            | ("qoder" | "codebuddy", "Edit")
            | ("codex", "fileChange")
            | ("grok", "search_replace")
            | ("antigravity", "edit_file")
            | ("dsh" | "qwen", "edit")
            | ("pi" | "opencode", "edit" | "write")
            | ("cursor", "edit" | "write" | "delete") => EditFile,
            ("claude" | "kimi" | "factory", "Grep" | "Glob")
            | ("qoder" | "codebuddy", "Grep")
            | ("qwen", "grep_search")
            | ("pi", "grep" | "find")
            | ("grok" | "dsh", "grep")
            | ("opencode", "grep" | "glob")
            | ("cursor", "grep" | "glob" | "semSearch") => Search,
            ("claude" | "kimi", "WebSearch") | ("cursor" | "codex", "webSearch") => Web,
            ("claude", "WebFetch") | ("kimi", "FetchURL") | ("cursor", "webFetch") => Fetch,
            ("claude", "Agent" | "Task")
            | ("kimi", "Agent")
            | ("opencode", "subagent")
            | ("codex", "collabAgentToolCall" | "collabToolCall") => Subagent,
            ("codex", "sleep") | ("kimi", "WaitFor") => Wait,
            ("codex", "mcpToolCall") | ("cursor", "mcp") | ("grok", "use_tool") => Mcp,
            (_, name) if name.starts_with("mcp__") => Mcp,
            ("antigravity", name)
                if name
                    .strip_prefix("mcp_")
                    .and_then(|rest| rest.split_once('_'))
                    .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty()) =>
            {
                Mcp
            }
            ("factory", name)
                if name.contains("___")
                    && !name.starts_with("___")
                    && !name.ends_with("___")
                    && !name.contains(['\r', '\n']) =>
            {
                Mcp
            }
            _ => Other,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolAction {
    pub kind: ToolActionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
}
impl Tool {
    fn classification_name(&self) -> &str {
        self.native_type
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("")
    }
    pub fn action(&self, runtime: &str) -> ToolAction {
        use ToolActionKind::*;
        let runtime = runtime.strip_suffix("-aimock").unwrap_or(runtime);
        let name = self.classification_name();
        let mut kind = ToolActionKind::classify(runtime, name);
        let input = self
            .input
            .as_ref()
            .filter(|_| !self.input_partial)
            .unwrap_or(&Value::Null);
        // Imported Voyage arguments may be serialized JSON, whereas native Rust events preserve values.
        let parsed = input
            .as_str()
            .filter(|s| s.trim_start().starts_with(['{', '[']))
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let body = parsed.as_ref().unwrap_or(input);
        let field = |k: &str| {
            body[k]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        if runtime == "qwen"
            && name == "tool_call"
            && body["name"]
                .as_str()
                .is_some_and(|s| s.starts_with("mcp__"))
        {
            kind = Mcp;
        }
        let mut paths = Vec::new();
        if kind == ReadFile || kind == EditFile {
            match (runtime, name) {
                ("codex", "fileChange") => {
                    let mut seen = std::collections::HashSet::new();
                    for change in body.as_array().into_iter().flatten() {
                        for path in [
                            change["path"].as_str(),
                            change["kind"]["move_path"]
                                .as_str()
                                .or(change["kind"]["movePath"].as_str()),
                        ]
                        .into_iter()
                        .flatten()
                        {
                            if !path.is_empty() && seen.insert(path) {
                                paths.push(path.to_owned());
                            }
                        }
                    }
                }
                ("claude", "NotebookEdit") => paths.extend(field("notebook_path")),
                ("antigravity", _) => paths.extend(field("Path")),
                ("grok", "read_file") => paths.extend(field("target_file")),
                _ => paths.extend(
                    ["path", "file_path", "filePath", "file"]
                        .into_iter()
                        .find_map(field),
                ),
            }
        }
        let command = match (kind, runtime) {
            (RunCommand, "codex") if !body.is_object() => {
                input.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
            }
            (RunCommand, "antigravity") => field("CommandLine"),
            (RunCommand, _) => field("command"),
            _ => None,
        };
        let description = match (kind, runtime) {
            (Subagent, "claude" | "kimi" | "opencode")
            | (RunCommand, "claude" | "grok" | "dsh") => field("description"),
            (RunCommand, "antigravity") => field("toolSummary"),
            (RunCommand, "factory") => field("summary"),
            _ => None,
        };
        let mut detail = paths.first().cloned();
        if detail.is_none() && kind == Search {
            detail = field("pattern").or_else(|| field("query"));
        }
        let detail = detail
            .or_else(|| {
                [
                    "command",
                    "cmd",
                    "path",
                    "file_path",
                    "filePath",
                    "file",
                    "pattern",
                    "query",
                    "url",
                ]
                .into_iter()
                .find_map(|k| {
                    field(k).or_else(|| {
                        body[k]
                            .as_array()?
                            .last()?
                            .as_str()
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned)
                    })
                })
            })
            .or_else(|| command.clone())
            .or_else(|| (kind == Subagent).then(|| description.clone()).flatten())
            .or_else(|| (kind == Other && !name.is_empty()).then(|| name.to_owned()));
        let duration_ms = (kind == Wait && runtime == "codex")
            .then(|| body["durationMs"].as_f64())
            .flatten()
            .filter(|ms| *ms > 0.0);
        ToolAction {
            kind,
            detail,
            paths,
            command,
            description,
            duration_ms,
        }
    }
}

pub enum ToolGroupPart<'a> {
    Tool { id: &'a str, tool: &'a Tool },
    Reasoning,
    Other,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCount {
    pub kind: ToolActionKind,
    pub count: usize,
}
/// A range into the original ordered items. Indices preserve every ungrouped item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolGroupSegment {
    Part {
        index: usize,
    },
    Tools {
        index: usize,
        len: usize,
        counts: Vec<ToolCount>,
        failed: usize,
        running: Option<String>,
    },
}
pub fn group_tool_activity<'a>(
    runtime: &str,
    parts: impl IntoIterator<Item = (&'a Agent, ToolGroupPart<'a>)>,
) -> Vec<ToolGroupSegment> {
    use ToolGroupSegment::*;
    let mut segments = vec![];
    let mut previous = None;
    for (i, (agent, part)) in parts.into_iter().enumerate() {
        if matches!(part, ToolGroupPart::Other) {
            segments.push(Part { index: i });
            previous = None;
            continue;
        }
        if previous != Some(agent) {
            segments.push(Tools {
                index: i,
                len: 0,
                counts: vec![],
                failed: 0,
                running: None,
            });
            previous = Some(agent);
        }
        let Some(Tools {
            len,
            counts,
            failed,
            running,
            ..
        }) = segments.last_mut()
        else {
            unreachable!()
        };
        *len += 1;
        if let ToolGroupPart::Tool { id, tool } = part {
            let kind = ToolActionKind::classify(runtime, tool.classification_name());
            if let Some(row) = counts.iter_mut().find(|c| c.kind == kind) {
                row.count += 1;
            } else {
                counts.push(ToolCount { kind, count: 1 });
            }
            if tool
                .result
                .as_ref()
                .is_some_and(|r| r.status == Some(crate::ToolStatus::Failed))
            {
                *failed += 1;
            }
            if !tool.finished {
                *running = Some(id.into());
            }
        }
    }
    segments
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActionState {
    Writing,
    Running,
    Done,
    Failed,
}
impl ToolActionKind {
    pub fn label(self, state: ToolActionState) -> &'static str {
        use ToolActionKind::*;
        let labels = match self {
            RunCommand => [
                "Preparing command",
                "Running command",
                "Ran command",
                "Command failed",
            ],
            ReadFile => [
                "Preparing file read",
                "Reading file",
                "Read file",
                "Read failed",
            ],
            EditFile => [
                "Preparing file edit",
                "Editing file",
                "Edited file",
                "Edit failed",
            ],
            Search => ["Preparing search", "Searching", "Searched", "Search failed"],
            Web => [
                "Preparing web search",
                "Searching the web",
                "Searched the web",
                "Web search failed",
            ],
            Fetch => [
                "Preparing page fetch",
                "Fetching page",
                "Fetched page",
                "Fetch failed",
            ],
            Subagent => [
                "Preparing subagent",
                "Running subagent",
                "Ran subagent",
                "Subagent failed",
            ],
            Mcp => [
                "Preparing a tool call",
                "Using a tool",
                "Used a tool",
                "Tool failed",
            ],
            Wait => ["Preparing to wait", "Waiting", "Waited", "Wait failed"],
            Other => ["Preparing", "Working", "Done", "Failed"],
        };
        match state {
            ToolActionState::Writing => labels[0],
            ToolActionState::Running => labels[1],
            ToolActionState::Done => labels[2],
            ToolActionState::Failed => labels[3],
        }
    }
}
/// English presentation of the ordered counts. Hosts can translate the counts directly.
pub fn tool_group_summary(counts: &[ToolCount], running: bool) -> String {
    use ToolActionKind::*;
    if counts.is_empty() {
        return if running { "Thinking…" } else { "Thought" }.into();
    }
    let tools: usize = counts
        .iter()
        .filter(|c| matches!(c.kind, Mcp | Other))
        .map(|c| c.count)
        .sum();
    let mut tools_said = false;
    let mut phrases = Vec::new();
    for row in counts {
        let n = row.count;
        let phrase = match row.kind {
            RunCommand if n == 1 => "ran a command".into(),
            RunCommand => format!("ran {n} commands"),
            ReadFile if n == 1 => "read a file".into(),
            ReadFile => format!("read {n} files"),
            EditFile if n == 1 => "edited a file".into(),
            EditFile => format!("edited {n} files"),
            Search if n == 1 => "searched".into(),
            Search => format!("searched {n} times"),
            Web if n == 1 => "searched the web".into(),
            Web => format!("searched the web {n} times"),
            Fetch if n == 1 => "fetched a page".into(),
            Fetch => format!("fetched {n} pages"),
            Subagent if n == 1 => "ran a subagent".into(),
            Subagent => format!("ran {n} subagents"),
            Wait if n == 1 => "waited".into(),
            Wait => format!("waited {n} times"),
            Mcp | Other if tools_said => continue,
            Mcp | Other => {
                tools_said = true;
                if tools == 1 {
                    "used a tool".into()
                } else {
                    format!("used {tools} tools")
                }
            }
        };
        phrases.push(phrase);
    }
    let mut text = phrases.join(", ");
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn tool(name: &str, input: Value) -> Tool {
        Tool {
            name: Some(name.into()),
            input: Some(input),
            ..Default::default()
        }
    }
    #[test]
    fn classification_and_groups_use_native_names_and_separate_agents() {
        let root = Agent {
            session_id: Some("s".into()),
            path: vec![],
        };
        let child = Agent {
            path: vec!["t".into()],
            ..root.clone()
        };
        let edit = tool(
            "fileChange",
            json!([{"path":"a","kind":{"move_path":"b"}},{"path":"b"},{"path":"c"}]),
        );
        let action = edit.action("codex");
        assert_eq!(action.paths, ["a", "b", "c"]);
        assert_eq!(action.detail.as_deref(), Some("a"));
        let custom = Tool {
            name: Some("custom".into()),
            native_type: Some("mcpToolCall".into()),
            ..Default::default()
        };
        let input = tool("tool_call", json!({"name":"mcp__x__tool"}));
        assert_eq!(input.action("qwen").kind, ToolActionKind::Mcp);
        assert_eq!(
            Tool {
                input_partial: true,
                ..input
            }
            .action("qwen")
            .kind,
            ToolActionKind::Other
        );
        let call = |id, t| ToolGroupPart::Tool { id, tool: t };
        let groups = group_tool_activity(
            "codex",
            [
                (&root, ToolGroupPart::Other),
                (&root, call("a", &edit)),
                (&root, ToolGroupPart::Reasoning),
                (&root, call("m", &custom)),
                (&child, call("b", &edit)),
                (&root, ToolGroupPart::Reasoning),
            ],
        );
        assert_eq!(groups.len(), 4);
        let ToolGroupSegment::Tools {
            index,
            len,
            counts,
            running,
            ..
        } = &groups[1]
        else {
            panic!()
        };
        assert_eq!((*index, *len, running.as_deref()), (1, 3, Some("m")));
        assert_eq!(
            tool_group_summary(counts, false),
            "Edited a file, used a tool"
        );
        assert_eq!(tool_group_summary(&[], true), "Thinking…");
    }
}
