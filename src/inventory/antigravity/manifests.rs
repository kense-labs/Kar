use super::*;
use std::collections::HashSet;
use tokio::fs;

pub(super) async fn allowed(path: &Path, root: Option<&Path>) -> bool {
    match root {
        Some(root) => fs::canonicalize(path)
            .await
            .is_ok_and(|p| p.starts_with(root)),
        None => true,
    }
}

pub(super) async fn read(path: &Path, limit: u64) -> Option<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || crate::account::read_profile(&path, limit))
        .await
        .ok()?
        .ok()?
}
pub(super) async fn json(path: &Path) -> Option<Value> {
    let data: Value = serde_json::from_str(&read(path, 8 * 1024 * 1024).await?).ok()?;
    data.is_object().then_some(data)
}
pub(super) async fn dirs(path: &Path, root: Option<&Path>) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if !allowed(path, root).await {
        return list;
    }
    if let Ok(mut dir) = fs::read_dir(path).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if allowed(&path, root).await && fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
                list.push(path);
            }
        }
    }
    list.sort();
    list
}
fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}
fn normalized(path: PathBuf) -> PathBuf {
    let mut output = PathBuf::new();
    for part in path.components() {
        if part == std::path::Component::ParentDir {
            output.pop();
        } else {
            output.push(part);
        }
    }
    output
}

struct Manifest {
    path: PathBuf,
    include: Option<HashSet<String>>,
    exclude: HashSet<String>,
}
impl Manifest {
    fn inherited(&self, path: PathBuf, entry: &Value) -> Self {
        let only = strings(&entry["include_only"]);
        let include = if only.is_empty() {
            self.include.clone()
        } else {
            Some(
                only.into_iter()
                    .filter(|s| self.include.as_ref().is_none_or(|v| v.contains(*s)))
                    .map(str::to_owned)
                    .collect(),
            )
        };
        let mut exclude = self.exclude.clone();
        exclude.extend(strings(&entry["exclude"]).into_iter().map(str::to_owned));
        Self {
            path,
            include,
            exclude,
        }
    }
    fn accepts(&self, child: &Path) -> bool {
        child.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
            !self.exclude.contains(s) && self.include.as_ref().is_none_or(|v| v.contains(s))
        })
    }
}

pub(super) async fn manifest(file: PathBuf, base: &Path, root: Option<&Path>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![Manifest {
        path: file,
        include: None,
        exclude: HashSet::new(),
    }];
    let mut visited = HashSet::new();
    while let Some(current) = stack.pop() {
        let Ok(real) = fs::canonicalize(&current.path).await else {
            continue;
        };
        if root.is_some_and(|r| !real.starts_with(r)) || !visited.insert(real.clone()) {
            continue;
        }
        let Some(data) = json(&real).await else {
            continue;
        };
        let resolve = async |entry: &Value| {
            let path = entry["path"].as_str()?;
            let local = normalized(base.join(path));
            let path = if let Some(root) = root
                && !fs::try_exists(&local).await.unwrap_or(false)
            {
                normalized(root.join(path))
            } else {
                local
            };
            allowed(&path, root).await.then_some(path)
        };
        for entry in data["entries"].as_array().into_iter().flatten() {
            let Some(parent) = resolve(entry).await else {
                continue;
            };
            let only = strings(&entry["include_only"]);
            let excluded = strings(&entry["exclude"]);
            let candidates = if only.is_empty() {
                dirs(&parent, root)
                    .await
                    .into_iter()
                    .filter(|p| {
                        !p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| excluded.contains(&n))
                    })
                    .collect()
            } else {
                only.into_iter()
                    .filter(|name| !excluded.contains(name))
                    .map(|name| {
                        normalized(parent.join(name.trim_start_matches(std::path::is_separator)))
                    })
                    .collect::<Vec<_>>()
            };
            for child in candidates {
                if current.accepts(&child) && allowed(&child, root).await {
                    result.push(child);
                }
            }
        }
        for entry in data["inherits"].as_array().into_iter().flatten().rev() {
            if let Some(path) = resolve(entry).await {
                stack.push(current.inherited(path, entry));
            }
        }
    }
    result
}
