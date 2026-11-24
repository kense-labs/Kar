//! The host selects the script; a model can only request or skip that configured script.
use super::*;
use crate::workbench::Project;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Script {
    id: String,
    name: String,
    command: String,
    terminal: Uuid,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Outcome {
    Skipped,
    NoScript,
    Started { terminal: Uuid, name: String },
    Unconfirmed { terminal: Uuid, name: String },
    Failed { detail: String },
}
fn selected(project: &Project) -> Option<&Value> {
    project
        .metadata
        .settings
        .get("scripts")?
        .as_array()?
        .iter()
        .find(|v| v["runOnWorktreeCreate"] == true)
}
impl Script {
    pub(super) fn select(project: &Project) -> Option<Self> {
        let s = selected(project)?;
        Some(Self {
            id: s["id"].as_str()?.into(),
            name: s["name"].as_str()?.into(),
            command: s["command"].as_str()?.into(),
            terminal: Uuid::new_v4(),
        })
    }
}

pub(super) fn run(app: &Workbench, row: &Move, workspace: &Path) -> Outcome {
    if !row.request.run_setup_script {
        return Outcome::Skipped;
    }
    let Some(script) = &row.script else {
        return Outcome::NoScript;
    };
    let allowed = {
        let db = app.0.catalog.lock().unwrap();
        directory_ok(Policy::current(&db, row.project, row.chat), &row.directory)
            && db
                .projects
                .get(&row.project)
                .and_then(selected)
                .is_some_and(|s| s["id"] == script.id && s["command"] == script.command)
    };
    if !allowed {
        return Outcome::Skipped;
    }
    let result = app.start_task_script(
        row.project,
        row.request.command_id,
        workspace,
        script.terminal,
        &script.command,
    );
    match result {
        Ok(true) => Outcome::Started {
            terminal: script.terminal,
            name: script.name.clone(),
        },
        Ok(false) => Outcome::Unconfirmed {
            terminal: script.terminal,
            name: script.name.clone(),
        },
        Err(e) => Outcome::Failed {
            detail: e.to_string(),
        },
    }
}
