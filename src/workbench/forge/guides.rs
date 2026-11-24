//! Generate a PR review tour against immutable Git objects and retain the head it describes.
mod checkout;
mod generate;
use super::*;
use crate::workbench::{Catalog, mcp::Policy};
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(in crate::workbench) struct Settings {
    pub model: Option<String>,
    pub instructions: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Run {
    id: Uuid,
    project: Uuid,
    chat: Uuid,
    repo: String,
    number: u64,
    head: String,
    base: String,
    title: String,
    description: String,
    settings: Settings,
    started_ms: u64,
    error: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Saved {
    guide: Option<Value>,
    run: Option<Run>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Command {
    project: Uuid,
    chat: Uuid,
    repo: String,
    numbers: Vec<u64>,
    results: BTreeMap<u64, Value>,
}
fn now() -> u64 {
    jiff::Timestamp::now().as_millisecond().max(0) as u64
}
fn key(p: Uuid, repo: &str, n: u64) -> String {
    format!("{p}/{repo}/{n}")
}
fn permitted(db: &Catalog, r: &Run) -> bool {
    Policy::current(db, r.project, r.chat).is_some_and(|p| {
        p.writes_pulls()
            && p.pull_repository.as_deref() == Some(&r.repo)
            && p.review_guides.as_ref() == Some(&r.settings)
    })
}
pub(in crate::workbench) fn recover(app: &Workbench) -> crate::Result<()> {
    if app
        .0
        .catalog
        .lock()
        .unwrap()
        .forge
        .guides
        .values()
        .any(|g| g.run.as_ref().is_some_and(|r| r.error.is_none()))
    {
        app.update(|db| {
            for g in db.forge.guides.values_mut() {
                if let Some(r) = &mut g.run
                    && r.error.is_none()
                {
                    r.error = Some(
                        "host stopped before the guide was published; start a new run to retry"
                            .into(),
                    );
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}
pub(in crate::workbench) async fn read(
    app: Workbench,
    p: Uuid,
    n: u64,
    auth: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    if n == 0 {
        return Err(ForgeError::bad("invalid pull number"));
    }
    let c = Context::open(app.clone(), p).await?;
    let repo = c.identity();
    let key = key(p, &repo, n);
    auth(&app.0.catalog.lock().unwrap(), &repo)?;
    let detail = c.execute(&Operation::Pull { number: n }).await?;
    let head = &detail["head_sha"];
    let db = app.0.catalog.lock().unwrap();
    auth(&db, &repo)?;
    let saved = db.forge.guides.get(&key);
    let guide = saved.and_then(|s| s.guide.as_ref());
    let run=saved.and_then(|s|s.run.as_ref()).map(|r|json!({"id":r.id,"head_sha":r.head,"started_ms":r.started_ms,"status":if r.error.is_some(){"failed"}else{"running"},"detail":r.error}));
    Ok(
        json!({"number":n,"repository":repo,"guide":guide,"run":run,"current_head_sha":head,"stale":guide.is_some_and(|g|head.is_string()&&g["head_sha"]!=*head)}),
    )
}
pub(in crate::workbench) async fn write(
    app: Workbench,
    p: Uuid,
    chat: Uuid,
    cmd: Uuid,
    numbers: Vec<u64>,
    settings: Settings,
    auth: impl Fn(&Catalog, &str) -> crate::Result<()>,
) -> Result<Value, ForgeError> {
    if numbers.is_empty()
        || numbers.len() > 20
        || numbers.iter().any(|n| *n == 0)
        || numbers.iter().collect::<BTreeSet<_>>().len() != numbers.len()
    {
        return Err(ForgeError::bad(
            "review guides need one to twenty distinct pull numbers",
        ));
    }
    let c = Context::open(app.clone(), p).await?;
    let repo = c.identity();
    let gate = app.0.mcp.command_gate(cmd);
    let _guard = gate.lock().await;
    app.update(|db| {
        auth(db, &repo)?;
        if let Some(old) = db.forge.guide_commands.get(&cmd) {
            if old.project != p || old.chat != chat || old.repo != repo || old.numbers != numbers {
                return Err(Error::Refused("guide command ID already used".into()));
            }
        } else {
            db.forge.guide_commands.insert(
                cmd,
                Command {
                    project: p,
                    chat,
                    repo: repo.clone(),
                    numbers: numbers.clone(),
                    results: BTreeMap::new(),
                },
            );
        }
        Ok(())
    })?;
    let mut rows = vec![];
    for n in numbers {
        let prior = app
            .0
            .catalog
            .lock()
            .unwrap()
            .forge
            .guide_commands
            .get(&cmd)
            .and_then(|c| c.results.get(&n))
            .cloned();
        if let Some(r) = prior {
            rows.push(r);
            continue;
        }
        let observed = c.execute(&Operation::Pull { number: n }).await;
        let key = key(p, &repo, n);
        let (r,run)=app.update(|db|{
            auth(db,&repo)?;
            let result=match observed {
                Ok(detail)=> {
                    if let(Some(head),Some(base))=(detail["head_sha"].as_str(),detail["base_sha"].as_str()) {
                        let saved=db.forge.guides.entry(key.clone()).or_default();
                        if let Some(active)=saved.run.as_ref().filter(|r|r.head==head&&r.error.is_none()){
                            (json!({"number":n,"outcome":"running","head_sha":head,"run_id":active.id}),None)
                        }else{
                            let run=Run{id:Uuid::new_v4(),project:p,chat,repo:repo.clone(),number:n,head:head.into(),base:base.into(),title:detail["title"].as_str().unwrap_or("").into(),description:detail["body"].as_str().unwrap_or("").chars().take(12000).collect(),settings:settings.clone(),started_ms:now(),error:None};
                            saved.run=Some(run.clone());
                            (json!({"number":n,"outcome":"started","head_sha":head,"run_id":run.id}),Some(run))
                        }
                    }else{(json!({"number":n,"outcome":"failed","detail":"pull lacks an immutable head or base"}),None)}
                }
                Err(e)=>(json!({"number":n,"outcome":"failed","detail":e.detail}),None)
            };
            db.forge.guide_commands.get_mut(&cmd).unwrap().results.insert(n,result.0.clone());
            Ok(result)
        })?;
        if let Some(run) = run {
            launch(app.clone(), key, run);
        }
        rows.push(r);
    }
    Ok(json!({"guides":rows}))
}
fn launch(app: Workbench, key: String, r: Run) {
    tokio::spawn(async move {
        let result = async {
            let c = Context::open(app.clone(), r.project).await?;
            if c.identity() != r.repo || !permitted(&app.0.catalog.lock().unwrap(), &r) {
                return Err(ForgeError::bad(
                    "review guide authorization or repository changed",
                ));
            }
            generate::run(&c, &r).await
        }
        .await;
        let _ = app.update(|db| {
            let allowed = permitted(db, &r);
            let Some(saved) = db
                .forge
                .guides
                .get_mut(&key)
                .filter(|s| s.run.as_ref().is_some_and(|a| a.id == r.id))
            else {
                return Ok(());
            };
            match result {
                Ok(mut guide) if allowed => {
                    guide["head_sha"] = r.head.into();
                    guide["generated_at"] = jiff::Timestamp::now().to_string().into();
                    guide["model_selection"] = json!({"runtime":"codex","model":r.settings.model});
                    saved.guide = Some(guide);
                    saved.run = None;
                }
                Ok(_) => {
                    saved.run.as_mut().unwrap().error =
                        Some("review guide permission was revoked".into())
                }
                Err(e) => saved.run.as_mut().unwrap().error = Some(e.detail),
            }
            Ok(())
        });
    });
}
