//! Owner-managed launch profiles. Saved chats pin a revision, while the profile list
//! advertises the latest one. Removing a profile never changes an external installation.
use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Local {
        command_path: PathBuf,
        #[serde(default)]
        command_args: Vec<String>,
    },
    Registry {
        runtime: Option<String>,
        agent_id: Option<String>,
        distribution: Option<String>,
        command_path: Option<PathBuf>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    name: String,
    #[serde(flatten)]
    source: Input,
    #[serde(default)]
    auth_method_id: Option<String>,
    #[serde(default)]
    custom_models: Vec<crate::forms::ModelEntry>,
}
#[derive(Clone, Serialize, Deserialize)]
enum Launcher {
    Local { program: PathBuf, launch: Launch },
    Managed { runtime: String },
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Profile {
    name: String,
    launcher: Launcher,
    pub(super) auth_method_id: Option<String>,
    pub(super) custom_models: Vec<crate::forms::ModelEntry>,
}
fn base(store: &ChatStore, id: &str) -> Result<PathBuf> {
    if !catalog::id_ok(id) {
        return Err(bad("invalid ACP profile ID"));
    }
    Ok(root(store).join("profiles").join(id))
}
pub(super) fn load(store: &ChatStore, runtime: &str) -> Result<Profile> {
    let fields = runtime.split(':').collect::<Vec<_>>();
    let ["acp-profile", id, revision] = fields.as_slice() else {
        return Err(bad("invalid ACP profile handle"));
    };
    if revision.len() != 64 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("invalid ACP profile revision"));
    }
    let dir = base(store, id)?;
    Ok(serde_json::from_reader(File::open(
        dir.join("versions").join(revision),
    )?)?)
}
fn executable(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(bad("ACP launcher must have an absolute host path"));
    }
    let p = path.canonicalize()?;
    if !p.is_file() {
        return Err(bad("ACP launcher must be a file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if p.metadata()?.permissions().mode() & 0o111 == 0 {
            return Err(bad("ACP launcher is not executable"));
        }
    }
    #[cfg(windows)]
    if p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("bat") || s.eq_ignore_ascii_case("cmd"))
    {
        return Err(bad(
            "supply the underlying program rather than a shell batch file",
        ));
    }
    Ok(p)
}
pub(super) fn resolve(
    store: &ChatStore,
    runtime: &str,
    opts: &mut SessionOptions,
) -> Result<Runtime> {
    let p = load(store, runtime)?;
    match p.launcher {
        Launcher::Local { program, launch } => {
            configure(&launch, opts);
            let args = launch.args.iter().map(String::as_str).collect::<Vec<_>>();
            Ok(Runtime::new(
                runtime,
                executable(&program)?,
                &args,
                Protocol::Acp,
            ))
        }
        Launcher::Managed { runtime: base } => {
            let mut rt = super::resolve(store, &base, opts)?;
            rt.id = runtime.into();
            Ok(rt)
        }
    }
}
pub(super) fn owner_routes() -> Router<Workbench> {
    Router::new()
        .route("/acp-registry/profiles", get(index))
        .route(
            "/acp-registry/profiles/{id}",
            axum::routing::put(configure_profile).delete(remove_profile),
        )
}
fn summary(id: &str, runtime: &str, p: &Profile) -> Value {
    json!({"id":id,"runtime":runtime,"name":p.name,"custom_models":p.custom_models,"auth_method_id":p.auth_method_id})
}
fn visible(store: &ChatStore) -> Result<Vec<Value>> {
    let mut out = vec![];
    for dir in subdirs(&root(store).join("profiles"))? {
        let id = dir.file_name().unwrap().to_string_lossy();
        let current = match fs::read_to_string(dir.join("current")) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let runtime: String = serde_json::from_str(&current)?;
        let p = load(store, &runtime)?;
        out.push(summary(&id, &runtime, &p));
    }
    Ok(out)
}
pub(super) fn instances(store: &ChatStore) -> Result<Vec<Value>> {
    visible(store)
}
async fn index(State(app): State<Workbench>) -> Reply {
    super::super::http::blocking(move || Ok(json!({"profiles":visible(&app.0.chats)?}))).await
}

async fn configure_profile(
    State(app): State<Workbench>,
    Param(id): Param<String>,
    Json(input): Json<Settings>,
) -> Reply {
    let store = &app.0.chats;
    let dir = base(store, &id)?;
    if input.name.trim().is_empty()
        || input.name.len() > 160
        || input
            .auth_method_id
            .as_ref()
            .is_some_and(|v| v.trim().is_empty() || v.len() > 256)
        || input.custom_models.len() > 128
        || input
            .custom_models
            .iter()
            .any(|m| m.id.trim().is_empty() || m.id.len() > 256)
    {
        return Err(bad("invalid ACP model or profile settings").into());
    }
    let launcher = match input.source {
        Input::Local {
            command_path,
            command_args,
        } => {
            if command_args.len() > 64
                || command_args
                    .iter()
                    .any(|a| a.len() > 1024 || a.contains('\0'))
            {
                return Err(bad("invalid ACP launcher arguments").into());
            }
            Launcher::Local {
                program: executable(&command_path)?,
                launch: Launch {
                    args: command_args,
                    env: Default::default(),
                },
            }
        }
        Input::Registry {
            runtime,
            agent_id,
            distribution,
            command_path,
        } => {
            let release = match (runtime, agent_id) {
                (Some(runtime), None) => {
                    let key = Key::parse(&runtime)?;
                    let r: Release = serde_json::from_reader(
                        File::open(key.path_in(&root(store).join("releases")))
                            .map_err(Error::from)?,
                    )
                    .map_err(Error::from)?;
                    if r.runtime != runtime {
                        return Err(bad("invalid ACP registry release").into());
                    }
                    r
                }
                (None, Some(id)) => {
                    latest(&app, &id, distribution.as_deref().unwrap_or("auto")).await?
                }
                _ => return Err(bad("select one registry runtime or agent ID").into()),
            };
            if let Some(program) = command_path {
                Launcher::Local {
                    program: executable(&program)?,
                    launch: release.distribution.launch().clone(),
                }
            } else {
                install(&app, &release.runtime, Some(&release)).await?;
                Launcher::Managed {
                    runtime: release.runtime,
                }
            }
        }
    };
    let profile = Profile {
        name: input.name,
        auth_method_id: input.auth_method_id,
        custom_models: input.custom_models,
        launcher,
    };
    let data = serde_json::to_vec(&profile).map_err(Error::from)?;
    let revision = format!("{:x}", Sha256::digest(data));
    let runtime = format!("acp-profile:{id}:{revision}");
    let _lock = lock_path(&root(store).join("profile-locks").join(&id), true)?;
    record(&dir.join("versions").join(revision), &profile)?;
    record(&dir.join("current"), &runtime)?;
    Ok(Json(summary(&id, &runtime, &profile)))
}
async fn remove_profile(State(app): State<Workbench>, Param(id): Param<String>) -> Reply {
    super::super::http::blocking(move || {
        let store = &app.0.chats;
        let dir = base(store, &id)?;
        let _lock = lock_path(&root(store).join("profile-locks").join(&id), true)?;
        if !dir.try_exists()? {
            return Ok(json!({"removed":false}));
        }
        let trash = tempfile::tempdir_in(root(store))?;
        fs::rename(dir, trash.path().join("profile"))?;
        trash.close()?;
        Ok(json!({"removed":true}))
    })
    .await
}
