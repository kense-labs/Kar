use super::*;
use crate::workbench::prepared_runs::{self, Input, Launch, Workspace};

pub(super) async fn run(
    app: &Workbench,
    grant: &Grant,
    access: &Access,
    project: Uuid,
    chat: Uuid,
    input: &Value,
) -> Result<Value> {
    let mut title = string(input, "title")?.trim().to_owned();
    if title.chars().count() > 200 || title.contains(['\n', '\r', '\0']) {
        return Err(refused("invalid chat title"));
    }
    if chat == grant.chat {
        return Err(refused("launch ID belongs to the calling chat"));
    }
    let scratch = input
        .get("scratch")
        .map(|v| v.as_bool().ok_or_else(|| refused("invalid scratch flag")))
        .transpose()?
        .unwrap_or(false);
    let project = if scratch {
        if !access.scratch || input.get("project_id").is_some() || input.get("workspace").is_some()
        {
            return Err(refused("scratch launch needs its own grant and workspace"));
        }
        app.ensure_scratch()?.id
    } else {
        let p = app.project(project)?;
        if !access.sees(grant, &p) || p.metadata.deleted {
            return Err(refused("launch project is outside the grant"));
        }
        project
    };
    let parent = app.binding(grant.project, grant.chat)?;
    let p = app.project(project)?;
    let runtime = input
        .get("runtime")
        .map(|_| string(input, "runtime"))
        .transpose()?
        .unwrap_or(&parent.runtime)
        .to_owned();
    let model = input
        .get("model")
        .map(|_| string(input, "model").map(str::to_owned))
        .transpose()?
        .or_else(|| {
            (runtime == parent.runtime)
                .then(|| parent.model.clone())
                .flatten()
        });
    let attachments: Vec<Uuid> = input
        .get("attachments")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let attachments = attachments
        .into_iter()
        .map(|id| app.0.chats.visible_attachment(grant.chat, id))
        .collect::<Result<Vec<_>>>()?;
    let message = input
        .get("message")
        .map(|v| v.as_str().ok_or_else(|| refused("invalid launch message")))
        .transpose()?;
    if input.get("requested_output_schema").is_some() && message.is_none() {
        return Err(refused("requested_output_schema needs a launch message"));
    }
    if input
        .get("generate_title")
        .cloned()
        .map(serde_json::from_value::<bool>)
        .transpose()?
        .unwrap_or(false)
    {
        let seed = message
            .filter(|v| !v.trim().is_empty())
            .or_else(|| attachments.first().map(|a| a.name.as_str()))
            .ok_or_else(|| refused("a generated title needs a message or attachment"))?;
        title = seed
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
    }
    let approvals = input
        .get("approvals")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or(parent.approvals);
    if parent.approvals == crate::ApprovalMode::Deny && approvals != parent.approvals {
        return Err(refused(
            "approval mode exceeds the calling chat's permissions",
        ));
    }
    let requested_mode = input
        .get("acp_mode")
        .map(|_| string(input, "acp_mode"))
        .transpose()?;
    if requested_mode.is_some() && requested_mode != parent.settings.acp_mode.as_deref() {
        return Err(refused(
            "native mode must remain within the calling chat's permissions",
        ));
    }
    let same_runtime = runtime == parent.runtime;
    if !same_runtime
        && (!parent.settings.disallowed_tools.is_empty()
            || !parent.settings.exclude_user_configuration.is_empty()
            || parent.settings.acp_mode.is_some())
    {
        return Err(refused(
            "the chosen runtime cannot inherit native restrictions from another provider",
        ));
    }
    let strategy = input
        .get("workspace")
        .map(|_| string(input, "workspace"))
        .transpose()?
        .unwrap_or("root");
    let workspace = match strategy {
        "root" => {
            if ["base_ref", "branch", "directory", "start_from_origin"]
                .iter()
                .any(|k| input.get(k).is_some())
            {
                return Err(refused("worktree options require a worktree launch"));
            }
            Workspace::Root
        }
        "existing_worktree" => {
            if ["base_ref", "branch", "start_from_origin"]
                .iter()
                .any(|k| input.get(k).is_some())
            {
                return Err(refused(
                    "an existing checkout does not take a base or a new branch",
                ));
            }
            Workspace::Existing {
                directory: PathBuf::from(string(input, "directory")?),
            }
        }
        "worktree" => {
            let dest = match input.get("directory") {
                Some(_) => access.directory(Path::new(string(input, "directory")?))?,
                None => {
                    let parent = access
                        .directories
                        .first()
                        .ok_or_else(|| refused("worktree launch needs a granted directory"))?;
                    access.directory(&parent.join(format!("kar-{chat}")))?
                }
            };
            let branch = match input.get("branch") {
                Some(_) => string(input, "branch")?.to_owned(),
                None => format!("kar-{chat}"),
            };
            let base = input
                .get("base_ref")
                .map(|_| string(input, "base_ref"))
                .transpose()?
                .unwrap_or("HEAD")
                .to_owned();
            let origin = input
                .get("start_from_origin")
                .map(|v| {
                    v.as_bool()
                        .ok_or_else(|| refused("invalid start_from_origin"))
                })
                .transpose()?
                .unwrap_or(
                    p.metadata
                        .settings
                        .get("newWorktreesStartFromOrigin")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                );
            Workspace::Worktree {
                directory: dest,
                branch,
                base,
                from_origin: origin,
            }
        }
        _ => return Err(refused("unsupported launch workspace")),
    };
    if !grant.active()
        || !super::super::issues::authorized(app, grant)
            .is_some_and(|p| p.executes() && &p.project_access == access)
    {
        return Err(refused("calling grant expired"));
    }
    let settings = crate::SessionSettings {
        effort: input
            .get("effort")
            .map(|_| string(input, "effort").map(str::to_owned))
            .transpose()?
            .or_else(|| {
                (runtime == parent.runtime)
                    .then(|| parent.settings.effort.clone())
                    .flatten()
            }),
        acp_mode: parent.settings.acp_mode.clone(),
        disallowed_tools: parent.settings.disallowed_tools.clone(),
        exclude_user_configuration: parent.settings.exclude_user_configuration.clone(),
        // An explicit grant to trust one checkout does not grant trust to a new one.
        pi_project_trusted: match parent.settings.pi_project_trusted {
            Some(true)
                if project != grant.project || strategy != "root" || p.root != parent.workspace =>
            {
                Some(false)
            }
            other => other,
        },
        system_prompt: parent.settings.system_prompt.clone(),
        append_system_prompt: parent.settings.append_system_prompt.clone(),
        end_turn_processes_on_abort: parent.settings.end_turn_processes_on_abort,
        ..Default::default()
    };
    let run = crate::chat::stable_id([chat.as_bytes().as_slice(), b"model-launch-message"]);
    let input = Input {
        launch: Some(Launch {
            runtime,
            model,
            approvals,
            settings,
        }),
        title: Some(title),
        workspace,
        message: message.map(str::to_owned),
        attachments: attachments.into_iter().map(|a| a.id).collect(),
        requested_output_schema: input.get("requested_output_schema").cloned(),
        defer_release: input
            .get("defer_release")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or(false),
    };
    let caller = prepared_runs::Caller {
        project: grant.project,
        chat: grant.chat,
        access: access.clone(),
    };
    prepared_runs::model_launch(app, project, chat, run, input, caller).await
}
