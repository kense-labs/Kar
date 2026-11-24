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

struct Fixture {
    human: OfficeEdits,
    ai: OfficeEdits,
    alternative: OfficeEdits,
}

fn fixture(path: &Path, format: &str) -> Result<Fixture> {
    use kar_documents::{
        Change,
        excel::{Cell, CellEdit},
        slides::{Object, ObjectEdit},
    };
    let change = |a: &str, b: &str| Change {
        base: a.to_owned(),
        proposed: b.to_owned(),
    };
    match format {
        "docx" => {
            let mut d = Document::new();
            d.insert_text(Selection::default(), "周一\n小李", None);
            d.save_as(path)?;
            let edit = |p, value: &str| {
                OfficeEdits::Word(vec![WordEdit {
                    paragraph: p,
                    range: 0..6,
                    replacement: value.into(),
                }])
            };
            Ok(Fixture {
                human: edit(0, "周三"),
                ai: edit(1, "小林"),
                alternative: edit(0, "周五"),
            })
        }
        "xlsx" => {
            let mut b = sheets_engine::Workbook::new_empty()?;
            b.rename_sheet(0, "预算")?;
            b.set_input(0, 2, 1, "10")?;
            b.set_input(0, 2, 3, "=B3*2")?;
            b.save_as(path)?;
            let b3 = Cell::at(&b, 0, 2, 1)?;
            let d3 = Cell::at(&b, 0, 2, 3)?;
            let edit = |cell, a, b| OfficeEdits::Excel(vec![(cell, CellEdit::Input(change(a, b)))]);
            Ok(Fixture {
                human: edit(b3.clone(), "10", "12"),
                ai: edit(d3, "=B3*2", "=B3*3"),
                alternative: edit(b3, "10", "15"),
            })
        }
        "pptx" => {
            let mut d = slides_engine::Presentation::new();
            let i = d.add_text_box(0)?;
            d.set_shape_text((0, i), "初稿")?;
            let obj = Object::at(&d, 0, i)?;
            let x = d.slide(0).shapes[i].xfrm.unwrap().x;
            d.save_as(path)?;
            let text =
                |v| OfficeEdits::Slides(vec![(obj.clone(), ObjectEdit::Text(change("初稿", v)))]);
            Ok(Fixture {
                human: text("二稿"),
                ai: OfficeEdits::Slides(vec![(
                    obj.clone(),
                    ObjectEdit::X(Change {
                        base: x,
                        proposed: 121212,
                    }),
                )]),
                alternative: text("终稿"),
            })
        }
        _ => anyhow::bail!("unknown fixture"),
    }
}

fn verify_file(path: &Path, format: &str, resolved: bool) -> Result<()> {
    match format {
        "docx" => {
            let d = Document::open(path)?;
            assert_eq!(
                d.paragraphs()[0].text,
                if resolved { "周五" } else { "周三" }
            );
            assert_eq!(d.paragraphs()[1].text, "小林");
        }
        "xlsx" => {
            let b = sheets_engine::Workbook::open(path)?;
            assert_eq!(b.input(0, 2, 1), if resolved { "15" } else { "12" });
            assert_eq!(b.input(0, 2, 3), "=B3*3");
            assert_eq!(b.display_value(0, 2, 3), if resolved { "45" } else { "36" });
        }
        "pptx" => {
            let d = slides_engine::Presentation::open(path)?;
            let shape = d
                .slide(0)
                .shapes
                .iter()
                .find(|s| s.xfrm.is_some_and(|v| v.x == 121212))
                .context("moved shape")?;
            assert_eq!(
                shape.text.as_ref().unwrap().plain_text(),
                if resolved { "终稿" } else { "二稿" }
            );
        }
        _ => anyhow::bail!("unknown fixture"),
    }
    Ok(())
}

#[test]
fn all_office_formats_support_both_orders_and_simultaneous_service_clients() -> Result<()> {
    for format in ["docx", "xlsx", "pptx"] {
        for order in ["human first", "AI first", "simultaneous"] {
            let dir = tempdir()?;
            let file = dir.path().join(format!("work.{format}"));
            let state = dir.path().join("store");
            let fixture = fixture(&file, format)?;
            let (server, human) = start(&file, &state)?;
            let ai = human.clone();
            let command = |author: &str, id: &str, edits| Command {
                author: author.into(),
                operation_id: id.into(),
                base_revision: 0,
                action: Action::Edit { edits },
            };
            let h = command("human", "one", fixture.human);
            let a = command("AI", "two", fixture.ai);
            match order {
                "human first" => {
                    assert_eq!(human.submit(&h)?.0, 200);
                    assert_eq!(ai.submit(&a)?.0, 200);
                }
                "AI first" => {
                    assert_eq!(ai.submit(&a)?.0, 200);
                    assert_eq!(human.submit(&h)?.0, 200);
                }
                _ => {
                    let gate = Arc::new(Barrier::new(2));
                    let thread_gate = gate.clone();
                    let c = ai.clone();
                    let worker = thread::spawn(move || {
                        thread_gate.wait();
                        c.submit(&a)
                    });
                    gate.wait();
                    assert_eq!(human.submit(&h)?.0, 200);
                    assert_eq!(worker.join().unwrap()?.0, 200);
                }
            }
            verify_file(&file, format, false)?;
            assert_eq!(
                serde_json::to_value(human.events(0, 0)?)?,
                serde_json::to_value(ai.events(0, 0)?)?
            );
            let before = fs::read(&file)?;
            let disputed = command("AI", "three", fixture.alternative);
            let (code, dispute) = ai.submit(&disputed)?;
            assert_eq!(code, 409);
            assert_eq!(dispute.revision, 2);
            assert_eq!(fs::read(&file)?, before);
            let both = human.events(2, 0)?;
            assert_eq!(both[0].receipt.cursor, dispute.cursor);
            drop(server);
            let (_process, again) = start(&file, &state)?;
            let (code, retry) = again.submit(&disputed)?;
            assert_eq!(code, 409);
            assert_eq!(retry.cursor, dispute.cursor);
            assert_eq!(again.events(2, 0)?.len(), 1);
            let resolve = Command {
                author: "human".into(),
                operation_id: "resolve".into(),
                base_revision: 2,
                action: Action::Resolve {
                    conflict: dispute.cursor,
                    decision: Decision::AdoptProposed,
                },
            };
            let (code, done) = again.submit(&resolve)?;
            assert_eq!(code, 200);
            assert_eq!(done.revision, 3);
            verify_file(&file, format, true)?;
        }
    }
    Ok(())
}
