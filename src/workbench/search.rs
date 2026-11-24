use super::{Project, Workbench, files::Workspace};
use crate::{
    Error, Result,
    chat::{Item, Role},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Paths,
    Content,
    Messages,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hit {
    Path {
        project: Uuid,
        path: PathBuf,
    },
    Content {
        project: Uuid,
        path: PathBuf,
        line: usize,
        text: String,
    },
    Message {
        project: Uuid,
        chat: Uuid,
        turn: Uuid,
        item: usize,
        role: Role,
        text: String,
    },
}

impl Workbench {
    pub fn search(
        &self,
        scope: Option<Uuid>,
        kind: SearchKind,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || !(1..=500).contains(&limit) {
            return Err(Error::Refused(
                "specify a search query and a limit between 1 and 500".into(),
            ));
        }
        let projects = match scope {
            Some(id) => vec![self.project(id)?],
            None => self.projects(),
        };
        let mut hits = Vec::new();
        for project in projects {
            if matches!(kind, SearchKind::Messages) {
                for binding in self.chats(project.id, true)? {
                    let h = self.0.chats.history(binding.info.id)?;
                    for turn in h.turns {
                        for (item, entry) in turn.items.into_iter().enumerate() {
                            if let Item::Message { role, text, .. } = entry.item
                                && text.to_lowercase().contains(&query)
                            {
                                hits.push(Hit::Message {
                                    project: project.id,
                                    chat: binding.info.id,
                                    turn: turn.id,
                                    item,
                                    role,
                                    text,
                                });
                                if hits.len() == limit {
                                    return Ok(hits);
                                }
                            }
                        }
                    }
                }
            } else {
                let workspace = self.workspace(&project)?;
                search_files(&workspace, &project, kind, &query, limit, &mut hits)?;
                if hits.len() == limit {
                    break;
                }
            }
        }
        Ok(hits)
    }
}

fn search_files(
    workspace: &Workspace,
    project: &Project,
    kind: SearchKind,
    query: &str,
    limit: usize,
    hits: &mut Vec<Hit>,
) -> Result<()> {
    let private = workspace.private.clone();
    let mut walk = ignore::WalkBuilder::new(&workspace.root);
    walk.hidden(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            !e.file_name().eq_ignore_ascii_case(".git") && !e.path().starts_with(&private)
        });
    for entry in walk.build() {
        let entry = entry.map_err(|e| Error::Protocol(e.to_string()))?;
        if !entry.file_type().is_some_and(|f| f.is_file()) {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(&workspace.root)
            .expect("workspace walk");
        match kind {
            SearchKind::Paths if path.to_string_lossy().to_lowercase().contains(query) => {
                hits.push(Hit::Path {
                    project: project.id,
                    path: path.into(),
                });
            }
            SearchKind::Content => {
                let text = match workspace.read(Path::new(path)) {
                    Ok(s) => s,
                    Err(Error::Refused(_)) => continue,
                    Err(Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                for (line, text) in text.lines().enumerate() {
                    if text.to_lowercase().contains(query) {
                        hits.push(Hit::Content {
                            project: project.id,
                            path: path.into(),
                            line: line + 1,
                            text: text.into(),
                        });
                        if hits.len() == limit {
                            return Ok(());
                        }
                    }
                }
            }
            _ => {}
        }
        if hits.len() == limit {
            break;
        }
    }
    Ok(())
}
