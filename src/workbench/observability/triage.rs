//! Per-request diagnostic context and commit-addressed, atomically published Kar source.
use super::*;
use axum::{Json, extract::State};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(super) struct Triage {
    base: PathBuf,
    source: String,
    cloning: Mutex<BTreeSet<String>>,
}
impl Triage {
    pub(super) fn new(state: &Path) -> Self {
        Self {
            base: state.join("triage"),
            source: std::env::var("KAR_TRIAGE_REPOSITORY")
                .unwrap_or_else(|_| "https://github.com/kense-labs/Kar.git".into()),
            cloning: Default::default(),
        }
    }
    async fn source(self: &Arc<Self>) -> Value {
        match self.resolve().await {
            Ok(v) => v,
            Err(e) => json!({"status":"failed","reason":e.to_string()}),
        }
    }
    async fn resolve(self: &Arc<Self>) -> Result<Value> {
        let cache = self.base.join("source");
        fs::create_dir_all(&cache)?;
        let tag = format!("refs/tags/v{}", env!("CARGO_PKG_VERSION"));
        let peeled = format!("{tag}^{{}}");
        let refs = git(
            &cache,
            &[
                "ls-remote",
                "--",
                &self.source,
                &tag,
                &peeled,
                "refs/heads/main",
            ],
            5,
        )
        .await?;
        let refs = refs
            .lines()
            .filter_map(|s| s.split_once('\t'))
            .map(|(v, k)| (k, v))
            .collect::<BTreeMap<_, _>>();
        let (reference, commit) = [
            (peeled.as_str(), tag.as_str()),
            (tag.as_str(), tag.as_str()),
            ("refs/heads/main", "refs/heads/main"),
        ]
        .into_iter()
        .find_map(|(key, label)| refs.get(key).map(|&c| (label, c)))
        .ok_or_else(|| Error::Refused("Kar source has neither this release tag nor main".into()))?;
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(Error::Refused("invalid Kar source commit".into()));
        }
        let dest = cache.join(commit);
        let mut info = json!({"ref":reference,"commit":commit,"directory":dest});
        if dest.exists() {
            if !dest.join(".git").is_dir()
                || git(&dest, &["rev-parse", "HEAD"], 10).await?.trim() != commit
            {
                return Err(Error::Refused(
                    "incomplete cached Kar source checkout".into(),
                ));
            }
            info["status"] = json!("cloned");
            return Ok(info);
        }
        if self.cloning.lock().unwrap().insert(commit.to_owned()) {
            let service = self.clone();
            let commit = commit.to_owned();
            tokio::spawn(async move {
                let result = service.clone_commit(&cache, &dest, &commit).await;
                let v = match result {
                    Ok(()) => json!({"status":"cloned","commit":commit,"directory":dest}),
                    Err(e) => json!({"status":"failed","reason":e.to_string(),"commit":commit}),
                };
                let _ = atomic(
                    &cache.join(format!("{commit}.json")),
                    &serde_json::to_vec(&v).expect("source status"),
                );
                service.cloning.lock().unwrap().remove(&commit);
            });
        }
        info["status"] = json!("cloning");
        Ok(info)
    }
    async fn clone_commit(&self, cache: &Path, dest: &Path, commit: &str) -> Result<()> {
        let staging = self.base.join("source-staging");
        fs::create_dir_all(&staging)?;
        let temp = tempfile::Builder::new()
            .prefix("kar-")
            .tempdir_in(staging)?;
        git(temp.path(), &["init", "--quiet"], 10).await?;
        git(temp.path(), &["remote", "add", "origin", &self.source], 10).await?;
        git(
            temp.path(),
            &[
                "fetch",
                "--depth",
                "1",
                "--filter=blob:none",
                "origin",
                commit,
            ],
            600,
        )
        .await?;
        git(
            temp.path(),
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            600,
        )
        .await?;
        if git(temp.path(), &["rev-parse", "HEAD"], 10).await?.trim() != commit {
            return Err(Error::Refused(
                "Kar source checkout differs from the requested commit".into(),
            ));
        }
        match fs::rename(temp.path(), dest) {
            Ok(()) => {}
            Err(e)
                if dest.join(".git").is_dir()
                    && git(dest, &["rev-parse", "HEAD"], 10).await?.trim() == commit =>
            {
                let _ = e;
            }
            Err(e) => return Err(e.into()),
        }
        #[cfg(unix)]
        File::open(cache)?.sync_all()?;
        Ok(())
    }
}
async fn git(cwd: &Path, args: &[&str], seconds: u64) -> Result<String> {
    let mut cmd = tokio::process::Command::from(crate::workbench_git::command(cwd));
    cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "credential.helper="])
        .args(args)
        .env("GIT_ASKPASS", "")
        .env("LANGUAGE", "C")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(seconds), cmd.output())
        .await
        .map_err(|_| Error::Closed("Kar source query timed out".into()))??;
    if !output.status.success() {
        return Err(Error::Refused(format!(
            "Kar source Git operation failed ({})",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| Error::Refused("invalid Kar source response".into()))
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = tempfile::NamedTempFile::new_in(path.parent().expect("triage file"))?;
    f.write_all(bytes)?;
    f.as_file().sync_all()?;
    f.persist(path).map_err(|e| e.error)?;
    Ok(())
}
#[derive(Deserialize, serde::Serialize)]
pub(super) struct Client {
    label: Option<String>,
    app_version: Option<String>,
    os: Option<String>,
    browser: Option<String>,
    web_deployment: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct Prepare {
    client: Client,
}
pub(super) async fn prepare(
    State(app): State<Workbench>,
    Json(q): Json<Prepare>,
) -> std::result::Result<Json<Value>, super::super::http::ApiError> {
    let triage = &app.0.observability.triage;
    let source = triage.source().await;
    let path = triage
        .base
        .join("sessions")
        .join(Uuid::new_v4().to_string());
    let sessions = path.parent().expect("triage session directory");
    fs::create_dir_all(sessions).map_err(Error::from)?;
    let staging = tempfile::Builder::new()
        .prefix("kar-")
        .tempdir_in(sessions)
        .map_err(Error::from)?;
    let context = path.join("context.md");
    let playbook = path.join("triage-playbook.md");
    let client = serde_json::to_string(&q.client).map_err(Error::from)?;
    let source_record = source["commit"]
        .as_str()
        .map(|c| triage.base.join("source").join(format!("{c}.json")));
    let data = format!(
        "# Kar diagnostic context\n\nCreated {} on {} {}. Kar {} is running as PID {}.\n\nClient metadata\n\n{}\n\nTrace file\n\n{}\n\nSource checkout\n\n{}\n\nSource status record\n\n{}\n",
        stamp(now()),
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        client,
        app.0.observability.traces.path(0).display(),
        source,
        source_record
            .as_deref()
            .map(Path::display)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "Unavailable".into())
    );
    atomic(&staging.path().join("context.md"), data.as_bytes())?;
    atomic(&staging.path().join("triage-playbook.md"),b"# Kar triage playbook\n\nAsk the user what went wrong and what they expected. Record the approximate time and the smallest reproducible action. Read the sibling context.md for the Kar build, client, logs and resolved source checkout. Correlate the reported action with the trace time and request name. Inspect the matching source version and report the observed behavior, evidence and outstanding uncertainty.\n")?;
    fs::rename(staging.path(), &path).map_err(Error::from)?;
    #[cfg(unix)]
    File::open(sessions)
        .and_then(|d| d.sync_all())
        .map_err(Error::from)?;
    Ok(Json(
        json!({"prompt":format!("Read [{}]({}) and follow the Kar triage playbook.",playbook.file_name().unwrap().to_string_lossy(),playbook.display()),"prompt_path":playbook,"context_path":context,"source":source}),
    ))
}
