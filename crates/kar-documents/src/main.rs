use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::DefaultBodyLimit,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use kar_documents::store::{
    Command, DocumentState, DocumentStore, Receipt, Record, Rejection, Status,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tempfile::NamedTempFile;
use tokio::{
    net::TcpListener,
    sync::watch,
    time::{Instant, timeout_at},
};

#[derive(Parser)]
#[command(about = "Serve one local Kar document with durable revisions and conflict receipts")]
struct Args {
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    state: PathBuf,
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
}

#[derive(Clone)]
struct App {
    store: Arc<Mutex<DocumentStore>>,
    changed: watch::Sender<u64>,
}

struct Failure(anyhow::Error);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let code = if self.0.is::<Rejection>() {
            StatusCode::CONFLICT
        } else if self.0.is::<std::io::Error>() {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        let mut body = json!({"error":self.0.to_string()});
        if let Some(Rejection::StaleResolution { current, .. }) = self.0.downcast_ref::<Rejection>()
        {
            body["current_revision"] = json!(current);
        }
        (code, Json(body)).into_response()
    }
}

impl App {
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut DocumentStore) -> Result<T> + Send + 'static,
    ) -> Result<T, Failure> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| anyhow::anyhow!("document lock poisoned"))?;
            f(&mut store)
        })
        .await
        .map_err(|e| Failure(e.into()))?
        .map_err(Failure)
    }
}

async fn state(State(app): State<App>) -> Result<Json<DocumentState>, Failure> {
    app.run(DocumentStore::state).await.map(Json)
}

async fn snapshot(State(app): State<App>, Path(rev): Path<u64>) -> Result<Response, Failure> {
    let bytes = app.run(move |s| s.snapshot(rev)).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn stage(State(app): State<App>, bytes: Bytes) -> Result<Json<serde_json::Value>, Failure> {
    let digest = app.run(move |s| s.stage(&bytes)).await?;
    Ok(Json(json!({"sha256":digest})))
}
async fn proposal(State(app): State<App>, Path(digest): Path<String>) -> Result<Response, Failure> {
    let bytes = app.run(move |s| s.proposal(&digest)).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn submit(
    State(app): State<App>,
    Json(command): Json<Command>,
) -> Result<(StatusCode, Json<Receipt>), Failure> {
    let receipt = app.run(move |s| s.submit(command)).await?;
    app.changed.send_replace(receipt.cursor);
    let code = if receipt.status == Status::Conflict {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    Ok((code, Json(receipt)))
}

#[derive(Deserialize, Default)]
struct Events {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    wait_ms: u64,
}

async fn events(
    State(app): State<App>,
    Query(args): Query<Events>,
) -> Result<Json<Vec<Record>>, Failure> {
    let mut changed = app.changed.subscribe();
    let deadline = Instant::now() + Duration::from_millis(args.wait_ms.min(30_000));
    loop {
        let records = app.run(move |s| s.events(args.after)).await?;
        if !records.is_empty() || Instant::now() >= deadline {
            return Ok(Json(records));
        }
        // Check the working file periodically for saves made by an external editor.
        let until = deadline.min(Instant::now() + Duration::from_millis(500));
        let _ = timeout_at(until, changed.changed()).await;
    }
}

fn token(state: &FsPath) -> Result<(PathBuf, String)> {
    let path = state.join("access-token");
    if !path.try_exists()? {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
        let mut file = NamedTempFile::new_in(state)?;
        for byte in random {
            write!(file, "{byte:02x}")?;
        }
        file.as_file().sync_all()?;
        file.persist_noclobber(&path)?;
    }
    let secret = fs::read_to_string(&path)?;
    ensure!(
        secret.len() == 64 && secret.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid document access token"
    );
    Ok((path.canonicalize()?, secret))
}

async fn authenticate(State(secret): State<Arc<String>>, req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(bearer.as_bytes().ct_eq(secret.as_bytes())) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.listen.ip().is_loopback(),
        "document service requires a loopback listen address"
    );
    let store = DocumentStore::open(&args.state, &args.file)?;
    let (token_file, secret) = token(&args.state)?;
    let (changed, _) = watch::channel(0);
    let app = Router::new()
        .route("/state", get(state))
        .route(
            "/proposals",
            post(stage).layer(DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route("/proposals/{digest}", get(proposal))
        .route("/revisions/{revision}", get(snapshot))
        .route("/events", get(events))
        .route("/operations", post(submit))
        .with_state(App {
            store: Arc::new(Mutex::new(store)),
            changed,
        })
        .layer(middleware::from_fn_with_state(
            Arc::new(secret),
            authenticate,
        ));
    let listener = TcpListener::bind(args.listen).await?;
    println!(
        "{}",
        json!({"address":listener.local_addr()?.to_string(),"token_file":token_file})
    );
    std::io::stdout()
        .flush()
        .context("write document service address")?;
    axum::serve(listener, app).await?;
    Ok(())
}
