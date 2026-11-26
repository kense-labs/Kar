use crate::{
    Error, Result,
    workbench::{Project, Workbench, denied},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub(in super::super) struct Tracked {
    pub path: PathBuf,
    pub name: String,
}
#[derive(Clone, Serialize)]
pub(super) struct Folder {
    pub name: String,
    pub path: PathBuf,
    pub state: &'static str,
    pub discovered: bool,
}
#[derive(Serialize)]
pub(super) struct Scanned {
    path: PathBuf,
    name: String,
    is_git_repository: bool,
    source: &'static str,
}
#[derive(Serialize)]
pub(super) struct Scan {
    workspace_root: PathBuf,
    is_git_repository: bool,
    code_workspace_file: Option<PathBuf>,
    folders: Vec<Scanned>,
}

fn children(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut names = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}
fn label(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .take(128)
        .collect::<String>()
        .trim()
        .into()
}
fn visible(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    !name.starts_with('.') && name != "node_modules"
}
fn owns(path: &Path) -> bool {
    path.join(".git").is_dir()
}
// VS Code workspace files allow C-style comments and trailing commas outside strings.
fn document(data: &[u8]) -> Option<Value> {
    let mut clean = Vec::new();
    json_comments::StripComments::with_settings(json_comments::CommentSettings::c_style(), data)
        .read_to_end(&mut clean)
        .ok()?;
    let mut quoted = false;
    let mut escaped = false;
    let mut comma = None;
    for i in 0..clean.len() {
        let b = clean[i];
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
            continue;
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        if matches!(b, b']' | b'}')
            && let Some(j) = comma
        {
            clean[j] = b' ';
        }
        comma = (b == b',').then_some(i);
        quoted = b == b'"';
    }
    serde_json::from_slice(&clean).ok()
}
fn gitmodules(data: &[u8]) -> HashSet<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            (key.trim() == "path").then(|| {
                PathBuf::from(
                    value
                        .trim()
                        .trim_matches('"')
                        .replace('\\', "/")
                        .trim_end_matches('/'),
                )
            })
        })
        .collect()
}
pub(super) fn scan(app: &Workbench, project: &Project, root: &Path) -> Result<Scan> {
    let root = root.canonicalize()?;
    let ws = app.workspace(&Project {
        root: root.clone(),
        ..project.clone()
    })?;
    let paths = children(&root)?;
    let file = paths
        .iter()
        .find(|p| p.extension().is_some_and(|s| s == "code-workspace"));
    let workspace = file
        .and_then(|p| {
            ws.read_bytes(Path::new(p.file_name()?), 4 * 1024 * 1024)
                .ok()
        })
        .and_then(|v| document(&v));
    let submodules = gitmodules(
        &ws.read_bytes(Path::new(".gitmodules"), 1024 * 1024)
            .unwrap_or_default(),
    );
    let mut candidates = vec![];
    for row in workspace
        .as_ref()
        .and_then(|v| v["folders"].as_array())
        .into_iter()
        .flatten()
    {
        let Some(path) = row["path"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let path = root.join(path);
        let name = row["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(128).collect())
            .unwrap_or_else(|| label(&path));
        candidates.push((path, name, "code-workspace"));
    }
    for path in paths.iter().filter(|p| visible(p)) {
        let Ok(dir) = path.canonicalize() else {
            continue;
        };
        if app.allowed(&dir).is_err() || !dir.is_dir() {
            continue;
        }
        if owns(&dir) {
            candidates.push((path.clone(), label(path), "child"));
            continue;
        }
        if dir.join(".git").exists() || path.is_symlink() {
            continue;
        }
        for child in children(&dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| visible(p))
        {
            if let Ok(real) = child.canonicalize()
                && app.allowed(&real).is_ok()
                && owns(&real)
            {
                candidates.push((child.clone(), label(&child), "child"));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut folders = vec![];
    for (path, name, source) in candidates {
        let Ok(real) = path.canonicalize() else {
            continue;
        };
        if !real.is_dir()
            || root.starts_with(&real)
            || app.allowed(&real).is_err()
            || !seen.insert(real.clone())
            || real
                .strip_prefix(&root)
                .is_ok_and(|rel| submodules.contains(rel))
        {
            continue;
        }
        folders.push(Scanned {
            is_git_repository: real.join(".git").exists(),
            path: real,
            name,
            source,
        });
    }
    Ok(Scan {
        workspace_root: root.clone(),
        is_git_repository: root.join(".git").exists(),
        code_workspace_file: workspace.and(file.cloned()),
        folders,
    })
}
pub(super) fn list(app: &Workbench, project: &Project) -> Result<Vec<Folder>> {
    let scanned = scan(app, project, &project.root)?;
    let ignored = app
        .workspace(project)?
        .read_bytes(Path::new("kar.json"), 1024 * 1024)
        .ok()
        .and_then(|b| document(&b))
        .and_then(|v| v["ignoreRepositories"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| project.root.join(v.as_str()?).canonicalize().ok())
        .collect::<Vec<_>>();
    let mut found: Vec<_> = scanned
        .folders
        .into_iter()
        .filter(|f| {
            f.path.starts_with(&project.root)
                && owns(&f.path)
                && !ignored.iter().any(|p| f.path.starts_with(p))
        })
        .map(|f| Folder {
            name: f.name,
            path: f.path,
            state: "git",
            discovered: true,
        })
        .collect();
    let known: HashSet<_> = found.iter().map(|f| f.path.as_path()).collect();
    let catalog = app.0.catalog.lock().unwrap();
    let added: Vec<_> = catalog
        .tracked_folders
        .get(&project.id)
        .into_iter()
        .flatten()
        .filter(|f| !known.contains(f.path.as_path()))
        .filter_map(|f| {
            if f.path.exists()
                && f.path
                    .canonicalize()
                    .ok()
                    .is_none_or(|p| p != f.path || app.allowed(&p).is_err())
            {
                return None;
            }
            Some(Folder {
                name: f.name.clone(),
                path: f.path.clone(),
                state: if f.path.join(".git").exists() {
                    "git"
                } else if f.path.is_dir() {
                    "plain"
                } else {
                    "missing"
                },
                discovered: false,
            })
        })
        .collect();
    found.truncate(128usize.saturating_sub(added.len()));
    found.extend(added);
    Ok(found)
}
pub(super) fn track(app: &Workbench, p: Uuid, tracked: Vec<Tracked>) -> Result<Vec<Folder>> {
    let project = app.project(p)?;
    let saved = normalize(app, &project, tracked)?;
    app.update(|c| {
        if c.projects.get(&p).map(|p| &p.root) != Some(&project.root) {
            return Err(Error::Busy);
        }
        c.tracked_folders.insert(p, saved);
        Ok(())
    })?;
    list(app, &project)
}
pub(in super::super) fn normalize(
    app: &Workbench,
    project: &Project,
    tracked: Vec<Tracked>,
) -> Result<Vec<Tracked>> {
    if tracked.len() > 128 {
        return Err(Error::Refused(
            "at most 128 tracked folders are supported".into(),
        ));
    }
    let mut seen = HashSet::new();
    let mut saved = vec![];
    for mut entry in tracked {
        entry.path = entry.path.canonicalize()?;
        app.allowed(&entry.path)?;
        if !entry.path.is_dir() || project.root.starts_with(&entry.path) {
            return Err(denied());
        }
        if entry.name.is_empty()
            || entry.name.trim() != entry.name
            || entry.name.chars().count() > 128
        {
            return Err(Error::Refused("invalid tracked folder name".into()));
        }
        if seen.insert(entry.path.clone()) {
            saved.push(entry);
        }
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_merges_workspace_entries_and_repository_wrappers_with_scope() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let auth = temp.path().join("allowed");
        let project = auth.join("project");
        let one = project.join("one");
        let two = project.join("wrapper/two");
        let extra = auth.join("outside-project");
        for path in [&one, &two, &extra] {
            fs::create_dir_all(path.join(".git"))?;
        }
        fs::create_dir(project.join("sub"))?;
        fs::write(
            project.join(".gitmodules"),
            "[submodule \"sub\"]\npath = sub\n",
        )?;
        fs::write(
            project.join("sample.code-workspace"),
            r#"{// project
            "folders":[{"path":"one","name":"First"},{"path":"../outside-project"},{"path":"sub"},],}"#,
        )?;
        let app = Workbench::open(
            crate::chat::ChatStore::open(temp.path().join("store"))?,
            &[auth],
        )?;
        let p = app.register(&project, "project")?;
        let scan = scan(&app, &p, &p.root)?;
        assert_eq!(scan.folders.len(), 3);
        assert_eq!(scan.folders[0].name, "First");
        assert_eq!(scan.folders[0].source, "code-workspace");
        assert_eq!(scan.folders[2].name, "two");
        let folders = track(
            &app,
            p.id,
            vec![Tracked {
                path: extra,
                name: "External".into(),
            }],
        )?;
        assert_eq!(
            folders.iter().map(|f| f.discovered).collect::<Vec<_>>(),
            [true, true, false]
        );
        Ok(())
    }
}
