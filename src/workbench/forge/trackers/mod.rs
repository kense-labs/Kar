//! Host-local standalone issue tracker accounts and project scopes.
use super::*;
use axum::{
    Router,
    extract::{Path, State},
    routing::{get, post, put},
};
use std::{
    fs::File,
    io::{self, Read, Write},
    sync::Mutex,
};
mod connection;

const FIRST: &str = "default";
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Account {
    token: String,
    api: String,
    workspace: String,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    tracker: String,
    account_id: Option<Uuid>,
    scope: String,
    #[serde(default)]
    additional_scopes: Vec<String>,
}
impl Scope {
    fn account(&self) -> String {
        key(self.account_id)
    }
    fn permits(&self, scope: &str) -> bool {
        scope == self.scope || self.additional_scopes.iter().any(|s| s == scope)
    }
}
fn key(id: Option<Uuid>) -> String {
    id.map_or_else(|| FIRST.into(), |v| v.to_string())
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Data {
    accounts: BTreeMap<String, Account>,
    bindings: BTreeMap<Uuid, Scope>,
}
pub(in crate::workbench) struct Store {
    file: PathBuf,
    data: Mutex<Data>,
}
impl Store {
    pub(in crate::workbench) fn open(state: &std::path::Path) -> crate::Result<Self> {
        let file = state.join("issue-tracker-accounts.json");
        let data = match File::open(&file) {
            Ok(f) => serde_json::from_reader(f.take(16 * 1024 * 1024 + 1))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            file,
            data: Mutex::new(data),
        })
    }
    fn change<T>(
        &self,
        f: impl FnOnce(&mut Data) -> Result<T, ForgeError>,
    ) -> Result<T, ForgeError> {
        let mut lock = self.data.lock().unwrap();
        let mut next = lock.clone();
        let result = f(&mut next)?;
        let root = self.file.parent().expect("private workbench directory");
        let save = || -> io::Result<()> {
            let mut tmp = tempfile::NamedTempFile::new_in(root)?;
            serde_json::to_writer(&mut tmp, &next)?;
            if tmp.as_file().metadata()?.len() > 16 * 1024 * 1024 {
                return Err(io::Error::other("account store full"));
            }
            tmp.flush()?;
            tmp.as_file().sync_all()?;
            tmp.persist(&self.file)?;
            Ok(())
        };
        save().map_err(|_| ForgeError::bad("could not persist issue tracker accounts"))?;
        *lock = next;
        #[cfg(unix)]
        File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|_| ForgeError::bad("could not sync issue tracker accounts"))?;
        Ok(result)
    }
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/forge/issue-trackers", get(list))
        .route("/forge/issue-trackers/connect", post(connect))
        .route("/projects/{project}/forge/issue-tracker/binding", put(bind))
}
async fn list(State(app): State<Workbench>) -> Reply<Value> {
    use futures_util::{StreamExt, stream};
    let Data { accounts, bindings } = app.0.tracker_accounts.data.lock().unwrap().clone();
    let mut first = connection::entry(None, None, None);
    let mut extra = BTreeMap::new();
    let views = stream::iter(accounts.into_iter().map(|(id, account)| async move {
        let id = Uuid::parse_str(&id).ok();
        let probe = connection::probe(&account).await;
        (
            id,
            match probe {
                Ok(info) => connection::entry(id, Some(info), None),
                Err(err) => connection::entry(id, None, Some(err.detail)),
            },
        )
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;
    for (id, v) in views {
        if let Some(id) = id {
            extra.insert(id, v);
        } else {
            first = v
        }
    }
    let (normal, named): (BTreeMap<_, _>, BTreeMap<_, _>) = bindings
        .into_iter()
        .filter(|(p, _)| app.project(*p).is_ok())
        .partition(|(_, v)| v.account_id.is_none());
    Ok(Json(
        json!({"trackers":[first],"accounts":extra.into_values().collect::<Vec<_>>(),"bindings":normal,"account_bindings":named}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    tracker: String,
    account_id: Option<Uuid>,
    #[serde(default)]
    add_account: bool,
    #[serde(deserialize_with = "required_nullable")]
    token: Option<String>,
}
fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
fn require_kind(kind: &str) -> Result<(), ForgeError> {
    if kind == "linear" {
        Ok(())
    } else {
        Err(ForgeError::new(
            "unknown_tracker",
            400,
            "unknown standalone issue tracker",
        ))
    }
}
async fn connect(State(app): State<Workbench>, Json(input): Json<Connect>) -> Reply<Value> {
    require_kind(&input.tracker)?;
    if input.add_account && input.account_id.is_some() {
        return Err(ForgeError::bad(
            "add_account cannot replace a named account",
        ));
    }
    let id = if input.add_account {
        Some(Uuid::new_v4())
    } else {
        input.account_id
    };
    let handle = key(id);
    let mut info = None;
    let mut account = None;
    if let Some(token) = input.token {
        if token.trim() != token
            || token.is_empty()
            || token.len() > 4096
            || token.chars().any(char::is_control)
        {
            return Err(ForgeError::bad("invalid issue tracker credential"));
        }
        let api = std::env::var("KAR_LINEAR_API_BASE")
            .unwrap_or_else(|_| "https://api.linear.app/graphql".into());
        let mut incoming = Account {
            token,
            api: Context::base(&api)?.to_string(),
            workspace: String::new(),
        };
        let snapshot = connection::probe(&incoming).await.map_err(|e| {
            if e.status == 401 {
                ForgeError::new("rejected", 401, "Linear rejected this credential")
            } else {
                e
            }
        })?;
        incoming.workspace = snapshot.workspace.clone();
        account = Some(incoming);
        info = Some(snapshot);
    } else if input.add_account {
        return Err(ForgeError::bad("a new account requires a credential"));
    }
    app.0.tracker_accounts.change(move |db| {
        if input.account_id.is_some() && !db.accounts.contains_key(&handle) {
            return Err(ForgeError::bad("issue tracker account is not saved"));
        }
        if let Some(next) = account {
            if db
                .accounts
                .iter()
                .any(|(other, a)| other != &handle && a.token == next.token)
            {
                return Err(ForgeError::bad(
                    "credential is already connected as another account",
                ));
            }
            db.accounts.insert(handle, next);
        } else {
            db.accounts.remove(&handle);
            db.bindings.retain(|_, b| b.account() != handle);
        }
        Ok(())
    })?;
    Ok(Json(connection::entry(id, info, None)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bind {
    #[serde(deserialize_with = "required_nullable")]
    binding: Option<Scope>,
    account_id: Option<Uuid>,
}
async fn bind(
    State(app): State<Workbench>,
    Path(project): Path<Uuid>,
    Json(input): Json<Bind>,
) -> Reply<Value> {
    app.project(project)?;
    app.0.tracker_accounts.change(|db| {
        if let Some(mut scope) = input.binding {
            require_kind(&scope.tracker)?;
            if !db.accounts.contains_key(&scope.account()) {
                return Err(ForgeError::bad("issue tracker account is not connected"));
            }
            if scope.additional_scopes.len() > 250 {
                return Err(ForgeError::bad("too many additional issue scopes"));
            }
            let valid = |s: &str| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            };
            if !valid(&scope.scope) || scope.additional_scopes.iter().any(|s| !valid(s)) {
                return Err(ForgeError::bad("invalid Linear team key"));
            }
            scope.scope.make_ascii_uppercase();
            let mut seen = std::collections::BTreeSet::from([scope.scope.clone()]);
            scope.additional_scopes.retain_mut(|s| {
                s.make_ascii_uppercase();
                seen.insert(s.clone())
            });
            db.bindings.insert(project, scope);
        } else if db
            .bindings
            .get(&project)
            .is_some_and(|s| s.account_id == input.account_id)
        {
            db.bindings.remove(&project);
        }
        Ok(())
    })?;
    Ok(Json(Value::Null))
}
pub(in crate::workbench::forge) fn bound(
    app: &Workbench,
    project: Uuid,
    requested: Option<&str>,
) -> Result<Option<(Context, String)>, ForgeError> {
    let lock = app.0.tracker_accounts.data.lock().unwrap();
    let Some(b) = lock.bindings.get(&project) else {
        return Ok(None);
    };
    let account = lock
        .accounts
        .get(&b.account())
        .ok_or_else(|| ForgeError::remote(401, "bound account is unavailable"))?;
    let key = requested.unwrap_or(&b.scope).to_ascii_uppercase();
    if !b.permits(&key) {
        return Err(ForgeError::remote(
            403,
            "issue scope is not bound to this project",
        ));
    }
    let mut c = connection::context(account)?;
    c.cwd = app.project(project)?.root;
    c.repository = key;
    Ok(Some((c, account.workspace.clone())))
}
