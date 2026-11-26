//! OpenCode sends the launch through ACP. The same native session persists the completion.
mod jobs;
use crate::{
    BackgroundKind, BackgroundWork, Error, Event, Result, SessionOptions, TaskChange,
    resources::tree::{self, ProcessId},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Tracker {
    root: String,
    process: Option<ProcessId>,
    database: Option<PathBuf>,
    launches: HashMap<String, (Uuid, Option<String>)>,
    shells: BTreeMap<String, Shell>,
    secrets: Vec<String>,
    seen: Option<Stamp>,
    read: Option<oneshot::Receiver<Option<JobSnapshot>>>,
    force_read: bool,
}
type JobSnapshot = (Stamp, BTreeMap<String, jobs::Job>);
struct Shell {
    work: BackgroundWork,
    process: Option<ProcessId>,
    alias: Option<String>,
    ended: bool,
    stop_sent: bool,
}
type Stamp = ((u64, SystemTime), Option<(u64, SystemTime)>);
fn stamp(path: &Path) -> Option<Stamp> {
    let stat = |p: &Path| {
        let m = std::fs::symlink_metadata(p).ok()?;
        m.is_file().then_some((m.len(), m.modified().ok()?))
    };
    Some((
        stat(path)?,
        stat(&crate::workbench::native_sqlite::store::wal_path(path)),
    ))
}
pub(super) fn database(options: &SessionOptions) -> Option<PathBuf> {
    let data = crate::options::env_path(options, "XDG_DATA_HOME")
        .or_else(|| options.session_home().map(|p| p.join(".local/share")))?;
    let file = crate::options::env_path(options, "OPENCODE_DB")
        .unwrap_or_else(|| PathBuf::from("opencode.db"));
    (file != Path::new(":memory:")).then(|| options.cwd.join(data).join("opencode").join(file))
}
impl Tracker {
    pub(super) fn for_runtime(id: &str) -> Option<Self> {
        (id == "opencode").then(Self::default)
    }
    pub(super) fn bind(
        &mut self,
        root: &str,
        process: Option<ProcessId>,
        database: Option<PathBuf>,
    ) {
        if root != self.root {
            self.root = root.into();
            self.launches.clear();
            self.shells.clear();
            self.seen = None;
            self.read = None;
            self.force_read = true;
        }
        self.process = process;
        self.database = database;
    }
    pub(super) fn configure(
        &mut self,
        process: Option<ProcessId>,
        database: Option<PathBuf>,
        secrets: &[String],
    ) {
        self.process = process;
        self.database = database;
        self.secrets = secrets.to_vec();
    }
    pub(super) fn root(&mut self, root: &str) {
        if root != self.root {
            self.bind(root, self.process, self.database.clone());
        }
    }
    fn command(&self, cmd: Option<&str>) -> Option<String> {
        cmd.map(|cmd| {
            let mut cmd = cmd.to_owned();
            crate::transport::redact_text(&mut cmd, &self.secrets);
            cmd
        })
    }
    fn start(
        &mut self,
        id: &str,
        cmd: Option<String>,
        tool: Option<&str>,
        turn: Option<Uuid>,
        events: &mut Vec<Event>,
    ) {
        let order = self.shells.len();
        if let Some(shell) = self.shells.get_mut(id) {
            if !shell.ended && shell.work.turn_id.is_none() && turn.is_some() {
                shell.work.turn_id = turn;
                shell.work.tool_use_id = tool.map(str::to_owned);
                if cmd.is_some() {
                    shell.work.command = cmd;
                }
                events.push(super::mark(id, shell.work.clone()));
            }
            return;
        }
        let work = BackgroundWork {
            kind: BackgroundKind::Command,
            pending: true,
            order,
            awaited: false,
            turn_id: turn,
            tool_use_id: tool.map(str::to_owned),
            command: cmd,
            parent_task_id: None,
            description: None,
        };
        events.push(super::mark(id, work.clone()));
        self.shells.insert(
            id.into(),
            Shell {
                work,
                process: None,
                alias: None,
                ended: false,
                stop_sent: false,
            },
        );
    }
    pub(super) fn frame(&mut self, v: &Value, turn: Option<Uuid>, events: &mut Vec<Event>) {
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
        if !matches!(
            u["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            return;
        }
        let Some(tool) = u["toolCallId"].as_str() else {
            return;
        };
        if u["kind"] == "execute" {
            // Only the current turn's observed executable tool can bind a native PID.
            self.launches.retain(|_, (t, _)| *t == turn);
            let cmd = self.command(
                u["rawInput"]["command"]
                    .as_str()
                    .or(u["rawInput"]["cmd"].as_str()),
            );
            self.launches.insert(tool.into(), (turn, cmd));
        }
        if !matches!(u["status"].as_str(), Some("completed" | "failed")) {
            return;
        }
        let Some((owner, cmd)) = self.launches.remove(tool) else {
            return;
        };
        let meta = &u["rawOutput"]["metadata"];
        if owner != turn
            || u["status"] != "completed"
            || meta["status"] != "running"
            || !meta["exit"].is_null()
            || !meta["signal"].is_null()
            || !meta["timeout"].is_null()
        {
            return;
        }
        let Some(id) = meta["shellID"].as_str().filter(|s| !s.is_empty()) else {
            return;
        };
        self.start(id, cmd, Some(tool), Some(turn), events);
        if let Some(shell) = self.shells.get_mut(id)
            && !shell.ended
        {
            if let Some(pid) = meta["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
                shell.process = owned(self.process, pid);
            }
            self.force_read = true;
        }
    }
    pub(super) fn resolve_alias(&self, id: &str) -> Option<&str> {
        if let Some((k, _)) = self.shells.get_key_value(id) {
            return Some(k);
        }
        let mut matched = self.shells.iter().filter(|(_, s)| {
            !s.ended
                && (s.alias.as_deref() == Some(id)
                    || s.process.is_some_and(|p| {
                        id.parse::<u32>() == Ok(p.pid) && tree::identity(p.pid) == Some(p)
                    }))
        });
        let (k, _) = matched.next()?;
        matched.next().is_none().then_some(k)
    }
    pub(super) fn stop(&mut self, id: &str) -> Result<()> {
        let key = self.resolve_alias(id).ok_or_else(|| {
            Error::Refused("unknown OpenCode shell in this native session".into())
        })?;
        let key = key.to_owned();
        let shell = &self.shells[&key];
        if shell.ended {
            return Err(Error::Refused("OpenCode shell has already ended".into()));
        }
        let p = shell.process.ok_or_else(|| {
            Error::Unsupported(
                "OpenCode did not supply an owned live shell PID for this launch".into(),
            )
        })?;
        if owned(self.process, p.pid) != Some(p) {
            return Err(Error::Refused(
                "OpenCode shell process identity changed".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            // The POSIX OpenCode runner creates a detached process group per command.
            // Refuse remote or alternative spawners that do not prove the same group ownership.
            let pid = nix::unistd::Pid::from_raw(p.pid as i32);
            if nix::unistd::getpgid(Some(pid)).ok() != Some(pid) || tree::identity(p.pid) != Some(p)
            {
                return Err(Error::Refused(
                    "OpenCode shell no longer owns its command process group".into(),
                ));
            }
            nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGTERM)
                .map_err(|err| Error::Refused(format!("stopping OpenCode shell: {err}")))?;
            self.shells.get_mut(&key).expect("selected shell").stop_sent = true;
            let owner = self.process;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if owned(owner, p.pid) == Some(p)
                    && nix::unistd::getpgid(Some(pid)).ok() == Some(pid)
                    && tree::identity(p.pid) == Some(p)
                {
                    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                }
            });
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        Err(Error::Unsupported(
            "OpenCode shell process ownership is unavailable on this platform".into(),
        ))
    }
    fn apply(&mut self, jobs: BTreeMap<String, jobs::Job>, events: &mut Vec<Event>) {
        for (id, job) in jobs {
            if job.running && job.ended.is_none() {
                let cmd = self.command(job.command.as_deref());
                self.start(&id, cmd, None, None, events);
            }
            let Some(shell) = self.shells.get_mut(&id) else {
                continue;
            };
            if let Some(alias) = job.alias {
                shell.alias = Some(alias);
            }
            if let Some(status) = job.ended
                && !shell.ended
            {
                shell.ended = true;
                let status = if shell.stop_sent && job.killed_by_signal {
                    crate::TaskStatus::Stopped
                } else {
                    status
                };
                events.push(Event::Task {
                    id,
                    change: TaskChange::Ended {
                        status,
                        summary: None,
                        output: None,
                        output_file: None,
                    },
                });
            }
        }
    }
    pub(super) fn refresh(&mut self, events: &mut Vec<Event>) {
        if let Some(rx) = &mut self.read {
            match rx.try_recv() {
                Ok(Some((stamp, jobs))) => {
                    self.seen = Some(stamp);
                    self.apply(jobs, events);
                }
                Err(oneshot::error::TryRecvError::Empty) => return,
                _ => {}
            }
            self.read = None;
        }
        let Some(path) = self.database.clone() else {
            return;
        };
        if self.root.is_empty() {
            return;
        }
        if self.seen.is_some() && !self.force_read && self.shells.values().all(|s| s.ended) {
            return;
        }
        let Some(before) = stamp(&path) else {
            return;
        };
        if self.seen == Some(before) && !self.force_read {
            return;
        }
        self.force_read = false;
        let watching = self
            .shells
            .iter()
            .filter(|(_, s)| !s.ended)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let root = self.root.clone();
        let (tx, rx) = oneshot::channel();
        self.read = Some(rx);
        tokio::task::spawn_blocking(move || {
            let result = jobs::read(&path, &root, &watching).ok();
            let _ = tx.send(
                result
                    .filter(|_| stamp(&path) == Some(before))
                    .map(|jobs| (before, jobs)),
            );
        });
    }
}
#[cfg(target_os = "macos")]
fn owned(root: Option<ProcessId>, pid: u32) -> Option<ProcessId> {
    let root = root?;
    if root.pid == pid {
        return None;
    }
    tree::descendants(root).into_iter().find(|p| p.pid == pid)
}
#[cfg(not(target_os = "macos"))]
fn owned(_: Option<ProcessId>, _: u32) -> Option<ProcessId> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn root_native_shell_receipt_owns_one_task_until_its_actual_end() {
        let mut s = Tracker::for_runtime("opencode").unwrap();
        assert!(Tracker::for_runtime("cursor").is_none());
        s.configure(None, None, &["sensitive".into()]);
        s.root("root");
        let turn = Some(Uuid::new_v4());
        let mut e = vec![];
        let frame = |p: &str, u: Value| json!({"method":"session/update","params":{"sessionId":p,"update":u}});
        let begin = json!({"sessionUpdate":"tool_call","toolCallId":"tool","kind":"execute","rawInput":{"command":"echo sensitive"}});
        let result = json!({"sessionUpdate":"tool_call_update","toolCallId":"tool","status":"completed","rawOutput":{"metadata":{"status":"running","shellID":"shell","pid":1}}});
        s.frame(&frame("child", begin.clone()), turn, &mut e);
        s.frame(&frame("root", result.clone()), turn, &mut e);
        assert!(e.is_empty());
        s.frame(&frame("root", begin), turn, &mut e);
        s.frame(&frame("root", result.clone()), turn, &mut e);
        s.frame(&frame("root", result), turn, &mut e);
        assert!(
            matches!(&e[..],[Event::Task{id,change:TaskChange::Background{work}}] if id=="shell" && work.turn_id==turn && !work.command.as_ref().unwrap().contains("sensitive"))
        );
        assert!(s.resolve_alias("1").is_none());
        assert!(s.stop("shell").is_err());
        s.apply(
            [(
                "shell".into(),
                jobs::Job {
                    alias: Some("job".into()),
                    running: true,
                    ..Default::default()
                },
            )]
            .into(),
            &mut e,
        );
        assert_eq!(s.resolve_alias("job"), Some("shell"));
        s.apply(BTreeMap::new(), &mut e);
        assert_eq!(e.len(), 1);
        s.apply(
            [(
                "shell".into(),
                jobs::Job {
                    ended: Some(crate::TaskStatus::Failed),
                    ..Default::default()
                },
            )]
            .into(),
            &mut e,
        );
        assert!(
            matches!(&e[1],Event::Task{id,change:TaskChange::Ended{status:crate::TaskStatus::Failed,..}} if id=="shell")
        );
        assert!(s.resolve_alias("job").is_none());
    }
    #[test]
    fn read_direct_native_rows_scopes_both_session_ids_and_observes_consumed_receipts() {
        use sqlite_core::{
            Value as Sql,
            rebuild::{RecoveredTable, build_recovered_db_tables},
        };
        let table = |name: &str, columns: &[&str], rows: Vec<Vec<String>>| RecoveredTable {
            name: name.into(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Sql::Text).collect())
                .collect(),
        };
        let recovery = |root: &str, id: &str| {
            json!({"kind":"shell","sessionID":root,"shellID":id,"command":"sleep 1"}).to_string()
        };
        let receipt = |id: &str, exit: i32| {
            json!({"metadata":{"source":"shell","shellID":id,"state":"completed","exit":exit}})
                .to_string()
        };
        let bytes = build_recovered_db_tables(&[
            table(
                "job",
                &["session_id", "recovery", "id", "status"],
                vec![
                    vec![
                        "root".into(),
                        recovery("root", "live"),
                        "job-alias".into(),
                        "running".into(),
                    ],
                    vec![
                        "root".into(),
                        recovery("other", "foreign"),
                        "wrong".into(),
                        "running".into(),
                    ],
                ],
            ),
            table(
                "session_message",
                &["session_id", "type", "data"],
                vec![
                    vec!["root".into(), "synthetic".into(), receipt("settled", 7)],
                    vec!["other".into(), "synthetic".into(), receipt("live", 0)],
                    vec!["root".into(), "user".into(), receipt("live", 0)],
                ],
            ),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        std::fs::write(&path, bytes).unwrap();
        let rows = jobs::read(&path, "root", &["settled".into()]).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows["live"].running && rows["live"].ended.is_none());
        assert_eq!(rows["settled"].ended, Some(crate::TaskStatus::Failed));
    }
}
