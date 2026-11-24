//! Model-side forge operations use the current chat and the host-pinned repository identity.
use super::*;
use crate::{
    Error, Result,
    workbench::{
        Catalog,
        forge::{
            self,
            model::{Change, Kind},
        },
    },
};
use serde::Deserialize;
use toolkit::Tool;

const ISSUE_LINKS: &str = "kar_issue_links";
const PULL_LINKS: &str = "kar_pull_request_links";
const PULL: &str = "kar_pull_request";
const CI: &str = "kar_ci";
pub(super) fn handles(name: &str) -> bool {
    matches!(name, ISSUE_LINKS | PULL_LINKS | PULL | CI)
}

pub(super) fn tools(c: &Caller) -> Vec<Tool> {
    let mut tools = vec![];
    let Some(p) = c.policy.as_ref() else {
        return tools;
    };
    let id = json!({"type":"string","format":"uuid"});
    for (name, reads, writes) in [
        (ISSUE_LINKS, p.reads_issues(), p.writes_issues()),
        (PULL_LINKS, p.reads_pulls(), p.writes_pulls()),
    ] {
        if !reads {
            continue;
        }
        let acts = if writes && !c.plan {
            vec!["list", "link", "unlink"]
        } else {
            vec!["list"]
        };
        tools.push(Tool{name,description:"List this thread's linked objects. Link or unlink one number or an atomic batch in the granted repository using a stable command_id.",properties:json!({"project_id":id,"thread_id":id,"action":{"enum":acts},"number":{"type":"integer","minimum":1},"numbers":{"type":"array","items":{"type":"integer","minimum":1}},"command_id":id}),required:&["action"]});
    }
    if p.reads_pulls() {
        let mut acts = vec!["list", "list_page", "read", "threads", "thread_comments","watch_status","read_guide"];
        if p.writes_pulls() && !c.plan {
            acts.extend([
                "create", "review", "reply", "resolve", "update", "merge", "comment","watch","unwatch",
            ]);
        }
        if !c.plan && p.writes_pulls() && p.review_guides.is_some(){acts.push("write_guides");}
        let string = json!({"type":"string"});
        tools.push(Tool{name:PULL,description:"Use this chat's granted pull-request repository. Read and review changes, publish this chat’s current branch and open or reuse its pull, reply to a review thread or resolve it. Reuse command_id for mutations.",properties:json!({"project_id":id,"thread_id":id,"action":{"enum":acts},"command_id":id,"number":{"type":"integer","minimum":1},"until":{"enum":["checks","merged"]},"numbers":{"type":"array","maxItems":20,"items":{"type":"integer","minimum":1}},"cursor":string,"review_thread_id":string,"head":string,"head_repository":string,"draft":{"type":"boolean"},"private":{"type":"boolean"},"base":string,"title":string,"body":string,"verdict":{"enum":["comment","approve","request-changes"]},"comments":{"type":"array","items":{"type":"object"}},"resolved":{"type":"boolean"},"ready":{"type":"boolean"},"auto":{"type":"boolean"},"expected_head_sha":string,"method":{"enum":["merge","squash","rebase"]},"query":string,"involvement":{"enum":["all","authored","reviewing"]},"limit":{"type":"integer","minimum":1,"maximum":100},"include":{"type":"array","items":{"enum":["diff","threads","comments"]}},"include_resolved":{"type":"boolean"},"state":{"enum":["all","open","closed","merged"]},"add_labels":{"type":"array","items":string},"remove_labels":{"type":"array","items":string},"request_reviewers":{"type":"array","items":string}}),required:&["action"]});
    }
    if p.reads_pulls() {
        let acts = if !c.plan && p.writes_pulls() {
            vec!["checks", "logs", "rerun"]
        } else {
            vec!["checks", "logs"]
        };
        tools.push(Tool{name:CI,description:"Inspect checks for the current PR head, tail failing or named logs, or request a rerun. Reuse command_id for a rerun.",properties:json!({"project_id":id,"thread_id":id,"action":{"enum":acts},"number":{"type":"integer","minimum":1},"command_id":id,"check":{"type":"string"},"tail_lines":{"type":"integer","minimum":1,"maximum":2000}}),required:&["action","number"]});
    }
    tools
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Links {
    action: String,
    command_id: Option<Uuid>,
    number: Option<u64>,
    numbers: Option<Vec<u64>>,
}
fn denied() -> Error {
    Error::Refused("model forge permission or repository scope no longer matches".into())
}
pub(super) async fn call(app: Workbench, c: Caller, name: &str, mut input: Value) -> Result<Value> {
    let obj = input.as_object_mut().ok_or_else(denied)?;
    let project = serde_json::from_value(obj.remove("project_id").unwrap_or_default())?;
    let thread = serde_json::from_value(obj.remove("thread_id").unwrap_or_default())?;
    let (project, chat) = toolkit::target(&app, &c, project, thread)?;
    let kind = if name == ISSUE_LINKS {
        Kind::Issue
    } else {
        Kind::Pull
    };
    let action = obj
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(denied)?
        .to_owned();
    let write = !matches!(
        action.as_str(),
        "list" | "list_page" | "read" | "threads" | "thread_comments" | "checks" | "logs" | "watch_status" | "read_guide"
    );
    let p = c.policy.ok_or_else(denied)?;
    let grant = c.grant.ok_or_else(denied)?;
    let allowed = match (kind, write) {
        (Kind::Issue, true) => p.writes_issues(),
        (Kind::Issue, false) => p.reads_issues(),
        (Kind::Pull, true) => p.writes_pulls(),
        (Kind::Pull, false) => p.reads_pulls(),
    };
    if !allowed || write && c.plan {
        return Err(denied());
    }
    let guide_settings=p.review_guides.clone();
    let scope = (kind == Kind::Issue)
        .then(|| p.issue_scope.clone())
        .flatten();
    let authorize = move |db: &Catalog, repo: &str| {
        let pinned = if kind == Kind::Issue {
            p.repository.as_deref()
        } else {
            p.pull_repository.as_deref()
        };
        if !grant.active() || Policy::current(db, project, chat) != Some(&p) || pinned != Some(repo)
        {
            return Err(denied());
        }
        Ok(())
    };
    let name = name.to_owned();
    let result = tokio::spawn(async move {
        if name == PULL || name == CI {
            let fields = input.as_object_mut().unwrap();
            fields.remove("action");
            let id = fields
                .remove("command_id")
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| forge::ForgeError::bad("invalid command ID"))?;
            if name==PULL && (action=="list_page" || action=="list" && !fields.is_empty() || action=="read" && fields.keys().any(|k|k!="number")){
                if id.is_some(){return Err(forge::ForgeError::bad("pull reads take no command ID"));}
                return forge::model_reads::call(app,project,chat,action=="read",input,authorize).await;
            }
            if name==PULL && action=="create" && !fields.contains_key("head"){
                let req=serde_json::from_value(input).map_err(|e|forge::ForgeError::bad(e.to_string()))?;
                return forge::model_publish::run(app,project,chat,id.ok_or_else(||forge::ForgeError::bad("publication requires command ID"))?,req,authorize).await;
            }
            if name==PULL && action=="read_guide"{
                #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Read{number:u64}
                let q:Read=serde_json::from_value(input).map_err(|e|forge::ForgeError::bad(e.to_string()))?;
                if id.is_some(){return Err(forge::ForgeError::bad("reading a guide takes no command ID"));}
                return forge::guides::read(app,project,q.number,authorize).await;
            }
            if name==PULL && action=="write_guides"{
                #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Batch{numbers:Vec<u64>}
                let q:Batch=serde_json::from_value(input).map_err(|e|forge::ForgeError::bad(e.to_string()))?;
                return forge::guides::write(app,project,chat,id.ok_or_else(||forge::ForgeError::bad("guide generation requires command ID"))?,q.numbers,guide_settings.ok_or_else(||forge::ForgeError::bad("review guides are disabled"))?,authorize).await;
            }
            if name==PULL && matches!(action.as_str(),"watch"|"unwatch"|"watch_status"){
                #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct WatchRequest{number:u64,until:Option<forge::watches::Until>}
                let q:WatchRequest=serde_json::from_value(input).map_err(|e|forge::ForgeError::bad(e.to_string()))?;
                if action!="watch"&&q.until.is_some() || action=="watch_status"&&id.is_some(){return Err(forge::ForgeError::bad("invalid watch arguments"));}
                return forge::watches::command(app,project,chat,q.number,id,&action,q.until,authorize).await;
            }
            if name == CI || matches!(action.as_str(), "comment" | "merge" | "update") {
                if name == CI && !matches!(action.as_str(), "checks" | "logs" | "rerun") {
                    return Err(forge::ForgeError::bad("unknown CI action"));
                }
                let number = fields
                    .remove("number")
                    .and_then(|v| v.as_u64())
                    .filter(|n| *n > 0)
                    .ok_or_else(|| forge::ForgeError::bad("positive pull number required"))?;
                fields.insert("action".into(), action.into());
                let request = serde_json::from_value(input)
                    .map_err(|e| forge::ForgeError::bad(e.to_string()))?;
                return forge::model::pull(
                    app,
                    project,
                    chat,
                    id,
                    forge::Operation::PullCommand {
                        number,
                        command: request,
                    },
                    authorize,
                )
                .await;
            }
            let op = match action.as_str() {
                "list" => "pulls",
                "read" => "pull",
                "threads" => "review_threads",
                "thread_comments" => "thread_comments",
                "create" => "create_pull",
                "review" => "submit_review",
                "reply" => "reply_to_thread",
                "resolve" => "set_thread_resolution",
                _ => return Err(forge::ForgeError::bad("unknown pull action")),
            };
            if let Some(thread) = fields.remove("review_thread_id") {
                fields.insert("thread_id".into(), thread);
            }
            if fields.insert("op".into(), op.into()).is_some() {
                return Err(forge::ForgeError::bad("unexpected op"));
            }
            let op =
                serde_json::from_value(input).map_err(|e| forge::ForgeError::bad(e.to_string()))?;
            forge::model::pull(app, project, chat, id, op, authorize).await
        } else {
            let args: Links =
                serde_json::from_value(input).map_err(|e| forge::ForgeError::bad(e.to_string()))?;
            let change = match (
                args.action.as_str(),
                args.command_id,
                args.number,
                args.numbers,
            ) {
                ("list", None, None, None) => None,
                (a @ ("link" | "unlink"), Some(cmd), n, ns) if n.is_some() != ns.is_some() => {
                    Some((
                        if a == "link" {
                            Change::Link
                        } else {
                            Change::Unlink
                        },
                        cmd,
                        ns.unwrap_or_else(|| vec![n.unwrap()]),
                    ))
                }
                _ => {
                    return Err(forge::ForgeError::bad(
                        "link or unlink requires a command ID and either number or numbers",
                    ));
                }
            };
            forge::model::links(
                app,
                project,
                chat,
                kind,
                scope.as_deref(),
                change,
                authorize,
            )
            .await
        }
    })
    .await
    .map_err(|e| Error::Closed(e.to_string()))?;
    result.map_err(|e| Error::Refused(format!("{}: {}", e.kind, e.detail)))
}
