//! Owner-facing account access through the same native readers used by Kar's CLI.
use super::*;
use crate::account::{AccountUsageOptions, AuthStatusOptions, LoginOptions, LoginResult};
use crate::workbench::chatgpt;

pub(super) fn context(
    app: &Workbench,
    p: Uuid,
    id: &str,
) -> Result<(
    Runtime,
    crate::InstallationOptions,
    crate::account::binding::Key,
)> {
    let project = app.project(p)?;
    app.workspace(&project)?;
    let mut opts = SessionOptions::new(project.root);
    opts.remove_env("KAR_WORKBENCH_TOKEN")?;
    if dynamic(id) {
        return Err(Error::Unsupported("this agent provides ACP authentication under acp-registry; ACP has no standard account quota method".into()));
    }
    let instance = if chatgpt::managed(&app.0.chats, id)? {
        Some(crate::provider_instance::for_launch("codex", id)?)
    } else {
        crate::provider_instance::configured()?
            .into_iter()
            .find(|instance| instance.id == id)
    };
    let driver = instance
        .as_ref()
        .map_or(id, |instance| instance.runtime.runtime());
    if let Some(instance) = &instance {
        instance.configure(&mut opts);
    }
    let mut runtime =
        crate::workbench::managed_providers::resolve(&app.0.chats, crate::runtime(driver)?)?;
    if let Some(program) = instance.and_then(|instance| instance.program) {
        runtime.program = program;
    }
    let binding = crate::account::binding::Key::new(driver, &opts);
    Ok((runtime, opts.installation_options(), binding))
}
async fn status(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    let (rt, installation, binding) = context(&app, p, &id)?;
    let _guard = app.0.chats.accounts.operation(binding)?;
    if chatgpt::managed(&app.0.chats, &id)? {
        return Ok(Json(chatgpt::status(&app.0.chats, &id)?));
    }
    Ok(Json(
        serde_json::to_value(
            rt.auth_status(&AuthStatusOptions {
                installation,
                ..Default::default()
            })
            .await?,
        )
        .map_err(Error::from)?,
    ))
}
async fn quota(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    let (rt, installation, binding) = context(&app, p, &id)?;
    let _guard = app.0.chats.accounts.operation(binding)?;
    chatgpt::native_quota(&app.0.chats, &id)?;
    Ok(Json(
        serde_json::to_value(
            rt.account_usage(&AccountUsageOptions {
                installation,
                ..Default::default()
            })
            .await?,
        )
        .map_err(Error::from)?,
    ))
}
async fn methods(State(app): State<Workbench>, Param((p, id)): Param<(Uuid, String)>) -> Reply {
    let (rt, _, _) = context(&app, p, &id)?;
    if chatgpt::managed(&app.0.chats, &id)? {
        return Ok(Json(
            json!({"runtime":id,"login":true,"logout":true,"quota":false,"login_method":"chatgpt_handoff","prepare":"chatgpt/prepare"}),
        ));
    }
    Ok(Json(
        json!({"runtime":id,"login":rt.supports_login(),"logout":rt.supports_logout(),"quota":rt.supports_account_usage()}),
    ))
}
async fn login(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
) -> Reply {
    let (rt, installation, binding) = context(&app, p, &id)?;
    if chatgpt::managed(&app.0.chats, &id)? {
        return Err(bad("use chatgpt/prepare to reconnect this managed profile").into());
    }
    if !rt.supports_login() {
        return Err(Error::Unsupported(format!("hosted account login for {id}")).into());
    }
    let owner =
        flow::client(&headers)?.ok_or_else(|| bad("Kar-Auth-Client is required for sign-in"))?;
    let accounts = app.0.chats.accounts.clone();
    let program = rt.program.clone();
    let store = app.0.chats.clone();
    let instance = id.clone();
    app.0
        .acp_auth
        .start(p, id, &program, Some(owner), move |io| async move {
            let _change = accounts.change(binding).await?;
            if chatgpt::managed(&store, &instance)? {
                return Err(bad("use chatgpt/prepare to reconnect this managed profile"));
            }
            let options = LoginOptions {
                installation,
                open_browser: Some(false),
                ..Default::default()
            };
            let result = rt
                .login(&options, &mut |event| {
                    io.set(json!({"status":"interaction","event":event}));
                    Ok(())
                })
                .await?;
            let status = match &result {
                LoginResult::LoggedIn { .. } => "complete",
                LoginResult::Cancelled => "cancelled",
                LoginResult::Failed { .. } => "failed",
                LoginResult::Unsupported { .. } => "unsupported",
            };
            Ok(json!({"status":status,"result":result}))
        })
}
async fn logout(
    State(app): State<Workbench>,
    Param((p, id)): Param<(Uuid, String)>,
    headers: HeaderMap,
) -> Reply {
    let (rt, installation, binding) = context(&app, p, &id)?;
    if !rt.supports_logout() {
        return Err(Error::Unsupported(format!("hosted account logout for {id}")).into());
    }
    let owner =
        flow::client(&headers)?.ok_or_else(|| bad("Kar-Auth-Client is required for sign-out"))?;
    let accounts = app.0.chats.accounts.clone();
    let program = rt.program.clone();
    let store = app.0.chats.clone();
    let instance = id.clone();
    app.0
        .acp_auth
        .start(p, id, &program, Some(owner), move |_| async move {
            let _change = accounts.change(binding).await?;
            if chatgpt::managed(&store, &instance)? {
                return Ok(
                    json!({"status":"complete","result":chatgpt::logout(&store,&instance).await?}),
                );
            }
            let r = rt
                .logout(&AuthStatusOptions {
                    installation,
                    ..Default::default()
                })
                .await?;
            let status = match &r {
                crate::account::LogoutResult::LoggedOut => "complete",
                crate::account::LogoutResult::Failed { .. } => "failed",
                crate::account::LogoutResult::Unsupported { .. } => "unsupported",
            };
            Ok(json!({"status":status,"result":r}))
        })
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/projects/{project}/accounts/{runtime}", get(status))
        .route("/projects/{project}/accounts/{runtime}/quota", get(quota))
        .route(
            "/projects/{project}/accounts/{runtime}/methods",
            get(methods),
        )
        .route("/projects/{project}/accounts/{runtime}/login", post(login))
        .route(
            "/projects/{project}/accounts/{runtime}/logout",
            post(logout),
        )
        .route(
            "/projects/{project}/accounts/{runtime}/flows/{flow}",
            get(flow::status).delete(flow::cancel),
        )
}
