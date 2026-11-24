use super::{
    folders,
    setup::{self, Lifecycle, Snapshot},
};
use crate::{
    Error, Result,
    workbench::{Project, Workbench, denied, missing},
    workbench_git::Repository,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(super) struct Create {
    directory: PathBuf,
    branch: String,
    #[serde(default = "head")]
    base: String,
    repositories: Option<Vec<PathBuf>>,
}
fn head() -> String {
    "HEAD".into()
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Member {
    pub name: String,
    pub repository_path: PathBuf,
    pub worktree_path: PathBuf,
    worktree_id: Uuid,
    pub ready: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(in super::super) struct Task {
    pub root: PathBuf,
    pub project_id: Uuid,
    pub project_root: PathBuf,
    pub anchor: PathBuf,
    pub project_path: PathBuf,
    pub project_is_worktree: bool,
    pub(super) repositories: Vec<Member>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    id: Uuid,
    request: Create,
    pub task: Task,
    pub progress: Snapshot,
}
pub(in super::super) struct Store {
    dir: PathBuf,
    jobs: Mutex<HashMap<Uuid, (Uuid, Arc<Lifecycle>)>>,
    start: std::sync::Once,
}
impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            jobs: Default::default(),
            start: Default::default(),
        }
    }
    fn cancelled_path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.cancel"))
    }
    pub fn recover(&self, app: &Workbench) {
        self.start.call_once(|| {
            let app = app.clone();
            let dir = self.dir.clone();
            tokio::task::spawn_blocking(move || {
                let Ok(files) = fs::read_dir(dir) else { return };
                for file in files
                    .flatten()
                    .filter(|f| f.path().extension().is_some_and(|e| e == "json"))
                {
                    let Some(id) = file
                        .path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| Uuid::parse_str(s).ok())
                    else {
                        continue;
                    };
                    if let Ok(Some(r)) = app.0.task_folders.read(id)
                        && r.progress.phase == "running"
                    {
                        let _ = create(&app, r.task.project_id, id, r.request);
                    }
                }
            });
        });
    }
    pub(super) fn cancel(&self, p: Uuid, id: Uuid) -> Result<bool> {
        let life = self.lifecycle(p, id)?;
        let gate = life.running.lock().unwrap();
        let Some(flag) = gate.as_ref() else {
            return Ok(false);
        };
        if flag.load(Ordering::SeqCst)
            || self
                .get(p, id)?
                .is_none_or(|r| r.progress.phase != "running")
        {
            return Ok(false);
        }
        setup::write(&self.cancelled_path(id), &true)?;
        flag.store(true, Ordering::SeqCst);
        Ok(true)
    }
    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
    fn read(&self, id: Uuid) -> Result<Option<Record>> {
        match fs::File::open(self.path(id)) {
            Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub(super) fn get(&self, p: Uuid, id: Uuid) -> Result<Option<Record>> {
        let r = self.read(id)?;
        if r.as_ref().is_some_and(|r| r.task.project_id != p) {
            return Err(missing());
        }
        Ok(r)
    }
    pub fn workspace(&self, p: Uuid, id: Uuid) -> Result<Option<PathBuf>> {
        let Some(r) = self.get(p, id)? else {
            return Ok(None);
        };
        if r.progress.phase != "done" {
            return Err(Error::Refused(format!(
                "task workspace setup is {}",
                r.progress.phase
            )));
        }
        Ok(Some(r.task.project_path))
    }
    pub(super) fn lifecycle(&self, p: Uuid, id: Uuid) -> Result<Arc<Lifecycle>> {
        self.get(p, id)?;
        let mut jobs = self.jobs.lock().unwrap();
        let (owner, life) = jobs.entry(id).or_insert_with(|| (p, Arc::default()));
        if *owner != p {
            return Err(missing());
        }
        Ok(life.clone())
    }
    pub fn find(&self, p: Uuid, path: &Path) -> Result<Option<Task>> {
        if !path.is_absolute()
            || path
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            return Ok(None);
        }
        let named = path;
        let mut ancestor = path;
        let real = loop {
            match ancestor.canonicalize() {
                Ok(existing) => {
                    break existing.join(path.strip_prefix(ancestor).map_err(|_| denied())?);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor.parent().ok_or_else(denied)?
                }
                Err(e) => return Err(e.into()),
            }
        };
        let mut best: Option<Task> = None;
        let files = match fs::read_dir(&self.dir) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        for file in files {
            let f = file?.path();
            let Some(id) = f
                .file_stem()
                .and_then(|v| v.to_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            if let Some(r) = self.read(id)?
                && r.task.project_id == p
                && r.progress.phase == "done"
                && (real.starts_with(&r.task.root) || named.starts_with(&r.task.root))
                && best
                    .as_ref()
                    .is_none_or(|b| r.task.root.components().count() > b.root.components().count())
            {
                best = Some(r.task);
            }
        }
        Ok(best)
    }
    fn save(&self, r: &Record, life: &Lifecycle) -> Result<()> {
        setup::write(&self.path(r.id), r)?;
        life.updates.send_replace(Some(r.progress.clone()));
        Ok(())
    }
}

fn ensure_directory(app: &Workbench, path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(denied());
    }
    app.allowed(path)?;
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && path.canonicalize()? == path => Ok(()),
        Ok(_) => Err(denied()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure_directory(app, path.parent().ok_or_else(denied)?)?;
            match fs::create_dir(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure_directory(app, path)
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}
fn member(task: &Task, path: &Path, name: String) -> Result<Member> {
    let rel = path.strip_prefix(&task.anchor).map_err(|_| {
        Error::Refused("repository lies outside the task's original common directory".into())
    })?;
    Ok(Member {
        name,
        repository_path: path.into(),
        worktree_path: task.root.join(rel),
        worktree_id: Uuid::new_v4(),
        ready: false,
    })
}
fn plan(app: &Workbench, id: Uuid, project: Project, q: Create) -> Result<Record> {
    if app.0.chats.saved_info(id).is_ok() {
        return Err(Error::Refused("a chat already owns this ID".into()));
    }
    if q.branch.is_empty()
        || q.branch.starts_with('-')
        || q.base.is_empty()
        || q.base.starts_with('-')
    {
        return Err(Error::Refused(
            "expected a literal branch and base reference".into(),
        ));
    }
    let dir = app.destination(&q.directory)?;
    if fs::symlink_metadata(&dir).is_ok() {
        return Err(Error::Refused("task directory already exists".into()));
    }
    let tracked = folders::list(app, &project)?;
    let repos: Vec<_> = tracked.iter().filter(|f| f.state == "git").collect();
    if repos.is_empty() {
        return Err(Error::Refused(
            "the project has no tracked Git folders for a multi-repository task".into(),
        ));
    }
    let mut anchor = project.root.as_path();
    for folder in &repos {
        while !folder.path.starts_with(anchor) {
            anchor = anchor.parent().ok_or_else(denied)?;
        }
        if dir.starts_with(&folder.path) || folder.path.starts_with(&dir) {
            return Err(denied());
        }
    }
    if dir.starts_with(&project.root) || project.root.starts_with(&dir) {
        return Err(denied());
    }
    let mut task = Task {
        root: dir.clone(),
        project_id: project.id,
        project_root: project.root.clone(),
        anchor: anchor.into(),
        project_path: dir.join(project.root.strip_prefix(anchor).unwrap()),
        project_is_worktree: project.root.join(".git").exists(),
        repositories: vec![],
    };
    if task.project_is_worktree {
        task.repositories
            .push(member(&task, &project.root, project.name)?);
    }
    let requested: HashSet<PathBuf> = match &q.repositories {
        Some(paths) => paths
            .iter()
            .map(|p| p.canonicalize())
            .collect::<std::io::Result<_>>()?,
        None if repos.len() <= 8 => repos.iter().map(|f| f.path.clone()).collect(),
        None => HashSet::new(),
    };
    if requested
        .iter()
        .any(|p| !repos.iter().any(|f| f.path == *p))
    {
        return Err(Error::Refused(
            "requested repository is not tracked by this project".into(),
        ));
    }
    for f in repos.into_iter().filter(|f| requested.contains(&f.path)) {
        task.repositories
            .push(member(&task, &f.path, f.name.clone())?);
    }
    task.repositories
        .sort_by_key(|m| m.repository_path.components().count());
    let progress = Snapshot::new(id, &q.branch, &q.base, &task.project_path);
    Ok(Record {
        id,
        request: q,
        task,
        progress,
    })
}
#[cfg(unix)]
fn link_plain(app: &Workbench, source: &Path, dest: &Path, repos: &[PathBuf]) -> Result<()> {
    ensure_directory(app, dest)?;
    for file in fs::read_dir(source)? {
        let file = file?;
        let from = file.path();
        let to = dest.join(file.file_name());
        if file.file_name() == ".git" || file.file_name() == ".kar-task-owner" {
            continue;
        }
        if repos.contains(&from) {
            continue;
        }
        let Ok(real) = from.canonicalize() else {
            continue;
        };
        if app.allowed(&real).is_err() {
            continue;
        }
        if repos.iter().any(|r| r.starts_with(&from)) && file.file_type()?.is_dir() {
            link_plain(app, &from, &to, repos)?;
        } else {
            match fs::symlink_metadata(&to) {
                Ok(meta) if meta.is_symlink() && fs::read_link(&to)? == from => {}
                Ok(_) => {
                    return Err(Error::Refused(format!(
                        "task path already holds files: {}",
                        to.display()
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::os::unix::fs::symlink(&from, &to)?
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn link_plain(_: &Workbench, _: &Path, _: &Path, _: &[PathBuf]) -> Result<()> {
    Err(Error::Unsupported(
        "linking non-repository project files on this platform".into(),
    ))
}

fn claim(app: &Workbench, r: &Record) -> Result<()> {
    let t = &r.task;
    if t.project_is_worktree && t.project_path == t.root {
        return Ok(());
    }
    let marker = t.root.join(".kar-task-owner");
    if app.destination(&t.root)? != t.root {
        return Err(denied());
    }
    match fs::create_dir(&t.root) {
        Ok(()) => {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&marker)?;
            use std::io::Write;
            write!(file, "{}", r.id)?;
            file.sync_all()?;
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::AlreadyExists
                && !marker.is_symlink()
                && fs::read_to_string(&marker).ok().as_deref() == Some(&r.id.to_string()) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn checkout(
    app: &Workbench,
    entry: &Member,
    q: &Create,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    if entry.repository_path.canonicalize()? != entry.repository_path {
        return Err(denied());
    }
    app.allowed(&entry.repository_path)?;
    ensure_directory(app, entry.worktree_path.parent().ok_or_else(denied)?)?;
    let repo = Repository::open(&entry.repository_path)?;
    if repo.root() != entry.repository_path {
        return Err(Error::Refused(
            "expected an independent repository root".into(),
        ));
    }
    if !crate::workbench_git::git(
        repo.root(),
        &["rev-parse", "--show-superproject-working-tree"],
    )?
    .is_empty()
    {
        return Err(Error::Refused(
            "a submodule follows its parent repository".into(),
        ));
    }
    let from = q
        .base
        .trim_start_matches("refs/heads/")
        .trim_start_matches("origin/");
    let local = format!("refs/heads/{from}");
    let remote = format!("refs/remotes/origin/{from}");
    let start = if from == "HEAD" {
        "HEAD"
    } else if repo.ref_exists(&local)? {
        &local
    } else if repo.ref_exists(&remote)? {
        &remote
    } else {
        "HEAD"
    };
    repo.create_worktree_for(
        entry.worktree_id,
        &app.destination(&entry.worktree_path)?,
        &q.branch,
        start,
        true,
        cancel,
    )?;
    Ok(())
}
fn apply(app: &Workbench, r: &mut Record, l: &Lifecycle, cancel: &AtomicBool) -> Result<()> {
    let store = &app.0.task_folders;
    claim(app, r)?;
    let names = r
        .task
        .repositories
        .iter()
        .map(|m| m.repository_path.clone())
        .collect::<Vec<_>>();
    if !r.task.project_is_worktree {
        link_plain(app, &r.task.project_root, &r.task.project_path, &names)?;
    }
    for i in 0..r.task.repositories.len() {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Closed("worktree setup cancelled".into()));
        }
        let m = &r.task.repositories[i];
        r.progress.advance(
            format!("Checking out {}", m.name),
            (i * 100 / names.len()) as u8,
        );
        store.save(r, l)?;
        checkout(app, m, &r.request, Some(cancel))?;
        r.task.repositories[i].ready = true;
        store.save(r, l)?;
    }
    Ok(())
}
fn rollback(app: &Workbench, r: &Record) -> Result<()> {
    // Only an owned checkout can be removed. The Git registry preserves edited worktrees.
    for m in r.task.repositories.iter().rev() {
        if !m.ready {
            if !m.worktree_path.join(".git").is_file() {
                continue;
            }
            checkout(app, m, &r.request, None)?;
        }
        let repo = Repository::open(&m.repository_path)?;
        repo.remove_worktree_if(m.worktree_id, |path| {
            if path != m.worktree_path || app.destination(path)? != path {
                return Err(denied());
            }
            Ok(())
        })?;
    }
    if !r.task.project_is_worktree {
        fn unlink(source: &Path, dest: &Path, planned: &[Member]) -> Result<()> {
            if dest.is_symlink() || !dest.is_dir() {
                return Ok(());
            }
            for child in fs::read_dir(dest)? {
                let child = child?;
                let path = child.path();
                let original = source.join(child.file_name());
                if child.file_type()?.is_symlink() && fs::read_link(&path)? == original {
                    fs::remove_file(path)?;
                } else if child.file_type()?.is_dir()
                    && original.is_dir()
                    && planned
                        .iter()
                        .any(|m| m.repository_path.starts_with(&original))
                {
                    unlink(&original, &path, planned)?;
                }
            }
            let _ = fs::remove_dir(dest);
            Ok(())
        }
        unlink(
            &r.task.project_root,
            &r.task.project_path,
            &r.task.repositories,
        )?;
    }
    let marker = r.task.root.join(".kar-task-owner");
    if !marker.is_symlink()
        && fs::read_to_string(&marker).ok().as_deref() == Some(&r.id.to_string())
    {
        fs::remove_file(marker)?;
        let _ = fs::remove_dir(&r.task.root);
    }
    Ok(())
}
pub(super) fn create(app: &Workbench, p: Uuid, id: Uuid, q: Create) -> Result<Task> {
    let store = &app.0.task_folders;
    let life = store.lifecycle(p, id)?;
    let cancel = life.begin()?;
    let operation = (|| {
        let controller = app.controller(id);
        let chat = controller.owner.blocking_lock();
        let mut r = match store.get(p, id)? {
            Some(r) if r.request != q => {
                return Err(Error::Refused(
                    "task ID belongs to different setup inputs".into(),
                ));
            }
            Some(r) if r.progress.phase == "done" => return Ok(r.task),
            Some(r) if r.progress.phase == "cancelled" => {
                return Err(Error::Refused("cancelled task ID is retired".into()));
            }
            Some(r) => r,
            None => plan(app, id, app.project(p)?, q)?,
        };
        if r.progress.phase != "running" {
            r.progress =
                Snapshot::new(id, &r.request.branch, &r.request.base, &r.task.project_path);
        }
        store.save(&r, &life)?;
        drop(chat);
        if app.project(p)?.root != r.task.project_root
            || app.destination(&r.task.root)? != r.task.root
        {
            return Err(denied());
        }
        if store.cancelled_path(id).try_exists()? {
            cancel.store(true, Ordering::SeqCst);
        }
        let outcome = if cancel.load(Ordering::SeqCst) {
            Err(Error::Closed("worktree setup cancelled".into()))
        } else {
            apply(app, &mut r, &life, &cancel)
        };
        let mut gate = life.running.lock().unwrap();
        if cancel.load(Ordering::SeqCst) {
            let cleanup = rollback(app, &r);
            r.progress.finish(
                if cleanup.is_ok() {
                    "cancelled"
                } else {
                    "failed"
                },
                cleanup.as_ref().err().map(|e| e.to_string()).as_deref(),
            );
            store.save(&r, &life)?;
            *gate = None;
            return cleanup.and(Err(Error::Closed("worktree setup cancelled".into())));
        }
        r.progress.finish(
            if outcome.is_ok() { "done" } else { "failed" },
            outcome.as_ref().err().map(|e| e.to_string()).as_deref(),
        );
        store.save(&r, &life)?;
        *gate = None;
        outcome.map(|()| r.task)
    })();
    let mut gate = life.running.lock().unwrap();
    if gate
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(active, &cancel))
    {
        *gate = None;
    }
    operation
}

pub(super) fn add(app: &Workbench, p: Uuid, id: Uuid, src: PathBuf) -> Result<Task> {
    let store = &app.0.task_folders;
    let life = store.lifecycle(p, id)?;
    let gate = life.running.lock().unwrap();
    if gate.is_some() {
        return Err(Error::Busy);
    }
    let mut r = store.get(p, id)?.ok_or_else(missing)?;
    if r.progress.phase != "done" {
        return Err(Error::Busy);
    }
    let src = src.canonicalize()?;
    let f = folders::list(app, &app.project(p)?)?
        .into_iter()
        .find(|f| f.path == src && f.state == "git")
        .ok_or_else(|| Error::Refused("repository is not a tracked Git folder".into()))?;
    let pending = r
        .task
        .repositories
        .iter()
        .position(|m| m.repository_path == src);
    if pending.is_some_and(|i| r.task.repositories[i].ready) {
        return Err(Error::Refused(
            "repository already belongs to this task".into(),
        ));
    }
    let mut m = match pending {
        Some(i) => r.task.repositories[i].clone(),
        None => member(&r.task, &src, f.name)?,
    };
    let to = &m.worktree_path;
    // Open actual directory components in a plain mirrored project without writing through
    // a link to the source. Other siblings keep pointing at their original files.
    if !r.task.project_is_worktree && src.starts_with(&r.task.project_root) {
        let mut original = r.task.project_root.clone();
        let mut dest = r.task.project_path.clone();
        let components = src
            .strip_prefix(&original)
            .unwrap()
            .components()
            .collect::<Vec<_>>();
        for part in &components[..components.len().saturating_sub(1)] {
            original.push(part.as_os_str());
            dest.push(part.as_os_str());
            if dest.is_symlink() {
                if fs::read_link(&dest)? != original {
                    return Err(denied());
                }
                fs::remove_file(&dest)?;
                link_plain(app, &original, &dest, std::slice::from_ref(&src))?;
            }
            ensure_directory(app, &dest)?;
        }
    }
    let linked = to.is_symlink() && fs::read_link(to)? == src && !r.task.project_is_worktree;
    if pending.is_none() && fs::symlink_metadata(to).is_ok() && !linked {
        return Err(Error::Refused(
            "task destination already holds files".into(),
        ));
    }
    if pending.is_none() {
        r.task.repositories.push(m.clone());
        store.save(&r, &life)?;
    }
    if linked {
        fs::remove_file(to)?;
    }
    if let Err(e) = checkout(app, &m, &r.request, None) {
        #[cfg(unix)]
        if linked && fs::symlink_metadata(to).is_err() {
            let _ = std::os::unix::fs::symlink(&src, to);
        }
        return Err(e);
    }
    m.ready = true;
    let i = r
        .task
        .repositories
        .iter()
        .position(|row| row.repository_path == src)
        .unwrap();
    r.task.repositories[i] = m;
    store.save(&r, &life)?;
    Ok(r.task)
}
