use super::*;
use std::io::{Read, Write};

#[derive(Serialize, Deserialize)]
pub(super) struct Session {
    pub(super) access: String,
    expires: Option<i64>,
    refresh: Option<String>,
    refresh_expires: Option<i64>,
    client: String,
    api: String,
}
impl Session {
    pub(super) fn from_answer(e: &Endpoints, v: &Value) -> Option<Self> {
        let access = v["access_token"].as_str()?.trim();
        if access.is_empty() {
            return None;
        }
        let ms = now();
        Some(Self {
            access: access.to_owned(),
            expires: v["expires_in"].as_u64().map(|s| future(ms, s)),
            refresh: v["refresh_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            refresh_expires: v["refresh_token_expires_in"]
                .as_u64()
                .map(|s| future(ms, s)),
            client: e.client_id.clone(),
            api: e.context.api_base.to_string(),
        })
    }
}
fn file(app: &Workbench) -> PathBuf {
    app.0.state.join("github-app-session.json")
}
fn read(app: &Workbench, e: &Endpoints) -> Result<Option<Session>, ForgeError> {
    let f = match std::fs::File::open(file(app)) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure("failed", "could not read the saved GitHub sign-in")),
    };
    let stored: Session = serde_json::from_reader(f.take(65536))
        .map_err(|_| failure("failed", "invalid saved GitHub sign-in"))?;
    Ok(
        (stored.client == e.client_id && stored.api == e.context.api_base.as_str())
            .then_some(stored),
    )
}
pub(super) fn write(app: &Workbench, s: &Session) -> Result<(), ForgeError> {
    let save = || -> std::io::Result<()> {
        // NamedTempFile creates a user-only file beside the destination, outside project roots.
        let mut f = tempfile::NamedTempFile::new_in(&app.0.state)?;
        serde_json::to_writer(&mut f, s)?;
        f.flush()?;
        f.as_file().sync_all()?;
        f.persist(file(app))?;
        std::fs::File::open(&app.0.state)?.sync_all()
    };
    save().map_err(|_| failure("failed", "could not save the GitHub sign-in"))
}
pub(super) async fn token(app: &Workbench, e: &Endpoints) -> Result<Option<String>, ForgeError> {
    let _guard = app.0.signin.credential.lock().await;
    let Some(s) = read(app, e)? else {
        return Ok(None);
    };
    let ms = now();
    use sha2::{Digest, Sha256};
    let fingerprint: [u8; 32] = Sha256::digest(s.access.as_bytes()).into();
    let refused = app.0.signin.refused.lock().unwrap().contains(&fingerprint);
    if !refused && s.expires.is_none_or(|t| ms < t.saturating_sub(300000)) {
        return Ok(Some(s.access));
    }
    let use_old = !refused && s.expires.is_none_or(|t| ms < t);
    if let Some(refresh) = s
        .refresh
        .as_deref()
        .filter(|_| s.refresh_expires.is_none_or(|t| t > ms))
    {
        let answer = form(
            e,
            &["login", "oauth", "access_token"],
            &[
                ("client_id", &e.client_id),
                ("refresh_token", refresh),
                ("grant_type", "refresh_token"),
            ],
        )
        .await;
        match answer {
            Ok(v)
                if v["error"] == "bad_refresh_token"
                    || v["error"] == "incorrect_client_credentials" => {}
            Ok(v) => {
                if let Some(next) = Session::from_answer(e, &v) {
                    write(app, &next)?;
                    app.0.signin.refused.lock().unwrap().clear();
                    return Ok(Some(next.access));
                }
                if use_old {
                    return Ok(Some(s.access));
                }
                return Err(failure("failed", "GitHub could not refresh the sign-in"));
            }
            Err(_) if use_old => return Ok(Some(s.access)),
            Err(e) => return Err(e),
        }
    }
    std::fs::remove_file(file(app))
        .map_err(|_| failure("failed", "could not remove expired GitHub sign-in"))?;
    Ok(None)
}

pub(super) async fn covered(c: &Context, raw: &str) -> Result<bool, ForgeError> {
    let raw = raw
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    let mut parts = raw.rsplit(['/', ':']).filter(|s| !s.is_empty());
    let name = parts
        .next()
        .ok_or_else(|| ForgeError::bad("missing repository"))?;
    let owner = parts
        .next()
        .ok_or_else(|| ForgeError::bad("missing repository owner"))?;
    let repo = format!("{owner}/{name}");
    for page in 1..=10 {
        let q = page.to_string();
        let v = github::request(
            c,
            reqwest::Method::GET,
            &["user", "installations"],
            &[("per_page", "100"), ("page", &q)],
            None,
        )
        .await?;
        let rows = v["installations"]
            .as_array()
            .ok_or_else(|| ForgeError::remote(502, "invalid GitHub installations"))?;
        for row in rows {
            let Some(login) = row["account"]["login"].as_str() else {
                continue;
            };
            if !login.eq_ignore_ascii_case(owner) {
                continue;
            }
            if row["repository_selection"] == "all" {
                return Ok(true);
            }
            let installation = row["id"]
                .as_u64()
                .ok_or_else(|| ForgeError::remote(502, "invalid GitHub installation ID"))?
                .to_string();
            for page in 1..=10 {
                let q = page.to_string();
                let v = github::request(
                    c,
                    reqwest::Method::GET,
                    &["user", "installations", &installation, "repositories"],
                    &[("per_page", "100"), ("page", &q)],
                    None,
                )
                .await?;
                let rows = v["repositories"].as_array().ok_or_else(|| {
                    ForgeError::remote(502, "invalid GitHub installation repositories")
                })?;
                if rows.iter().any(|r| {
                    r["full_name"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&repo))
                }) {
                    return Ok(true);
                }
                if rows.len() < 100 {
                    break;
                }
                if page == 10 {
                    return Ok(true);
                }
            }
        }
        if rows.len() < 100 {
            break;
        }
    }
    Ok(false)
}
