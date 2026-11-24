//! Scheduled model runs retain their initiating chat, execution limits and trigger receipt.
use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(in crate::workbench) struct Binding {
    pub owner: Uuid,
    pub workspace: PathBuf,
    pub settings: crate::SessionSettings,
    pub approvals: crate::ApprovalMode,
    pub bind: bool,
    pub fresh: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Command {
    actor: Uuid,
    project: Uuid,
    args: Value,
    result: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::workbench) struct Save {
    pub scheduled_task_id: Option<Uuid>,
    pub prompt: Option<String>,
    pub title: Option<String>,
    pub schedule: Option<Schedule>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub enabled: Option<bool>,
    pub fresh_session: Option<bool>,
    pub bind_to_current_thread: Option<bool>,
    pub webhook_secret_ref: Option<Uuid>,
}
impl Workbench {
    pub(in crate::workbench) fn model_schedules(
        &self,
        project: Uuid,
        actor: Uuid,
    ) -> Result<Value> {
        let db = self.0.workflows.db.lock().unwrap();
        let rows=db.workflows.values().filter(|w|w.project==project&&db.model_schedules.get(&w.id).is_some_and(|m|m.owner==actor)).map(|w|json!({"scheduled_task_id":w.id,"project_id":project,"title":w.definition.name,"enabled":w.definition.enabled,"schedule":w.definition.schedule,"next_ms":w.next_ms,"trigger_path":matches!(w.definition.schedule,Schedule::Webhook).then(||format!("/hooks/workflows/{project}/{}",w.id))})).collect::<Vec<_>>();
        Ok(json!({"tasks":rows}))
    }
    pub(in crate::workbench) fn model_schedule_command(
        &self,
        project: Uuid,
        actor: Uuid,
        id: Uuid,
        name: &str,
        args: Value,
        defaults: Option<(Binding, String, Option<String>)>,
        authorize: impl Fn() -> Result<()>,
    ) -> Result<Value> {
        let cfg = json!({"name":name,"arguments":args});
        let engine = &self.0.workflows;
        authorize()?;
        let prior = engine
            .db
            .lock()
            .unwrap()
            .model_schedule_commands
            .get(&id)
            .cloned();
        if let Some(r) = prior {
            return if r.actor == actor && r.project == project && r.args == cfg {
                Ok(r.result)
            } else {
                Err(Error::Refused("scheduled command already used".into()))
            };
        }
        let reference: Option<Uuid> =
            serde_json::from_value(args.get("webhook_secret_ref").cloned().unwrap_or_default())?;
        let commit = |secret: Option<String>| {
            engine.update(|d|{
            authorize()?;
            if let Some(old)=d.model_schedule_commands.get(&id){
                if old.project!=project||old.actor!=actor||old.args!=cfg{return Err(Error::Refused("scheduled command already used".into()));}
                return Ok(old.result.clone());
            }
            let task:Option<Uuid>=serde_json::from_value(args.get("scheduled_task_id").cloned().unwrap_or(Value::Null))?;
            if let Some(t)=task && !d.model_schedules.get(&t).is_some_and(|b|b.owner==actor&&d.workflows.get(&t).is_some_and(|w|w.project==project)){return Err(missing());}
            let result=match name{
                "kar_schedule_task"=>{
                    let req:Save=serde_json::from_value(args.clone())?;
                    let (binding,rt,model)=defaults.ok_or_else(missing)?;
                    let target=req.scheduled_task_id.unwrap_or(id);
                    if task.is_none()&&(d.workflows.contains_key(&target)||d.model_schedules.contains_key(&target)){return Err(Error::Refused("scheduled task ID is occupied".into()));}
                    let old=d.workflows.get(&target);
                    let old_step=old.and_then(|w|w.definition.steps.first()).and_then(|s|if let Step::Agent{runtime,model,prompt}=s{Some((runtime.clone(),model.clone(),prompt.clone()))}else{None});
                    if old.is_some()&&old_step.is_none(){return Err(Error::Refused("scheduled workflow has been reconfigured by its owner".into()));}
                    let (runtime,selected_model)=if req.runtime.is_none()&&req.model.is_none(){old_step.as_ref().map_or((rt,model),|s|(s.0.clone(),s.1.clone()))}else{(rt,model)};
                    let prompt=req.prompt.or_else(||old_step.map(|s|s.2)).ok_or_else(||Error::Refused("new task needs a prompt".into()))?;
                    let title=req.title.or_else(||old.map(|w|w.definition.name.clone())).unwrap_or_else(||prompt.chars().take(80).collect());
                    let schedule=req.schedule.or_else(||old.map(|w|w.definition.schedule.clone())).ok_or_else(||Error::Refused("new task needs a schedule".into()))?;
                    let enabled=req.enabled.or_else(||old.map(|w|w.definition.enabled)).unwrap_or(true);
                    let mut binding=d.model_schedules.get(&target).cloned().unwrap_or(binding);
                    if let Some(f)=req.bind_to_current_thread{binding.bind=f;}
                    if let Some(f)=req.fresh_session{binding.fresh=f;}
                    if binding.bind && (req.runtime.is_some()||req.model.is_some()||req.fresh_session==Some(true)){
                        return Err(Error::Refused("bound schedules use their existing chat's model and context".into()));
                    }
                    let definition=Definition{name:title,enabled,schedule,steps:vec![Step::Agent{runtime,model:selected_model,prompt}]};
                    definition.validate()?;definition.schedule.next_after(engine.clock.now_ms())?;
                    if req.webhook_secret_ref.is_some() && !matches!(definition.schedule,Schedule::Webhook){return Err(Error::Refused("webhook signing secret requires a webhook schedule".into()));}
                    let w=install_definition(d,project,target,definition,secret,engine.clock.now_ms())?;
                    d.model_schedules.insert(target,binding);
                    let mut result=json!({"scheduled_task_id":target,"project_id":project,"next_ms":w.next_ms,"enabled":w.definition.enabled});
                    if matches!(w.definition.schedule,Schedule::Webhook){
                        if task.is_none() && req.webhook_secret_ref.is_none() {
                            let token=format!("{}{}",Uuid::new_v4().simple(),Uuid::new_v4().simple());
                            result["trigger_path"]=format!("/hooks/workflows/{project}/{target}/{token}").into();
                            d.webhook_tokens.insert(target,token);
                        }else{result["trigger_path"]=format!("/hooks/workflows/{project}/{target}").into();}
                    }
                    result
                }
                "kar_delete_scheduled_task"=>{
                    let target=task.ok_or_else(missing)?;
                    for r in d.runs.values_mut().filter(|r|r.workflow==target&&!matches!(r.status,Status::Completed|Status::Cancelled|Status::Failed)){
                        r.cancel_requested=true;
                        if r.status!=Status::Running{r.status=Status::Cancelled;}
                    }
                    views::erase_workflow(d,target);d.model_schedules.remove(&target);
                    json!({"scheduled_task_id":target,"deleted":true})
                }
                "kar_run_scheduled_task_now"=>{
                    let w=d.workflows.get(&task.ok_or_else(missing)?).cloned().ok_or_else(missing)?;
                    let run=insert_run(d,&w,Trigger::Manual(id),Value::Null,engine.clock.now_ms())?;
                    json!({"scheduled_task_id":w.id,"run_id":run.id,"status":run.status})
                }
                _=>return Err(missing()),
            };
            d.model_schedule_commands.insert(id,Command{actor,project,args:cfg,result:result.clone()});
            Ok(result)
        })
        };
        match reference {
            Some(reference) => self.with_model_secret(project, actor, reference, |secret| {
                commit(Some(secret.to_owned()))
            }),
            None => commit(None),
        }
    }
}
