//! Cursor ACP spawns background shells in detached process groups on macOS.
//! Track only shells proven to belong to this session's `agent acp` process tree.
use crate::{
    BackgroundKind, BackgroundWork, Error, Event, Protocol, Result, SessionOptions, TaskChange,
    TaskStatus, resources::tree::ProcessId,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Read,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Tracker {
    root: String,
    process: Option<ProcessId>,
    cursor_data_dir: Option<PathBuf>,
    workspace_slug: String,
    ignored_leaders: BTreeSet<ProcessId>,
    launches: HashMap<String, Launch>,
    shells: BTreeMap<String, Shell>,
    secrets: Vec<String>,
    next_order: usize,
}

struct Launch {
    turn: Uuid,
    command: Option<String>,
    description: Option<String>,
    started_ms: u64,
}

struct Shell {
    process: Option<ProcessId>,
    tool_call_id: Option<String>,
    terminal_path: Option<PathBuf>,
    ended: bool,
    stop_sent: bool,
}

#[derive(Default)]
struct Receipt {
    shell_id: String,
    pid: Option<u32>,
    terminal_path: Option<PathBuf>,
    command: Option<String>,
    description: Option<String>,
}

#[derive(Default)]
struct TerminalMeta {
    pid: Option<u32>,
    status: Option<String>,
    exit_code: Option<i32>,
    started_at: Option<String>,
    command: Option<String>,
    path: Option<PathBuf>,
}

fn workspace_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let mut slug = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

fn cursor_data_dir(opts: &SessionOptions) -> Option<PathBuf> {
    let dir = crate::options::env_path(opts, "CURSOR_DATA_DIR")
        .or_else(|| opts.session_home().map(|h| h.join(".cursor")))?;
    let joined = opts.cwd.join(dir);
    Some(joined.canonicalize().unwrap_or(joined))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Tracker {
    pub(super) fn for_runtime(
        protocol: Protocol,
        id: &str,
        process: Option<ProcessId>,
        opts: &SessionOptions,
        secrets: &[String],
    ) -> Option<Self> {
        if !(cfg!(target_os = "macos") && protocol == Protocol::Acp && id == "cursor") {
            return None;
        }
        Some(Self {
            root: String::new(),
            process,
            cursor_data_dir: cursor_data_dir(opts),
            workspace_slug: workspace_slug(&opts.cwd),
            ignored_leaders: detached_leaders(process),
            launches: HashMap::new(),
            shells: BTreeMap::new(),
            secrets: secrets.to_vec(),
            next_order: 0,
        })
    }

    pub(super) fn root(&mut self, root: &str) {
        if root != self.root {
            let switching = !self.root.is_empty();
            self.root = root.into();
            if switching {
                self.ignored_leaders = detached_leaders(self.process);
                self.launches.clear();
                self.shells.clear();
                self.next_order = 0;
            }
        }
    }

    pub(super) fn before_prompt(&mut self, root: Option<&str>) {
        if let Some(root) = root {
            self.root(root);
        }
        self.absorb_non_shell_leaders();
    }

    fn redact(&self, text: Option<&str>) -> Option<String> {
        let mut s = text?.trim().to_owned();
        if s.is_empty() {
            return None;
        }
        crate::transport::redact_text(&mut s, &self.secrets);
        Some(s)
    }

    fn is_shell_tool(u: &Value) -> bool {
        if u["kind"] == "execute" {
            return true;
        }
        if matches!(
            u["title"].as_str().or_else(|| u["name"].as_str()),
            Some("shell" | "Shell" | "Terminal" | "terminal" | "BackgroundShellSpawn")
        ) {
            return true;
        }
        u["rawInput"]["command"].is_string() || u["rawInput"]["cmd"].is_string()
    }

    pub(super) fn frame(&mut self, v: &Value, turn: Option<Uuid>, events: &mut Vec<Event>) {
        // Ignore session/load history replay when no live turn is active.
        let Some(turn) = turn else {
            return;
        };
        if self.root.is_empty()
            || v["method"] != "session/update"
            || v["params"]["sessionId"] != self.root
        {
            return;
        }
        let u = &v["params"]["update"];
        let Some(update_kind) = u["sessionUpdate"].as_str() else {
            return;
        };
        if !matches!(update_kind, "tool_call" | "tool_call_update") {
            return;
        }
        let Some(tool_id) = u["toolCallId"].as_str().filter(|s| !s.is_empty()) else {
            return;
        };
        self.launches.retain(|_, l| l.turn == turn);

        let cmd = self.redact(
            u["rawInput"]["command"]
                .as_str()
                .or_else(|| u["rawInput"]["cmd"].as_str()),
        );
        let desc = self.redact(u["rawInput"]["description"].as_str().or_else(|| {
            u["title"].as_str().filter(|s| {
                !matches!(
                    *s,
                    "shell" | "Shell" | "Terminal" | "terminal" | "BackgroundShellSpawn"
                )
            })
        }));

        if Self::is_shell_tool(u) || self.launches.contains_key(tool_id) {
            let entry = self
                .launches
                .entry(tool_id.to_owned())
                .or_insert_with(|| Launch {
                    turn,
                    command: None,
                    description: None,
                    started_ms: now_ms(),
                });
            if cmd.is_some() {
                entry.command = cmd;
            }
            if desc.is_some() {
                entry.description = desc;
            }
        }

        if update_kind != "tool_call_update" {
            return;
        }
        let status = u["status"].as_str();
        if matches!(status, Some("failed" | "cancelled" | "aborted")) {
            self.launches.remove(tool_id);
            self.absorb_non_shell_leaders();
            return;
        }
        if status != Some("completed") {
            return;
        }
        let Some(receipt) = parse_receipt(u) else {
            self.launches.remove(tool_id);
            self.absorb_non_shell_leaders();
            return;
        };
        let launch = self.launches.remove(tool_id).unwrap_or_else(|| Launch {
            turn,
            command: None,
            description: None,
            started_ms: now_ms(),
        });
        if launch.turn != turn {
            return;
        }

        // Ignore duplicate completion frames for the exact same running tool call.
        if let Some(existing) = self.shells.get(&receipt.shell_id)
            && !existing.ended
            && existing.tool_call_id.as_deref() == Some(tool_id)
        {
            return;
        }

        let raw_terminal =
            self.read_terminal_meta(&receipt.shell_id, receipt.terminal_path.as_deref());

        let claimed: BTreeSet<ProcessId> = self
            .shells
            .values()
            .filter(|s| !s.ended)
            .filter_map(|s| s.process)
            .collect();

        let resolved_proc = receipt
            .pid
            .and_then(|pid| owned_leader(self.process, pid))
            .or_else(|| raw_terminal.pid.and_then(|pid| owned_leader(self.process, pid)))
            .or_else(|| {
                let current = detached_leaders(self.process);
                let mut candidates = current
                    .into_iter()
                    .filter(|p| !self.ignored_leaders.contains(p) && !claimed.contains(p));
                let first = candidates.next()?;
                candidates.next().is_none().then_some(first)
            });

        let terminal = if match (resolved_proc, raw_terminal.pid) {
            (Some(proc_id), Some(file_pid)) => proc_id.pid == file_pid,
            (None, Some(file_pid)) => {
                receipt.pid.is_none_or(|rp| rp == file_pid) && !pid_alive_anywhere(file_pid)
            }
            _ => true,
        } {
            raw_terminal
        } else {
            TerminalMeta::default()
        };

        let command = launch
            .command
            .or_else(|| self.redact(receipt.command.as_deref()))
            .or_else(|| self.redact(terminal.command.as_deref()));
        let description = launch
            .description
            .or_else(|| self.redact(receipt.description.as_deref()));

        let fast_ended = if resolved_proc.is_none() {
            fast_exit_status(&terminal, launch.started_ms)
        } else {
            None
        };

        if resolved_proc.is_none() && fast_ended.is_none() {
            return;
        }

        // If this shell_id was used earlier, close out any prior un-ended occurrence first
        // and purge its aliases before emitting the new Background start.
        if let Some(prior) = self.shells.get_mut(&receipt.shell_id) {
            if !prior.ended && prior.process.is_some() && prior.process == resolved_proc {
                return;
            }
            if !prior.ended {
                prior.ended = true;
                let prior_status = if prior.stop_sent {
                    TaskStatus::Stopped
                } else {
                    TaskStatus::Completed
                };
                events.push(Event::Task {
                    id: receipt.shell_id.clone(),
                    change: TaskChange::Ended {
                        status: prior_status,
                        summary: None,
                        output: None,
                        output_file: prior
                            .terminal_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                    },
                });
            }
            prior.tool_call_id = None;
            prior.process = None;
        }

        let order = self.next_order;
        self.next_order += 1;
        let work = BackgroundWork {
            kind: BackgroundKind::Command,
            pending: true,
            order,
            awaited: false,
            turn_id: Some(turn),
            description,
            tool_use_id: Some(tool_id.to_owned()),
            command,
            parent_task_id: None,
        };
        events.push(super::mark(&receipt.shell_id, work));

        let terminal_path = terminal.path.or(receipt.terminal_path);
        let ended = fast_ended.is_some();
        if let Some(status) = fast_ended {
            events.push(Event::Task {
                id: receipt.shell_id.clone(),
                change: TaskChange::Ended {
                    status,
                    summary: None,
                    output: None,
                    output_file: terminal_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                },
            });
        }

        self.shells.insert(
            receipt.shell_id,
            Shell {
                process: resolved_proc,
                tool_call_id: if ended {
                    None
                } else {
                    Some(tool_id.to_owned())
                },
                terminal_path,
                ended,
                stop_sent: false,
            },
        );
    }

    fn absorb_non_shell_leaders(&mut self) {
        if !self.launches.is_empty() {
            return;
        }
        let claimed: BTreeSet<ProcessId> = self
            .shells
            .values()
            .filter(|s| !s.ended)
            .filter_map(|s| s.process)
            .collect();
        for leader in detached_leaders(self.process) {
            if !claimed.contains(&leader) {
                self.ignored_leaders.insert(leader);
            }
        }
    }

    pub(super) fn resolve_alias(&self, id: &str) -> Option<&str> {
        if let Some((k, _)) = self.shells.get_key_value(id) {
            return Some(k);
        }
        let mut matched = self.shells.iter().filter(|(_, s)| {
            !s.ended
                && (s.tool_call_id.as_deref() == Some(id)
                    || s.process.is_some_and(|p| {
                        id.parse::<u32>() == Ok(p.pid) && shell_alive(p)
                    }))
        });
        let (k, _) = matched.next()?;
        matched.next().is_none().then_some(k)
    }

    pub(super) fn refresh(&mut self, events: &mut Vec<Event>) {
        if self.root.is_empty() {
            return;
        }
        let ids: Vec<String> = self
            .shells
            .iter()
            .filter(|(_, s)| !s.ended)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let (proc_id, stop_sent, known_path) = {
                let shell = &self.shells[&id];
                (shell.process, shell.stop_sent, shell.terminal_path.clone())
            };
            if proc_id.is_some_and(shell_alive) {
                continue;
            }
            let raw_meta = self.read_terminal_meta(&id, known_path.as_deref());
            let meta = if proc_id
                .is_some_and(|p| raw_meta.pid.is_some_and(|file_pid| file_pid != p.pid))
            {
                TerminalMeta::default()
            } else {
                raw_meta
            };
            let status = classify_exit_status(stop_sent, &meta);
            let output_file = meta
                .path
                .or(known_path)
                .map(|p| p.to_string_lossy().into_owned());
            if let Some(shell) = self.shells.get_mut(&id) {
                shell.ended = true;
                shell.tool_call_id = None;
                shell.process = None;
            }
            events.push(Event::Task {
                id,
                change: TaskChange::Ended {
                    status,
                    summary: None,
                    output: None,
                    output_file,
                },
            });
        }
    }

    pub(super) fn stop(&mut self, id: &str) -> Result<()> {
        let key = self
            .resolve_alias(id)
            .ok_or_else(|| Error::Refused("unknown Cursor background shell in this session".into()))?
            .to_owned();
        let shell = &self.shells[&key];
        if shell.ended {
            return Err(Error::Refused(
                "Cursor background shell has already ended".into(),
            ));
        }
        let proc_id = shell.process.ok_or_else(|| {
            Error::Unsupported(
                "Cursor did not supply an owned live shell PID for this launch".into(),
            )
        })?;
        #[cfg(target_os = "macos")]
        {
            use nix::{
                errno::Errno,
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };

            let self_pid = std::process::id();
            if proc_id.pid <= 1
                || proc_id.pid > i32::MAX as u32
                || proc_id.pid == self_pid
                || self.process.is_some_and(|r| proc_id.pid == r.pid)
            {
                return Err(Error::Refused(
                    "refusing to signal non-isolated process group".into(),
                ));
            }
            if !group_verified_for_signal(proc_id, self.process) {
                return Err(Error::Refused(
                    "Cursor background shell occurrence is no longer running".into(),
                ));
            }
            let pgid = Pid::from_raw(proc_id.pid as i32);
            match killpg(pgid, Signal::SIGTERM) {
                Ok(()) => {}
                Err(Errno::ESRCH) => {
                    return Err(Error::Refused(
                        "Cursor background shell has already ended".into(),
                    ));
                }
                Err(err) => {
                    return Err(Error::Refused(format!(
                        "stopping Cursor background shell: {err}"
                    )));
                }
            }
            self.shells.get_mut(&key).expect("selected shell").stop_sent = true;
            let root_proc = self.process;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if group_verified_for_signal(proc_id, root_proc) {
                    let _ = killpg(pgid, Signal::SIGKILL);
                }
            });
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = proc_id;
            Err(Error::Unsupported(
                "Cursor background shell stop is only supported on macOS".into(),
            ))
        }
    }

    fn read_terminal_meta(&self, shell_id: &str, hinted: Option<&Path>) -> TerminalMeta {
        let Some(base_path) = &self.cursor_data_dir else {
            return TerminalMeta::default();
        };
        read_terminal_file_safe(base_path, &self.workspace_slug, shell_id, hinted)
            .unwrap_or_default()
    }
}

fn classify_exit_status(stop_sent: bool, meta: &TerminalMeta) -> TaskStatus {
    if stop_sent {
        return TaskStatus::Stopped;
    }
    match meta.status.as_deref() {
        Some("aborted" | "cancelled" | "canceled" | "stopped" | "killed") => TaskStatus::Stopped,
        Some("failed" | "error") => TaskStatus::Failed,
        _ if meta.exit_code.is_some_and(|code| code != 0) => TaskStatus::Failed,
        _ => TaskStatus::Completed,
    }
}

fn fast_exit_status(meta: &TerminalMeta, launch_started_ms: u64) -> Option<TaskStatus> {
    let status = meta.status.as_deref()?;
    let terminal = match status {
        "succeeded" | "completed" => TaskStatus::Completed,
        "failed" | "error" => TaskStatus::Failed,
        "aborted" | "cancelled" | "canceled" | "stopped" => TaskStatus::Stopped,
        _ => return None,
    };
    let started = meta.started_at.as_deref()?.trim();
    if started.is_empty() {
        return None;
    }
    if let Ok(ts) = started.parse::<jiff::Timestamp>() {
        let ms = ts.as_millisecond().max(0) as u64;
        if ms + 5_000 < launch_started_ms {
            return None;
        }
    } else if let Ok(ms) = started.parse::<u64>()
        && ms + 5_000 < launch_started_ms
    {
        return None;
    }
    Some(terminal)
}

fn valid_single_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && matches!(Path::new(name).components().next(), Some(Component::Normal(_)))
}

fn read_terminal_file_safe(
    cursor_data_dir: &Path,
    workspace_slug: &str,
    shell_id: &str,
    hinted: Option<&Path>,
) -> Option<TerminalMeta> {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::{
        ambient_authority,
        fs::{Dir, OpenOptions},
    };

    if !valid_single_name(shell_id) {
        return None;
    }
    let root_dir = Dir::open_ambient_dir(cursor_data_dir, ambient_authority()).ok()?;

    let open_rel = |rel_parts: &[&str], full_path: PathBuf| -> Option<TerminalMeta> {
        let (file_name, dirs) = rel_parts.split_last()?;
        if !valid_single_name(file_name) {
            return None;
        }
        let mut dir = root_dir.try_clone().ok()?;
        for segment in dirs {
            if !valid_single_name(segment) {
                return None;
            }
            dir = dir.open_dir_nofollow(segment).ok()?;
        }
        if !dir.symlink_metadata(file_name).ok()?.is_file() {
            return None;
        }
        let mut opts = OpenOptions::new();
        opts.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            opts.custom_flags(nix::libc::O_NONBLOCK);
        }
        let file = dir.open_with(file_name, &opts).ok()?.into_std();
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut buf = String::new();
        file.take(64 * 1024).read_to_string(&mut buf).ok()?;
        let mut meta = parse_terminal_frontmatter(&buf);
        meta.path = Some(full_path);
        Some(meta)
    };

    if let Some(hint) = hinted {
        let rel = hint.strip_prefix(cursor_data_dir).ok().or_else(|| {
            let canon = cursor_data_dir.canonicalize().ok()?;
            hint.strip_prefix(&canon).ok()
        });
        if let Some(rel) = rel {
            let mut parts = Vec::new();
            let mut clean = true;
            for comp in rel.components() {
                match comp {
                    Component::Normal(os) => match os.to_str() {
                        Some(s) if valid_single_name(s) => parts.push(s),
                        _ => {
                            clean = false;
                            break;
                        }
                    },
                    _ => {
                        clean = false;
                        break;
                    }
                }
            }
            if clean
                && !parts.is_empty()
                && let Some(meta) = open_rel(&parts, cursor_data_dir.join(rel))
            {
                return Some(meta);
            }
        }
    }

    let file_name = format!("{shell_id}.txt");
    if valid_single_name(workspace_slug) {
        let full = cursor_data_dir
            .join("projects")
            .join(workspace_slug)
            .join("terminals")
            .join(&file_name);
        if let Some(meta) = open_rel(&["projects", workspace_slug, "terminals", &file_name], full) {
            return Some(meta);
        }
    }

    let projects = root_dir.open_dir_nofollow("projects").ok()?;
    for entry in projects.entries().ok()?.take(64) {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let slug_os = entry.file_name();
        let Some(slug) = slug_os.to_str().filter(|s| valid_single_name(s)) else {
            continue;
        };
        if slug == workspace_slug {
            continue;
        }
        let full = cursor_data_dir
            .join("projects")
            .join(slug)
            .join("terminals")
            .join(&file_name);
        if let Some(meta) = open_rel(&["projects", slug, "terminals", &file_name], full) {
            return Some(meta);
        }
    }
    None
}

fn parse_terminal_frontmatter(contents: &str) -> TerminalMeta {
    let mut meta = TerminalMeta::default();
    let trimmed = contents.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    else {
        return meta;
    };
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))
        .or_else(|| rest.strip_suffix("\n---").map(str::len));
    let header = match end {
        Some(idx) => &rest[..idx],
        None => rest,
    };
    for line in header.lines().take(32) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim().trim_matches('"').trim_matches('\'');
        match key {
            "pid" => meta.pid = val.parse::<u32>().ok(),
            "status" => {
                if !val.is_empty() {
                    meta.status = Some(val.to_ascii_lowercase());
                }
            }
            "exit_code" | "exitCode" => meta.exit_code = val.parse::<i32>().ok(),
            "started_at" | "startedAt" => {
                if !val.is_empty() {
                    meta.started_at = Some(val.to_owned());
                }
            }
            "command" => {
                if !val.is_empty() {
                    meta.command = Some(val.to_owned());
                }
            }
            _ => {}
        }
    }
    meta
}

fn parse_receipt(u: &Value) -> Option<Receipt> {
    let ro = &u["rawOutput"];
    let meta = &ro["metadata"];
    let str_field = |obj: &Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            obj[*k]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
    };
    let u32_field = |obj: &Value, keys: &[&str]| -> Option<u32> {
        keys.iter().find_map(|k| {
            obj[*k]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .or_else(|| obj[*k].as_str()?.trim().parse::<u32>().ok())
        })
    };

    if let Some(shell_id) = str_field(ro, &["shellId", "shell_id", "shellID"])
        .or_else(|| str_field(meta, &["shellId", "shell_id", "shellID"]))
    {
        let bg = ro["isBackground"] == true
            || meta["isBackground"] == true
            || ro["background"] == true
            || meta["background"] == true
            || ro["status"] == "running"
            || meta["status"] == "running";
        let settled = !ro["exit"].is_null()
            || !ro["exitCode"].is_null()
            || !meta["exit"].is_null()
            || !meta["exitCode"].is_null()
            || !meta["signal"].is_null()
            || !meta["timeout"].is_null();
        if bg && !settled && valid_single_name(&shell_id) {
            return Some(Receipt {
                shell_id,
                pid: u32_field(ro, &["pid"]).or_else(|| u32_field(meta, &["pid"])),
                terminal_path: str_field(
                    ro,
                    &["terminalPath", "terminal_path", "outputFile", "output_file"],
                )
                .or_else(|| {
                    str_field(
                        meta,
                        &["terminalPath", "terminal_path", "outputFile", "output_file"],
                    )
                })
                .map(PathBuf::from),
                command: str_field(ro, &["command"]).or_else(|| str_field(meta, &["command"])),
                description: str_field(ro, &["description"])
                    .or_else(|| str_field(meta, &["description"])),
            });
        }
    }

    let mut texts = Vec::new();
    collect_texts(&u["content"], &mut texts);
    collect_texts(ro, &mut texts);
    let joined = texts.join("\n");
    parse_text_receipt(&joined)
}

fn collect_texts(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.trim().is_empty() => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_texts(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["text", "output", "stdout", "message", "content"] {
                if let Some(child) = map.get(key) {
                    collect_texts(child, out);
                }
            }
        }
        _ => {}
    }
}

fn parse_text_receipt(text: &str) -> Option<Receipt> {
    let lower = text.to_ascii_lowercase();
    let has_bg_marker = lower.contains("background command started")
        || lower.contains("sent to the background")
        || lower.contains("moved to background")
        || lower.contains("moved to the background")
        || lower.contains("running in the background");
    if !has_bg_marker {
        return None;
    }

    let mut shell_id = None;
    let mut pid = None;
    let mut terminal_path = None;
    let mut command = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if shell_id.is_none()
            && let Some(pos) = trimmed.to_ascii_lowercase().find("shell id:")
        {
            let after = &trimmed[pos + "shell id:".len()..];
            let token = after
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | ',' | ')' | '(' | '.'));
            if valid_single_name(token) {
                shell_id = Some(token.to_owned());
            }
        }
        if pid.is_none()
            && let Some(pos) = trimmed.to_ascii_lowercase().find("pid:")
        {
            let after = &trimmed[pos + "pid:".len()..];
            let digits: String = after
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(parsed) = digits.parse::<u32>() {
                pid = Some(parsed);
            }
        }
        if command.is_none()
            && let Some(rest) = trimmed.strip_prefix("Command:")
        {
            let c = rest.trim();
            if !c.is_empty() {
                command = Some(c.to_owned());
            }
        }
        if terminal_path.is_none() {
            for token in trimmed.split_whitespace() {
                let clean = token.trim_matches(|c: char| {
                    matches!(c, '`' | '"' | '\'' | ',' | ')' | '(')
                });
                if clean.starts_with('/') && clean.ends_with(".txt") {
                    terminal_path = Some(PathBuf::from(clean));
                    break;
                }
            }
        }
    }

    Some(Receipt {
        shell_id: shell_id?,
        pid,
        terminal_path,
        command,
        description: None,
    })
}

#[cfg(target_os = "macos")]
fn stamp(sec: u64, usec: u64) -> Option<u64> {
    sec.checked_mul(1_000_000)?.checked_add(usec)
}

#[cfg(target_os = "macos")]
fn live_proc_row(pid: u32) -> Option<(u32, u32, u64)> {
    use libproc::{
        proc_pid::{kproc_info_raw, pidinfo},
        task_info::TaskAllInfo,
    };

    if pid == 0 {
        return None;
    }
    let raw: i32 = pid.try_into().ok()?;
    if let Ok(info) = pidinfo::<TaskAllInfo>(raw, 0) {
        if info.pbsd.pbi_status == nix::libc::SZOMB {
            return None;
        }
        return Some((
            info.pbsd.pbi_ppid,
            info.pbsd.pbi_pgid,
            stamp(info.pbsd.pbi_start_tvsec, info.pbsd.pbi_start_tvusec)?,
        ));
    }
    if let Ok(info) = kproc_info_raw(raw) {
        if u32::try_from(info.kp_proc.p_stat).ok()? == nix::libc::SZOMB {
            return None;
        }
        let birth = info.kp_proc.p_un;
        return Some((
            info.kp_eproc.e_ppid.try_into().ok()?,
            info.kp_eproc.e_pgid.try_into().ok()?,
            stamp(birth.tv_sec.try_into().ok()?, birth.tv_usec.try_into().ok()?)?,
        ));
    }
    None
}

#[cfg(target_os = "macos")]
fn detached_leaders(root: Option<ProcessId>) -> BTreeSet<ProcessId> {
    let Some(root) = root else {
        return BTreeSet::new();
    };
    let self_pid = std::process::id();
    crate::resources::tree::descendants(root)
        .into_iter()
        .filter(|p| {
            p.pid > 1
                && p.pid != root.pid
                && p.pid != self_pid
                && live_proc_row(p.pid).is_some_and(|(_, pgid, born)| pgid == p.pid && born == p.born)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn owned_leader(root: Option<ProcessId>, pid: u32) -> Option<ProcessId> {
    let root = root?;
    if pid <= 1 || pid == root.pid || pid == std::process::id() {
        return None;
    }
    let found = crate::resources::tree::descendants(root)
        .into_iter()
        .find(|p| p.pid == pid)?;
    let (_, pgid, born) = live_proc_row(pid)?;
    (pgid == pid && born == found.born).then_some(found)
}

#[cfg(target_os = "macos")]
fn shell_alive(proc_id: ProcessId) -> bool {
    use libproc::processes::{ProcFilter, pids_by_type};

    if live_proc_row(proc_id.pid).is_some_and(|(_, _, born)| born == proc_id.born) {
        return true;
    }
    let Ok(pids) = pids_by_type(ProcFilter::All) else {
        return false;
    };
    pids.into_iter().any(|pid| {
        pid > 1
            && live_proc_row(pid)
                .is_some_and(|(_, pgid, born)| pgid == proc_id.pid && born >= proc_id.born)
    })
}

#[cfg(target_os = "macos")]
fn group_verified_for_signal(proc_id: ProcessId, root: Option<ProcessId>) -> bool {
    use libproc::processes::{ProcFilter, pids_by_type};

    if proc_id.pid <= 1
        || proc_id.pid > i32::MAX as u32
        || proc_id.pid == std::process::id()
        || root.is_some_and(|r| proc_id.pid == r.pid)
    {
        return false;
    }
    match live_proc_row(proc_id.pid) {
        Some((_, pgid, born)) => return born == proc_id.born && pgid == proc_id.pid,
        None => {}
    }
    let Ok(pids) = pids_by_type(ProcFilter::All) else {
        return false;
    };
    pids.into_iter().any(|pid| {
        pid > 1
            && root.is_none_or(|r| pid != r.pid)
            && pid != std::process::id()
            && live_proc_row(pid)
                .is_some_and(|(_, pgid, born)| pgid == proc_id.pid && born >= proc_id.born)
    })
}

#[cfg(target_os = "macos")]
fn pid_alive_anywhere(pid: u32) -> bool {
    live_proc_row(pid).is_some()
}

#[cfg(not(target_os = "macos"))]
fn pid_alive_anywhere(_: u32) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn detached_leaders(_: Option<ProcessId>) -> BTreeSet<ProcessId> {
    BTreeSet::new()
}

#[cfg(not(target_os = "macos"))]
fn owned_leader(_: Option<ProcessId>, _: u32) -> Option<ProcessId> {
    None
}

#[cfg(not(target_os = "macos"))]
fn shell_alive(_: ProcessId) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cursor_text_and_structured_receipts_and_frontmatter() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "completed",
            "content": [{
                "type": "content",
                "content": {
                    "type": "text",
                    "text": "Background command started.\nShell ID: 7\nPID: 4242\nTerminal file: /tmp/.cursor/projects/demo/terminals/7.txt"
                }
            }]
        });
        let receipt = parse_receipt(&update).expect("parsed text receipt");
        assert_eq!(receipt.shell_id, "7");
        assert_eq!(receipt.pid, Some(4242));
        assert_eq!(
            receipt.terminal_path.as_deref(),
            Some(Path::new("/tmp/.cursor/projects/demo/terminals/7.txt"))
        );

        let fm = "---\npid: 4242\nstatus: aborted\nexit_code: 143\nstarted_at: 2026-11-24T22:00:00Z\n---\nout\n";
        let meta = parse_terminal_frontmatter(fm);
        assert_eq!(meta.pid, Some(4242));
        assert_eq!(meta.status.as_deref(), Some("aborted"));
        assert_eq!(meta.exit_code, Some(143));
        assert_eq!(classify_exit_status(false, &meta), TaskStatus::Stopped);
    }
}
