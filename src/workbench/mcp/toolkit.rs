use super::*;
use crate::{
    Error, Result,
    chat::{Item, QueueAction},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Map;

pub(super) struct Tool {
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) properties: Value,
    pub(super) required: &'static [&'static str],
}
impl Tool {
    fn public(&self) -> Value {
        json!({"name":self.name,"description":self.description,"inputSchema":{"type":"object",
            "additionalProperties":false,"properties":self.properties,"required":self.required}})
    }
}
fn tools(caller: &Caller) -> Vec<Tool> {
    let id = json!({"type":"string","format":"uuid"});
    let cursor = json!({"type":"integer","minimum":0});
    let limit = json!({"type":"integer","minimum":1,"maximum":100});
    let queue_write = !caller.plan && (caller.grant.is_none() || caller.policy.as_ref()
        .is_some_and(|p| p.executes() && p.thread_tools.as_ref().is_some_and(|s| s.write)));
    let queue_actions = if queue_write {vec!["list","read","edit","cancel","reorder","promote_to_steer"]} else {vec!["list","read"]};
    let mut tools = vec![
        Tool {
            name: "orchestrator_capabilities",
            description: "Show this caller's available Kar workbench tools and local project or chat scope.",
            properties: json!({}),
            required: &[],
        },
        Tool {
            name: "kar_project_list",
            description: "List visible projects with their workspace settings, in pages.",
            properties: json!({"cursor":cursor,"limit":limit}),
            required: &[],
        },
        Tool {
            name: "kar_project_read",
            description: "Read a project's workspace and saved scripts.",
            properties: json!({"project_id":id}),
            required: &["project_id"],
        },
        Tool {
            name: "kar_thread_list",
            description: "List chats with titles, latest run, status and snooze state.",
            properties: json!({"project_id":id,"title_contains":{"type":"string"},"settled":{"type":"boolean"},"snoozed":{"type":"boolean"},"include_archived":{"type":"boolean"},"cursor":cursor,"limit":limit}),
            required: &[],
        },
        Tool {
            name: "kar_thread_read",
            description: "Read a chat's messages or timeline activities by stable item ID or position.",
            properties: json!({"project_id":id,"thread_id":id,"view":{"enum":["messages","activity"]},"item_id":{"type":"string"},"after_position":{"type":"integer","minimum":-1},"before_position":cursor,"text_offset":cursor,"max_chars_per_item":{"type":"integer","minimum":1,"maximum":20000},"limit":limit}),
            required: &[],
        },
        Tool {
            name: "kar_queue",
            description: "Read queued inputs in send order. Edit, cancel or reorder a pending input with a command ID.",
            properties: json!({"project_id":id,"thread_id":id,"action":{"enum":queue_actions},"turn_id":id,"request_id":id,"input_id":id,"command_id":id,"before":{"type":["string","null"],"format":"uuid"},"text":{"type":"string"},"cursor":cursor,"limit":limit}),
            required: &["action"],
        },
        Tool {
            name: "tool_search",
            description: "Find workbench tools by name or description and load them into this provider session.",
            properties: json!({"query":{"type":"string"}}),
            required: &["query"],
        },
    ];
    if caller.policy.as_ref().is_some_and(Policy::reads_issues) {
        tools.push(Tool {
            name: "kar_list_issues",
            description: "Search one page of issues in this chat's authorized tracker. Pass the returned cursor for another page.",
            properties: json!({"project_id":id,"thread_id":id,"state":{"enum":["open","closed","all"]},"query":{"type":"string"},"limit":limit,"cursor":{"type":"string"}}),
            required: &[],
        });
        tools.push(Tool {
            name: "kar_read_issue", description: "Read an issue's details, comments and available states in this chat's authorized tracker.",
            properties: json!({"project_id":id,"thread_id":id,"number":{"type":"integer","minimum":1}}),
            required: &["number"],
        });
    }
    if !caller.plan && caller.policy.as_ref().is_some_and(Policy::writes_issues) {
        tools.push(Tool {
            name: "kar_set_issue_state", description: "Close or reopen an issue in this chat's authorized tracker. Reuse command_id on retries.",
            properties: json!({"command_id":id,"project_id":id,"thread_id":id,"number":{"type":"integer","minimum":1},"state":{"enum":["open","closed"]}}),
            required: &["command_id","number","state"],
        });
        let text = json!({"type":"string"});
        let names = json!({"type":"array","items":text});
        tools.push(Tool {
            name: "kar_create_issue",
            description: "Create an issue in this chat's authorized project tracker. Reuse command_id on retries.",
            properties: json!({"command_id":id,"project_id":id,"thread_id":id,"title":text,"body":text,"labels":names,"assignees":names}),
            required: &["command_id", "title"],
        });
        tools.push(Tool {
            name: "kar_comment_on_issue",
            description: "Post a comment on a numbered issue in this chat's authorized project tracker. Reuse command_id on retries.",
            properties: json!({"command_id":id,"project_id":id,"thread_id":id,"number":{"type":"integer","minimum":1},"body":text}),
            required: &["command_id", "number", "body"],
        });
    }
    if caller.grant.is_some() {
        tools.push(Tool{name:"kar_worktree_list",description:"List the local and remote Git refs visible from this chat's checkout, including checkout paths.",properties:json!({"project_id":id,"thread_id":id,"query":{"type":"string"},"kind":{"enum":["all","local","remote"]},"include_matching_remote_refs":{"type":"boolean"},"cursor":cursor,"limit":limit}),required:&[]});
        tools.push(Tool{name:"kar_worktree_status",description:"Read this chat's current workspace attachment and its last managed handoff.",properties:json!({"project_id":id,"thread_id":id}),required:&[]});
        if !caller.plan && caller.policy.as_ref().and_then(Policy::worktree_directory).is_some(){
            tools.push(Tool{name:"kar_worktree_handoff",description:"Move this chat to a new branch in its granted worktree directory after its current turn. Reuse command_id on retries. A continuation is admitted once in the new workspace.",
                properties:json!({"project_id":id,"thread_id":id,"command_id":id,"branch":{"type":"string"},"base_ref":{"type":"string"},"start_from_origin":{"type":"boolean"},"run_setup_script":{"type":"boolean"},"continuation_prompt":{"type":"string"}}),required:&["command_id","branch"]});
        }
    }
    if let Some(policy) = caller.policy.as_ref().filter(|p| p.reads_workflows()) {
        let mut actions = vec!["definitions","list","read","questions","hooks","store"];
        if !caller.plan && policy.writes_workflows() { actions.extend(["start","resume","cancel","stop","skip","signal","reconcile","store_set","store_delete"]); }
        tools.push(Tool {
            name:"kar_workflow",
            description:"Inspect durable project workflows and runs. Start a saved workflow, retry a failed step, answer a question, reconcile an interrupted attempt, stop, skip or cancel. Reuse command_id on retries.",
            properties:json!({"project_id":id,"thread_id":id,"action":{"enum":actions},"workflow_id":id,"run_id":id,"parent_run_id":id,"reason":{"type":"string"},"key":{"type":"string"},"channel":{"type":"string"},"command_id":id,"attempt_id":id,"input":{},"value":{},"succeeded":{"type":"boolean"},"evidence":{"type":"string"},"status":{"enum":["ready","running","asking","waiting","reconcile","failed","cancelled","stopped","skipped","completed"]},"cursor":cursor,"limit":limit}),
            required:&["action"],
        });
    }
    if caller.grant.as_ref().is_some_and(|g| g.supports_background_wait()) {
        tools.push(Tool {
            name:"kar_background_wait",
            description:"Mark or clear waits for a live background task in this chat. Antigravity holds its running command or scheduled wake.",
            properties:json!({"task_ids":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"string","minLength":1}},"waiting":{"type":"boolean"}}),
            required:&["task_ids"],
        });
    }
    tools.extend(super::collaboration_tools::tools(caller));
    tools.extend(super::orchestrator::tools(caller));
    tools.extend(super::operations::tools(caller));
    tools.extend(super::forge_tools::tools(caller));
    tools.extend(preview_tools::tools(caller));
    tools.extend(super::project_tools::tools(caller));
    tools
}
pub(super) fn enabled(name: &str, caller: &Caller, deferred: bool) -> bool {
    tools(caller).iter().any(|t| t.name == name)
        && (name != "tool_search" || (caller.grant.is_some() && deferred))
}
pub(super) fn list(caller: &Caller, deferred: bool) -> Vec<Value> {
    let loaded = caller.grant.as_ref().map(|g| g.loaded.lock().unwrap());
    tools(caller)
        .into_iter()
        .filter(|t| {
            if deferred {
                matches!(t.name, "tool_search" | "orchestrator_capabilities")
                    || loaded.as_ref().is_some_and(|s| s.contains(t.name))
            } else {
                t.name != "tool_search"
            }
        })
        .map(|t| t.public())
        .collect()
}
fn bad(s: &str) -> Error {
    Error::Refused(s.into())
}
fn args<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(Error::from)
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Page {
    cursor: usize,
    limit: Option<usize>,
}
pub(super) fn page(v: Vec<Value>, key: &str, start: usize, limit: Option<usize>) -> Result<Value> {
    let n = limit.unwrap_or(20);
    if n == 0 {
        return Err(bad("limit must be positive"));
    }
    let total = v.len();
    let end = start.saturating_add(n.min(100));
    Ok(
        json!({(key):v.into_iter().skip(start).take(n.min(100)).collect::<Vec<_>>(),"total":total,"next_cursor":(end<total).then_some(end)}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs {
    project_id: Uuid,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Threads {
    project_id: Option<Uuid>,
    title_contains: Option<String>,
    settled: Option<bool>,
    snoozed: Option<bool>,
    include_archived: bool,
    cursor: usize,
    limit: Option<usize>,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Read {
    project_id: Option<Uuid>,
    thread_id: Option<Uuid>,
    view: Option<String>,
    item_id: Option<String>,
    after_position: Option<i64>,
    before_position: Option<usize>,
    text_offset: usize,
    max_chars_per_item: Option<usize>,
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Queue {
    project_id: Option<Uuid>,
    thread_id: Option<Uuid>,
    action: String,
    input_id: Option<Uuid>,
    command_id: Option<Uuid>,
    before: Option<Uuid>,
    text: Option<String>,
    #[serde(default)]
    cursor: usize,
    limit: Option<usize>,
}
fn project_scope(c: &Caller, asked: Option<Uuid>) -> Result<Option<Uuid>> {
    if let Some(g) = &c.grant {
        if asked.is_some_and(|p| p != g.project) {
            return Err(bad("Project is outside this provider session"));
        }
        return Ok(Some(g.project));
    }
    Ok(asked)
}
pub(super) fn target(app: &Workbench, c: &Caller, p: Option<Uuid>, t: Option<Uuid>) -> Result<(Uuid, Uuid)> {
    let thread = t
        .or_else(|| c.grant.as_ref().map(|g| g.chat))
        .ok_or_else(|| bad("thread_id is required"))?;
    if c.grant.as_ref().is_some_and(|g| g.chat != thread) {
        return Err(bad("Chat is outside this provider session"));
    }
    let project = match project_scope(c, p)? {
        Some(p) => p,
        None => {
            let info = app.0.chats.saved_info(thread)?;
            let db = app.0.catalog.lock().unwrap();
            let mut project = None;
            for p in db
                .projects
                .values()
                .filter(|p| !p.metadata.deleted && app.allowed(&p.root).is_ok())
            {
                if app.owns_chat(&db, p, &info)? {
                    project = Some(p.id);
                    break;
                }
            }
            project.ok_or_else(super::super::missing)?
        }
    };
    app.binding(project, thread)?;
    Ok((project, thread))
}
pub(super) async fn call(
    app: Workbench,
    caller: Caller,
    name: &str,
    input: Value,
) -> Result<Value> {
    if super::project_tools::handles(name) { return super::project_tools::call(&app,&caller,name,input).await; }
    if name.starts_with("kar_preview_") || name.starts_with("kar_device_") || name.starts_with("kar_html_"){return preview_tools::call(&app,&caller,name,input).await;}
    if super::collaboration_tools::handles(name) { return super::collaboration_tools::call(&app,&caller,name,input).await; }
    if matches!(name, "kar_create_issue" | "kar_comment_on_issue" | "kar_set_issue_state" | "kar_list_issues" | "kar_read_issue") {
        return issues::call(app, caller, name, input).await;
    }
    if name=="kar_queue" && input["action"]=="promote_to_steer" {
        if caller.plan {return Err(bad("Queue promotion requires an execute grant"));}
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Promotion { project_id:Option<Uuid>, thread_id:Option<Uuid>, action:String, turn_id:Uuid, request_id:Option<Uuid>, input_id:Uuid, command_id:Uuid }
        let q:Promotion=args(input)?;
        debug_assert_eq!(q.action,"promote_to_steer");
        let (p,t)=super::operations::target(&app,&caller,q.project_id,q.thread_id,true)?;
        if let Some(g)=&caller.grant {super::operations::may_run(&app,p,g.project,g.chat,t)?;}
        return Ok(json!(super::super::interactions::steer_queued(app,p,t,q.input_id,q.command_id,crate::chat::SteerTarget{turn:q.turn_id,request_id:q.request_id}).await?));
    }
    if super::orchestrator::handles(name){return super::orchestrator::call(app,caller,name,input).await;}
    if super::operations::handles(name) {return super::operations::call(app,caller,name,input).await;}
    if super::forge_tools::handles(name) {return super::forge_tools::call(app,caller,name,input).await;}
    if name.starts_with("kar_worktree_") {return super::worktree::call(app,caller,name,input).await;}
    if name == "kar_background_wait" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Mark {
            task_ids: Vec<String>,
            waiting: Option<bool>,
        }
        let mark: Mark = args(input)?;
        let grant = caller
            .grant
            .as_ref()
            .filter(|g| g.supports_background_wait() && g.active())
            .ok_or_else(|| bad("This runtime needs a live background-task controller"))?;
        let request = crate::BackgroundWaitRequest {
            task_ids: mark.task_ids,
            waiting: mark.waiting.unwrap_or(true),
            started_at: Default::default(),
        };
        return Ok(json!(
            app.await_background_tasks(grant.project, grant.chat, request)
                .await?
        ));
    }
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || dispatch(&app, &caller, &name, input))
        .await
        .map_err(|e| Error::Closed(e.to_string()))?
}
fn dispatch(app: &Workbench, c: &Caller, name: &str, input: Value) -> Result<Value> {
    match name {
        "tool_search" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Search {
                query: String,
            }
            let search: Search = args(input)?;
            let query = search.query.to_ascii_lowercase();
            let terms: Vec<_> = query
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|s| !s.is_empty())
                .collect();
            let mut matches: Vec<_> = tools(c)
                .into_iter()
                .filter(|t| t.name != "tool_search")
                .filter_map(|t| {
                    let description = t.description.to_ascii_lowercase();
                    let score: usize = terms
                        .iter()
                        .map(|&term| {
                            if t.name == term {
                                100
                            } else if t.name.contains(term) {
                                3
                            } else if description.contains(term) {
                                1
                            } else {
                                0
                            }
                        })
                        .sum();
                    (score > 0).then_some((score, t))
                })
                .collect();
            matches.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            matches.truncate(10);
            let g = c
                .grant
                .as_ref()
                .ok_or_else(|| bad("Provider session required"))?;
            if !matches.is_empty() {
                g.loaded
                    .lock()
                    .unwrap()
                    .extend(matches.iter().map(|(_, t)| t.name));
                g.changes.send_modify(|n| *n += 1);
            }
            Ok(
                json!({"tools":matches.iter().map(|(_,t)|json!({"name":t.name,"description":t.description})).collect::<Vec<_>>()}),
            )
        }
        "orchestrator_capabilities" => {
            if !input.as_object().is_some_and(Map::is_empty) {
                return Err(bad("This tool takes no arguments"));
            }
            Ok(
                json!({"server":"Kar","tools":tools(c).iter().filter(|t|t.name!="tool_search").map(|t|t.name).collect::<Vec<_>>(),
                "provider_session":c.grant.as_ref().map(|g|json!({"id":g.id,"project_id":g.project,"thread_id":g.chat,"runtime":g.runtime})),
                "write_access":!c.plan && (c.grant.is_none() || c.policy.as_ref().is_some_and(|p| p.writes_issues() || p.writes_pulls() || p.thread_tools.as_ref().is_some_and(|s|s.write) && p.executes() || p.writes_workflows() || p.worktree_directory().is_some())),"issue_write_access":!c.plan && c.policy.as_ref().is_some_and(Policy::writes_issues), "issue_read_access":c.policy.as_ref().is_some_and(Policy::reads_issues), "workflow_write_access":!c.plan && c.policy.as_ref().is_some_and(Policy::writes_workflows)}),
            )
        }
        "kar_project_list" => {
            let a: Page = args(input)?;
            let rows = app
                .projects()
                .into_iter()
                .filter(|p| !p.metadata.deleted && super::project_tools::visible(c, p))
                .map(|p| json!(p))
                .collect();
            page(rows, "projects", a.cursor, a.limit)
        }
        "kar_project_read" => {
            let a: ProjectArgs = args(input)?;
            let p = app.project(a.project_id)?;
            if p.metadata.deleted || !super::project_tools::visible(c, &p) {
                return Err(super::super::missing());
            }
            Ok(json!(p))
        }
        "kar_thread_list" => {
            let a: Threads = args(input)?;
            let project = super::operations::list_project(app,c,a.project_id)?;
            if let Some(p) = project {
                app.project(p)?;
            }
            let shell = super::super::shell::snapshot(app)?.2;
            let title = a.title_contains.map(|s| s.to_lowercase());
            let rows = shell["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| {
                    let row = &item["row"];
                    item["kind"] == "chat"
                        && row["unavailable"] != true
                        && project.is_none_or(|p| row["project"] == json!(p))
                        && c.grant.as_ref().is_none_or(|_| serde_json::from_value::<Uuid>(item["id"].clone()).is_ok_and(|t| serde_json::from_value::<Uuid>(row["project"].clone()).is_ok_and(|p|super::operations::visible(&app.0.catalog.lock().unwrap(),c,p,t))))
                        && (a.include_archived || row["archived"] == false)
                        && a.snoozed.is_none_or(|s| row["snoozed"] == s)
                        && a.settled
                            .is_none_or(|s| (row["metadata"]["settled_override"] == "settled") == s)
                        && title.as_ref().is_none_or(|t| {
                            row["title"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(t)
                        })
                })
                .map(|i| json!({"thread_id":i["id"],"summary":i["row"]}))
                .collect();
            page(rows, "threads", a.cursor, a.limit)
        }
        "kar_workflow" => super::workflow::call(app, c, input),
        "kar_thread_read" => read(app, c, args(input)?),
        "kar_queue" => queue(app, c, args(input)?),
        _ => Err(bad("Unknown workbench tool")),
    }
}
fn read(app: &Workbench, c: &Caller, a: Read) -> Result<Value> {
    let (p, t) = super::operations::target(app,c,a.project_id,a.thread_id,false)?;
    let view = a.view.as_deref().unwrap_or("messages");
    if !matches!(view, "messages" | "activity") || a.after_position.is_some_and(|n| n < -1) {
        return Err(bad("Invalid view or position"));
    }
    let max = a.max_chars_per_item.unwrap_or(2000).min(20000);
    let limit = a.limit.unwrap_or(20).min(100);
    if max == 0 || limit == 0 {
        return Err(bad("Text and page limits must be positive"));
    }
    let observing=if let Some(grant)=&c.grant && grant.chat!=t {
        app.0.chats.history(grant.chat)?.turns.last().filter(|run|run.status==crate::chat::TurnStatus::Running).map(|run|run.id)
    }else{None};
    let history = app.0.chats.history(t)?;
    let mut out = vec![];
    let mut has_more = false;
    for (n, row) in history
        .turns
        .iter()
        .flat_map(super::super::thread_find::items)
        .enumerate()
    {
        if a.item_id.as_ref().is_some_and(|id| id != &row.entry_id)
            || a.after_position.is_some_and(|pos| n as i64 <= pos)
            || a.before_position.is_some_and(|pos| n >= pos)
        {
            continue;
        }
        let (kind, text) = match &row.item.item {
            Item::Message { role, text, .. } => {
                (json!(role), std::borrow::Cow::Borrowed(text.as_str()))
            }
            _ if view == "messages" && a.item_id.is_none() => continue,
            other => (
                json!("activity"),
                std::borrow::Cow::Owned(serde_json::to_string(other)?),
            ),
        };
        if out.len() == limit {
            has_more = true;
            break;
        }
        let (text, next) = window(&text, a.text_offset, max);
        out.push(json!({"item_id":row.entry_id,"run_id":row.turn,"position":n,"kind":kind,"text":text,"next_text_offset":next}));
    }
    super::orchestrator::observe_read(app,c,t,observing,&out)?;
    Ok(json!({"project_id":p,"thread_id":t,"items":out,"has_more":has_more}))
}
fn window(text: &str, offset: usize, max: usize) -> (String, Option<usize>) {
    let mut end = 0usize;
    let mut projected = String::new();
    for ch in text.chars() {
        let next = end + ch.len_utf16();
        if next > offset {
            if next > offset.saturating_add(max) && !projected.is_empty() {
                return (projected, Some(end));
            }
            projected.push(ch);
        }
        end = next;
    }
    (projected, None)
}
fn queue(app: &Workbench, c: &Caller, q: Queue) -> Result<Value> {
    let write=!matches!(q.action.as_str(),"list"|"read");
    let (_,t)=super::operations::target(app,c,q.project_id,q.thread_id,write)?;
    if matches!(q.action.as_str(), "list" | "read") {
        let (held, entries) = app.0.chats.queue_view(t)?;
        let waiting = entries.into_iter().filter(|e| e.outstanding());
        if q.action == "read" {
            let entry = waiting
                .into_iter()
                .find(|e| Some(e.id) == q.input_id)
                .ok_or_else(|| bad("Queued input not found"))?;
            let (text, more) = window(&entry.text, 0, 16000);
            return Ok(
                json!({"id":entry.id,"status":entry.status,"text":text,"truncated":more.is_some(),"attachments":entry.attachments}),
            );
        }
        let rows = waiting
            .map(|e| json!({"id":e.id,"status":e.status,"preview":window(&e.text,0,200).0}))
            .collect();
        let mut result = page(rows, "inputs", q.cursor, q.limit)?;
        result["held"] = held.into();
        return Ok(result);
    }
    if c.plan {
        return Err(bad("Queue writes require an execute grant"));
    }
    if app.archived(t) {
        return Err(bad("Chat is archived"));
    }
    let id = q.input_id.ok_or_else(|| bad("input_id is required"))?;
    let result = match q.action.as_str() {
        "cancel" => json!(app.0.chats.withdraw(t, id)?),
        "edit" | "reorder" => {
            let cmd = q.command_id.ok_or_else(|| bad("command_id is required"))?;
            let act = if q.action == "edit" {
                QueueAction::Edit {
                    input: id,
                    text: q.text.ok_or_else(|| bad("text is required"))?,
                }
            } else {
                QueueAction::Reorder {
                    input: id,
                    before: q.before,
                }
            };
            json!(app.0.chats.queue_control(t, cmd, act)?)
        }
        _ => return Err(bad("Unsupported queue action")),
    };
    app.wake_queue();
    Ok(result)
}
