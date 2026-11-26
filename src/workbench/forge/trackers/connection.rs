use super::*;
pub(super) struct Probe {
    pub workspace: String,
    name: String,
    scopes: Vec<Value>,
}
pub(super) fn context(
    state_dir: &std::path::Path,
    handle: &str,
    account: &Account,
) -> Result<Context, ForgeError> {
    let api = Context::base(&account.api)?;
    let svc = crate::workbench::secrets::SecretService::open(state_dir)?;
    let token = svc.resolve_local_sync(&format!("tracker:{handle}"), &account.token)?;
    Ok(Context {
        kind: "linear".into(),
        host: api.authority().into(),
        repository: String::new(),
        token: Some(token.as_str().to_owned()),
        api_base: api,
        client: Context::client()?,
        signin: None,
        cwd: PathBuf::new(),
    })
}
pub(super) async fn probe(
    state_dir: &std::path::Path,
    handle: &str,
    account: &Account,
) -> Result<Probe, ForgeError> {
    let api = Context::base(&account.api)?;
    let svc = crate::workbench::secrets::SecretService::open(state_dir)?;
    let token = svc
        .resolve_secret_value(&format!("tracker:{handle}"), &account.token)
        .await?;
    let c = Context {
        kind: "linear".into(),
        host: api.authority().into(),
        repository: String::new(),
        token: Some(token.as_str().to_owned()),
        api_base: api,
        client: Context::client()?,
        signin: None,
        cwd: PathBuf::new(),
    };
    let mut after = None;
    let mut scopes = vec![];
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor_set = std::collections::BTreeSet::new();
    let mut identity = None;
    loop {
        let v=providers::linear::gql(&c,"query KarTrackerAccount($after:String){viewer{id name displayName} organization{urlKey} teams(first:100,after:$after){nodes{key name} pageInfo{hasNextPage endCursor}}}",json!({"after":after}),false).await?;
        let text = |v: &Value| {
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let name = text(&v["viewer"]["displayName"])
            .or_else(|| text(&v["viewer"]["name"]))
            .ok_or_else(|| ForgeError::remote(502, "Linear account name missing"))?;
        let workspace = text(&v["organization"]["urlKey"])
            .filter(|s| !s.contains('/'))
            .ok_or_else(|| ForgeError::remote(502, "Linear workspace is missing"))?;
        if identity
            .replace((name.clone(), workspace.clone()))
            .is_some_and(|i| i != (name.clone(), workspace.clone()))
        {
            return Err(ForgeError::remote(
                502,
                "Linear account changed during probe",
            ));
        }
        let teams = v["teams"]["nodes"]
            .as_array()
            .ok_or_else(|| ForgeError::remote(502, "Linear teams missing"))?;
        for team in teams {
            let key = text(&team["key"])
                .ok_or_else(|| ForgeError::remote(502, "Linear team key missing"))?;
            let name = text(&team["name"])
                .ok_or_else(|| ForgeError::remote(502, "Linear team name missing"))?;
            if seen.insert(key.clone()) {
                scopes.push(json!({"key":key,"name":name}))
            }
        }
        match v["teams"]["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => {
                return Ok(Probe {
                    name,
                    workspace,
                    scopes,
                });
            }
            Some(true) => {
                let next = text(&v["teams"]["pageInfo"]["endCursor"])
                    .filter(|s| cursor_set.insert(s.clone()))
                    .ok_or_else(|| ForgeError::remote(502, "invalid Linear team cursor"))?;
                if cursor_set.len() > 100 {
                    return Err(ForgeError::remote(
                        502,
                        "Linear team discovery exceeded the page limit",
                    ));
                }
                after = Some(next);
            }
            None => return Err(ForgeError::remote(502, "Linear team pagination is missing")),
        }
    }
}
pub(super) fn entry(id: Option<Uuid>, p: Option<Probe>, detail: Option<String>) -> Value {
    let connected = p.is_some() || detail.is_some();
    let mut v = json!({"tracker":"linear","display_name":"Linear","host":"linear.app","scope_noun":"team",
        "credential":{"label":"Personal API key","help_url":"https://linear.app/settings/account/security"},
        "connected":connected,"account":null,"detail":detail,"scopes":[]});
    if let Some(id) = id {
        v["account_id"] = json!(id)
    }
    if let Some(p) = p {
        v["workspace"] = json!(p.workspace);
        v["account"] = json!(p.name);
        v["scopes"] = json!(p.scopes)
    }
    v
}
