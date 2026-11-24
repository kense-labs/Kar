//! Project-owned interactive terminals and revocable previews of their listeners.
mod history;
mod preview;
mod process;
mod routes;
use super::{Workbench, missing};
use crate::{Error, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
pub(super) use routes::routes;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
struct ProcessId {
    pid: u32,
    born: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
struct Port {
    owner: ProcessId,
    address: IpAddr,
    port: u16,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Status {
    Starting,
    Running,
    Closing,
    Exited { code: u32, signal: Option<String> },
    Interrupted { process_alive: Option<bool> },
    Error { reason: String },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Meta {
    id: Uuid,
    project: Uuid,
    cwd: PathBuf,
    shell: PathBuf,
    cols: u16,
    rows: u16,
    process: Option<ProcessId>,
    status: Status,
}
#[derive(Serialize)]
struct Page {
    terminal: Meta,
    oldest: u64,
    start: u64,
    next: u64,
    end: u64,
    missed: bool,
    data_base64: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct InputReceipt {
    id: Uuid,
    sha256: String,
    delivered: bool,
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Protocol("missing terminal storage parent".into()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(tmp.as_file_mut(), value)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_reader(File::open(path)?)?)
}
fn pty_error(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}
fn size(cols: u16, rows: u16) -> Result<PtySize> {
    if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
        return Err(Error::Refused(
            "terminal dimensions must be in 1..=1000".into(),
        ));
    }
    Ok(PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    })
}

struct Live {
    _worktree_pin: Option<File>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    process: Option<ProcessId>,
}
impl Live {
    fn kill(&mut self) {
        if let Some(root) = self.process {
            process::stop_tree(root)
        }
        let _ = self.killer.kill();
    }
}
struct Terminal {
    dir: PathBuf,
    meta: Mutex<Meta>,
    history: Mutex<history::History>,
    live: Mutex<Option<Live>>,
    changed: watch::Sender<u64>,
}
impl Terminal {
    fn new(dir: PathBuf, meta: Meta) -> Result<Arc<Self>> {
        fs::create_dir_all(dir.join("input"))?;
        let history = history::History::open(&dir.join("history"))?;
        Ok(Arc::new(Self {
            dir,
            meta: Mutex::new(meta),
            history: Mutex::new(history),
            live: Mutex::new(None),
            changed: watch::channel(0).0,
        }))
    }
    fn update(&self, f: impl FnOnce(&mut Meta)) -> Result<()> {
        let mut record = self.meta.lock().unwrap();
        let mut next = record.clone();
        f(&mut next);
        write_json(&self.dir.join("session.json"), &next)?;
        *record = next;
        self.notify();
        Ok(())
    }
    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    fn snapshot(&self) -> Meta {
        let mut m = self.meta.lock().unwrap().clone();
        if let Status::Interrupted { process_alive } = &mut m.status {
            *process_alive = m.process.map(|p| process::identity(p.pid) == Some(p));
        }
        m
    }
    fn start(self: &Arc<Self>, cwd: std::fs::File, worktree_pin: Option<File>) -> Result<()> {
        let m = self.snapshot();
        let pair = native_pty_system()
            .openpty(size(m.cols, m.rows)?)
            .map_err(pty_error)?;
        let mut command = CommandBuilder::new(&m.shell);
        command.arg("-i");
        command.env("TERM", "xterm-256color");
        command.env("PWD", &m.cwd);
        command.env_remove("KAR_WORKBENCH_TOKEN");
        // chdir to the capability opened by workspace(), before exec closes its fd.
        #[cfg(target_os = "macos")]
        {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let pinned = cwd.metadata()?;
            // Some macOS filesystems expose directories through volfs, others
            // through the descriptor namespace. Confirm identity first so the
            // PTY adapter cannot silently fall back to the user's home.
            let checked = [
                format!("/dev/fd/{}", cwd.as_raw_fd()),
                format!("/.vol/{}/{}", pinned.dev(), pinned.ino()),
            ]
            .into_iter()
            .find(|candidate| {
                fs::metadata(candidate)
                    .is_ok_and(|m| m.is_dir() && m.dev() == pinned.dev() && m.ino() == pinned.ino())
            })
            .ok_or_else(|| {
                Error::Unsupported("unable to pin terminal working directory on this volume".into())
            })?;
            command.cwd(checked);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = &cwd;
            command.cwd(&m.cwd);
        }
        let mut reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let writer = pair.master.take_writer().map_err(pty_error)?;
        let mut child = pair.slave.spawn_command(command).map_err(pty_error)?;
        drop(pair.slave);
        let process = child.process_id().and_then(process::identity);
        let killer = child.clone_killer();
        if let Err(e) = self.update(|m| {
            m.process = process;
            m.status = Status::Running;
        }) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        *self.live.lock().unwrap() = Some(Live {
            _worktree_pin: worktree_pin,
            master: pair.master,
            writer,
            killer,
            process,
        });
        let weak = Arc::downgrade(self);
        let (drain, drained) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let Some(s) = weak.upgrade() else { break };
                        let stored = s.history.lock().unwrap().append(&buf[..n]);
                        if let Err(error) = stored {
                            let _ = s.update(|m| {
                                m.status = Status::Error {
                                    reason: error.to_string(),
                                }
                            });
                            if let Some(live) = &mut *s.live.lock().unwrap() {
                                live.kill();
                            }
                            break;
                        }
                        s.notify();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = drain.send(());
        });
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            let exit = child.wait();
            let _ = drained.recv_timeout(Duration::from_secs(1));
            if let Some(s) = weak.upgrade() {
                s.live.lock().unwrap().take();
                let _ = s.update(|m| {
                    if !matches!(m.status, Status::Error { .. }) {
                        m.status = match exit {
                            Ok(status) => Status::Exited {
                                code: status.exit_code(),
                                signal: status.signal().map(str::to_owned),
                            },
                            Err(e) => Status::Error {
                                reason: e.to_string(),
                            },
                        }
                    }
                });
            }
        });
        Ok(())
    }
    fn page(&self, after: u64) -> Result<Page> {
        let m = self.snapshot();
        let history = self.history.lock().unwrap();
        let (start, bytes) = history.page(after)?;
        Ok(Page {
            terminal: m,
            oldest: history.first(),
            start,
            next: start + bytes.len() as u64,
            end: history.end,
            missed: after < start,
            data_base64: STANDARD.encode(bytes),
        })
    }
    fn input(&self, id: Uuid, bytes: &[u8]) -> Result<InputReceipt> {
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err(Error::Refused("invalid terminal input length".into()));
        }
        let mut live = self.live.lock().unwrap();
        let path = self.dir.join("input").join(format!("{id}.json"));
        let digest = format!("{:x}", Sha256::digest(bytes));
        if path.try_exists()? {
            let previous: InputReceipt = read_json(&path)?;
            if previous.sha256 != digest {
                return Err(Error::Refused(
                    "input ID belongs to different terminal bytes".into(),
                ));
            }
            return Ok(previous);
        }
        let owner = live
            .as_mut()
            .ok_or_else(|| Error::Closed("terminal input closed".into()))?;
        let mut receipt = InputReceipt {
            id,
            sha256: digest,
            delivered: false,
        };
        write_json(&path, &receipt)?;
        owner.writer.write_all(bytes)?;
        owner.writer.flush()?;
        receipt.delivered = true;
        write_json(&path, &receipt)?;
        Ok(receipt)
    }
    fn resize(&self, cols: u16, rows: u16) -> Result<Meta> {
        let live = self.live.lock().unwrap();
        let owner = live
            .as_ref()
            .ok_or_else(|| Error::Closed("terminal closed".into()))?;
        owner.master.resize(size(cols, rows)?).map_err(pty_error)?;
        self.update(|m| {
            m.cols = cols;
            m.rows = rows
        })?;
        Ok(self.snapshot())
    }
    fn close(&self) -> Result<Meta> {
        if let Some(live) = self.live.lock().unwrap().as_mut() {
            self.update(|m| m.status = Status::Closing)?;
            live.kill();
        } else if let Some(root) = self.snapshot().process
            && matches!(
                self.snapshot().status,
                Status::Interrupted {
                    process_alive: Some(true)
                }
            )
        {
            process::stop_tree(root);
        }
        Ok(self.snapshot())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Ok(Some(live)) = self.live.get_mut().map(|v| v.as_mut()) {
            live.kill();
        }
    }
}

pub(super) struct TerminalPreview {
    dir: PathBuf,
    terminals: Mutex<HashMap<Uuid, Arc<Terminal>>>,
    grants: preview::Grants,
}
impl TerminalPreview {
    pub fn open(state: &Path) -> Result<Self> {
        let dir = state.join("terminals");
        fs::create_dir_all(&dir)?;
        let mut terminals = HashMap::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let file = entry.path().join("session.json");
            if !entry.file_type()?.is_dir() || !file.try_exists()? {
                continue;
            }
            let m: Meta = read_json(&file)?;
            if entry.file_name().to_str() != Some(&m.id.to_string()) {
                return Err(Error::Protocol("invalid terminal identity".into()));
            }
            let id = m.id;
            let interrupted = matches!(
                m.status,
                Status::Running | Status::Starting | Status::Closing
            );
            let terminal = Terminal::new(entry.path(), m)?;
            if interrupted {
                terminal.update(|m| {
                    m.status = Status::Interrupted {
                        process_alive: m.process.map(|p| process::identity(p.pid) == Some(p)),
                    }
                })?;
            }
            terminals.insert(id, terminal);
        }
        Ok(Self {
            dir,
            terminals: Mutex::new(terminals),
            grants: preview::Grants::default(),
        })
    }
    fn find(&self, project: Uuid, id: Uuid) -> Result<Arc<Terminal>> {
        self.terminals
            .lock()
            .unwrap()
            .get(&id)
            .filter(|s| s.snapshot().project == project)
            .cloned()
            .ok_or_else(missing)
    }
}

impl Workbench {
    fn new_terminal(
        &self,
        project: Uuid,
        id: Uuid,
        worktree: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> Result<Meta> {
        size(cols, rows)?;
        let mut p = self.project(project)?;
        self.workspace(&p)?;
        if let Some(requested) = worktree {
            let selected = p.root.join(requested).canonicalize()?;
            if selected != p.root {
                let original = p.root.clone();
                p.root = selected.clone();
                self.workspace(&p)?;
                let root = gix::discover(&original).map_err(pty_error)?;
                let linked = gix::open(&selected).map_err(pty_error)?;
                let same =
                    root.common_dir().canonicalize()? == linked.common_dir().canonicalize()?;
                if !same
                    || linked
                        .workdir()
                        .and_then(|d| d.canonicalize().ok())
                        .as_ref()
                        != Some(&selected)
                {
                    return Err(super::denied());
                }
            }
        }
        let workspace = self.workspace(&p)?;
        let worktree_pin = crate::workbench_git::pin_checkout(&p.root)?.map(|(_, f)| f);
        let state = &self.0.terminal_preview;
        let mut known = state.terminals.lock().unwrap();
        if let Some(s) = known.get(&id) {
            let m = s.snapshot();
            if m.project != project || m.cwd != p.root {
                return Err(missing());
            }
            return Ok(m);
        }
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let m = Meta {
            id,
            project,
            cwd: p.root,
            shell,
            cols,
            rows,
            status: Status::Starting,
            process: None,
        };
        let term = Terminal::new(state.dir.join(id.to_string()), m)?;
        term.update(|_| {})?;
        let handle = workspace.root_handle()?;
        known.insert(id, term.clone());
        if let Err(e) = term.start(handle, worktree_pin) {
            term.update(|m| {
                m.status = Status::Error {
                    reason: e.to_string(),
                }
            })?;
            return Err(e);
        }
        Ok(term.snapshot())
    }
}
