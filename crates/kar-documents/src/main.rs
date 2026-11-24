use anyhow::{Context, Result, ensure};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use clap::Parser;
use kar_documents::{http::DocumentService, store::DocumentStore};
use serde_json::json;
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use subtle::ConstantTimeEq;
use tempfile::NamedTempFile;
use tokio::net::TcpListener;

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
    let app = DocumentService::new(store)
        .router()
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
