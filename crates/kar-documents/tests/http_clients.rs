use anyhow::{Context, Result, ensure};
use docs_engine::{Document, Selection};
use kar_documents::{
    OfficeEdits,
    store::{Action, Command, Decision, DocumentState, Receipt, Record, Status},
    word::WordEdit,
};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command as Process, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};
use tempfile::tempdir;

#[derive(Clone)]
struct Client {
    address: String,
    secret: String,
}

impl Client {
    fn send(&self, method: &str, path: &str, body: &[u8]) -> Result<TcpStream> {
        let mut socket = TcpStream::connect(&self.address)?;
        socket.set_read_timeout(Some(Duration::from_secs(15)))?;
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.address,
            self.secret,
            body.len()
        )?;
        socket.write_all(body)?;
        Ok(socket)
    }
    fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let mut stream = self.send(method, path, body)?;
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes)?;
        let at = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("missing HTTP headers")?;
        let status = std::str::from_utf8(&bytes[..at])?
            .split_whitespace()
            .nth(1)
            .context("missing status")?
            .parse()?;
        Ok((status, bytes[at + 4..].to_vec()))
    }
    fn submit(&self, command: &Command) -> Result<(u16, Receipt)> {
        let (code, body) = self.request("POST", "/operations", &serde_json::to_vec(command)?)?;
        Ok((code, serde_json::from_slice(&body)?))
    }
    fn events(&self, after: u64, wait: u64) -> Result<Vec<Record>> {
        let (code, body) =
            self.request("GET", &format!("/events?after={after}&wait_ms={wait}"), &[])?;
        ensure!(code == 200, "events HTTP {code}");
        Ok(serde_json::from_slice(&body)?)
    }
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(file: &Path, dir: &Path) -> Result<(Server, Client)> {
    let mut process = Server(
        Process::new(env!("CARGO_BIN_EXE_kar-documents"))
            .arg("--file")
            .arg(file)
            .arg("--state")
            .arg(dir)
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut line = String::new();
    BufReader::new(process.0.stdout.take().unwrap()).read_line(&mut line)?;
    let info: Value = serde_json::from_str(&line)?;
    let client = Client {
        address: info["address"].as_str().context("address")?.to_owned(),
        secret: fs::read_to_string(info["token_file"].as_str().context("token file")?)?,
    };
    Ok((process, client))
}
fn op(id: &str, paragraph: usize, value: &str) -> Command {
    Command {
        author: id.into(),
        operation_id: id.into(),
        base_revision: 0,
        action: Action::Edit {
            edits: OfficeEdits::Word(vec![WordEdit {
                paragraph,
                range: 0..3,
                replacement: value.into(),
            }]),
        },
    }
}

#[test]
fn independent_tcp_clients_subscribe_submit_retry_and_resolve_across_process_restart() -> Result<()>
{
    let dir = tempdir()?;
    let file = dir.path().join("demo.docx");
    let state = dir.path().join("state");
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "甲\n乙\n丙", None);
    doc.save_as(&file)?;
    let (server, a) = start(&file, &state)?;
    let b = a.clone();
    let unauthorized = Client {
        address: a.address.clone(),
        secret: "wrong".into(),
    };
    assert_eq!(unauthorized.request("GET", "/state", &[])?.0, 401);
    let both = Arc::new(Barrier::new(3));
    let mut clients = Vec::new();
    for (client, operation) in [
        (a.clone(), op("human", 0, "人")),
        (b.clone(), op("ai", 2, "机")),
    ] {
        let both = both.clone();
        clients.push(thread::spawn(move || -> Result<Receipt> {
            both.wait();
            let (status, receipt) = client.submit(&operation)?;
            ensure!(status == 200);
            Ok(receipt)
        }));
    }
    both.wait();
    for task in clients {
        assert_eq!(task.join().unwrap()?.status, Status::Applied);
    }
    let events = a.events(0, 0)?;
    assert_eq!(events.len(), 2);
    let second = b.events(0, 0)?;
    assert_eq!(serde_json::to_value(events)?, serde_json::to_value(second)?);
    let watch = b.clone();
    let waiting = thread::spawn(move || watch.events(2, 5000));
    let proposal = op("conflicting", 0, "新");
    let (status, conflict) = a.submit(&proposal)?;
    assert_eq!(status, 409);
    assert_eq!(waiting.join().unwrap()?[0].receipt.cursor, conflict.cursor);
    // The request reaches the server, but the submitting connection closes
    // without reading a receipt. A second client waits for its commit.
    let unacknowledged = op("lost-reply", 1, "另");
    let unread = a.send("POST", "/operations", &serde_json::to_vec(&unacknowledged)?)?;
    assert_eq!(
        b.events(conflict.cursor, 5000)?[0]
            .receipt
            .operation_id
            .as_deref(),
        Some("lost-reply")
    );
    drop(unread);
    drop(server);
    let (_restarted, fresh) = start(&file, &state)?;
    let (code, retry) = fresh.submit(&unacknowledged)?;
    assert_eq!(code, 200);
    assert_eq!(retry.cursor, 4);
    let (code, same_conflict) = fresh.submit(&proposal)?;
    assert_eq!(code, 409);
    assert_eq!(same_conflict.cursor, conflict.cursor);
    let (_, status) = fresh.request("GET", "/state", &[])?;
    let status: DocumentState = serde_json::from_slice(&status)?;
    assert_eq!(status.pending_conflicts, vec![conflict.cursor]);
    let resolved = Command {
        author: "owner".into(),
        operation_id: "finish".into(),
        base_revision: status.revision,
        action: Action::Resolve {
            conflict: conflict.cursor,
            decision: Decision::AdoptProposed,
        },
    };
    let (code, done) = fresh.submit(&resolved)?;
    assert_eq!(code, 200);
    let (code, bytes) = fresh.request("GET", &format!("/revisions/{}", done.revision), &[])?;
    assert_eq!(code, 200);
    assert_eq!(bytes, fs::read(&file)?);
    let doc = Document::open(&file)?;
    assert_eq!(
        doc.paragraphs()
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        ["新", "另", "机"]
    );
    Ok(())
}
