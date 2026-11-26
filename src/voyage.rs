//! Import and record Kar/Oar voyage v3 session recordings and raw Record JSONL streams.
use crate::{Direction, Error, Event, Outcome, Record, RecordKind, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::BufRead;
#[cfg(not(target_arch = "wasm32"))]
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VoyageSummary {
    pub stream_id: Uuid,
    pub header: Value,
    pub end: Option<Value>,
    pub records: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordEncoding {
    Upstream,
    OarRecordV1,
    KarRecordV1(Uuid),
}

/// Header metadata written at line 1 of a `kar-voyage/3` recording.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoyageHeader {
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub cwd: PathBuf,
    pub session_id: String,
    pub stream_id: Uuid,
    pub started_at: u64,
    pub recorder: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl VoyageHeader {
    pub fn new(
        runtime: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session_id: impl Into<String>,
        stream_id: Uuid,
    ) -> Self {
        Self {
            runtime: runtime.into(),
            model: None,
            effort: None,
            service_tier: None,
            cwd: cwd.into(),
            session_id: session_id.into(),
            stream_id,
            started_at: crate::stream::timestamp_ms(),
            recorder: "kar".into(),
        }
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    pub fn with_service_tier(mut self, service_tier: Option<String>) -> Self {
        self.service_tier = service_tier;
        self
    }

    pub fn with_recorder(mut self, recorder: impl Into<String>) -> Self {
        self.recorder = recorder.into();
        self
    }

    fn to_header_value(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("kind".into(), "header".into());
        obj.insert("format".into(), "kar-voyage/3".into());
        obj.insert("recordEncoding".into(), "kar-record/1".into());
        obj.insert("recorder".into(), self.recorder.clone().into());
        obj.insert("runtime".into(), self.runtime.clone().into());
        if let Some(model) = &self.model {
            obj.insert("model".into(), model.clone().into());
        }
        if let Some(effort) = &self.effort {
            obj.insert("effort".into(), effort.clone().into());
        }
        if let Some(tier) = &self.service_tier {
            obj.insert("serviceTier".into(), tier.clone().into());
        }
        obj.insert("cwd".into(), json!(self.cwd));
        obj.insert("sessionId".into(), self.session_id.clone().into());
        obj.insert("streamId".into(), json!(self.stream_id));
        obj.insert("startedAt".into(), self.started_at.into());
        Value::Object(obj)
    }
}

/// Crash-safe synchronous `kar-voyage/3` recorder.
///
/// Opens the target file with mode `0600` (on Unix) and flushes + `sync_data()`s
/// the header and every individual `Record` frame synchronously so tail frames
/// survive process panics or `SIGKILL`.
#[cfg(not(target_arch = "wasm32"))]
pub struct VoyageRecorder {
    file: File,
    stream_id: Uuid,
    records: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl VoyageRecorder {
    /// Create or truncate a `0600` voyage file and synchronously persist its header.
    pub fn create(path: &Path, header: &VoyageHeader) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        Self::init(file, header)
    }

    /// Exclusively create a new `0600` voyage file (never overwriting an existing file)
    /// and synchronously persist its header.
    pub fn create_new(path: &Path, header: &VoyageHeader) -> Result<Self> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(path)?;
        Self::init(file, header)
    }

    /// Open a crash-safe voyage recorder bound to a live [`crate::Session`].
    pub fn for_session(path: &Path, runtime: &str, session: &crate::Session) -> Result<Self> {
        let header = VoyageHeader::new(
            runtime,
            session.cwd().to_path_buf(),
            session.id(),
            session.stream_id(),
        )
        .with_model(session.model())
        .with_effort(session.effort())
        .with_service_tier(session.service_tier());
        Self::create_new(path, &header)
    }

    fn init(mut file: File, header: &VoyageHeader) -> Result<Self> {
        Self::write_frame_sync(&mut file, &header.to_header_value())?;
        Ok(Self {
            file,
            stream_id: header.stream_id,
            records: 0,
        })
    }

    fn write_frame_sync(file: &mut File, value: &impl Serialize) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    /// Synchronously append and `sync_data()` one [`Record`] frame.
    pub fn record(&mut self, record: &Record) -> Result<()> {
        if record.stream_id != self.stream_id {
            return Err(Error::Protocol(
                "voyage record stream_id does not match header streamId".into(),
            ));
        }
        if record.seq != self.records {
            return Err(Error::Protocol(format!(
                "expected contiguous voyage record seq {}, got {}",
                self.records, record.seq
            )));
        }
        #[derive(Serialize)]
        struct Entry<'a> {
            kind: &'static str,
            record: &'a Record,
        }
        Self::write_frame_sync(
            &mut self.file,
            &Entry {
                kind: "record",
                record,
            },
        )?;
        self.records += 1;
        Ok(())
    }

    pub fn records_written(&self) -> u64 {
        self.records
    }

    pub fn stream_id(&self) -> Uuid {
        self.stream_id
    }

    /// Write the terminal `"end"` frame with `"closed"` reason and `sync_all()`.
    pub fn finish(self) -> Result<u64> {
        self.finish_with_reason("closed")
    }

    /// Write the terminal `"end"` frame with a custom reason and `sync_all()`.
    pub fn finish_with_reason(mut self, reason: &str) -> Result<u64> {
        let end = json!({
            "kind": "end",
            "at": crate::stream::timestamp_ms(),
            "reason": reason,
            "records": self.records,
        });
        let mut bytes = serde_json::to_vec(&end)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(self.records)
    }
}

/// Import a strict `kar-voyage/3` recording.
///
/// A missing end marker describes an interrupted capture. Invalid records are errors.
/// Use [`import_records_or_voyage`] when also accepting upstream `oar-voyage/3` or raw JSONL recordings.
pub fn import(
    reader: impl BufRead,
    mut accept: impl FnMut(Record) -> Result<()>,
) -> Result<VoyageSummary> {
    let mut lines = reader.lines();
    let first = lines
        .next()
        .transpose()?
        .ok_or_else(|| Error::Protocol("empty voyage".into()))?;
    let header: Value = serde_json::from_str(&first)?;
    import_with_header(header, lines, false, &mut accept)
}

/// Import a `kar-voyage/3` recording, an `oar-voyage/3` recording, or a headless
/// raw `Record` JSONL file (such as legacy `kar run --record` output).
pub fn import_records_or_voyage(
    reader: impl BufRead,
    mut accept: impl FnMut(Record) -> Result<()>,
) -> Result<VoyageSummary> {
    let mut lines = reader.lines();
    let first = lines
        .next()
        .transpose()?
        .ok_or_else(|| Error::Protocol("empty recording".into()))?;
    let first_val: Value = serde_json::from_str(&first)?;
    if first_val["kind"] == "header" {
        return import_with_header(first_val, lines, true, &mut accept);
    }
    if first_val.get("stream_id").is_some() && first_val.get("seq").is_some() {
        let first_rec: Record = serde_json::from_value(first_val)?;
        if first_rec.seq != 0 {
            return Err(Error::Protocol(format!(
                "expected raw recording to start at seq 0, got {}",
                first_rec.seq
            )));
        }
        let stream_id = first_rec.stream_id;
        let session_id = first_rec.session_id.clone();
        accept(first_rec)?;
        let mut records = 1u64;
        for line in lines {
            let rec: Record = serde_json::from_str(&line?)?;
            if rec.stream_id != stream_id || rec.seq != records {
                return Err(Error::Protocol(format!(
                    "discontinuous raw recording at line {}",
                    records + 1
                )));
            }
            accept(rec)?;
            records += 1;
        }
        return Ok(VoyageSummary {
            stream_id,
            header: json!({
                "kind": "header",
                "format": "kar-voyage/3",
                "recordEncoding": "kar-record/1",
                "streamId": stream_id,
                "sessionId": session_id,
            }),
            end: None,
            records,
        });
    }
    if first_val.get("receivedAt").is_some()
        && first_val.get("seq").is_some()
        && matches!(
            first_val["kind"].as_str(),
            Some("frame" | "request" | "response")
        )
    {
        if first_val["seq"].as_u64() != Some(0) {
            return Err(Error::Protocol("expected upstream record 0".into()));
        }
        let stream_id = Uuid::new_v4();
        let session_id = first_val["sessionId"].as_str().map(str::to_owned);
        let first_rec = convert(&first_val, stream_id, 0)?;
        accept(first_rec)?;
        let mut records = 1u64;
        for line in lines {
            let raw: Value = serde_json::from_str(&line?)?;
            if raw["seq"].as_u64() != Some(records) {
                return Err(Error::Protocol(format!(
                    "expected upstream record {records}"
                )));
            }
            let rec = convert(&raw, stream_id, records)?;
            accept(rec)?;
            records += 1;
        }
        return Ok(VoyageSummary {
            stream_id,
            header: json!({
                "kind": "header",
                "format": "oar-voyage/3",
                "recordEncoding": "oar-record/1",
                "sessionId": session_id,
            }),
            end: None,
            records,
        });
    }
    Err(Error::Protocol(
        "expected a kar-voyage/3 or oar-voyage/3 header or raw Record JSONL".into(),
    ))
}

fn import_with_header<B: BufRead>(
    header: Value,
    lines: std::io::Lines<B>,
    allow_oar: bool,
    accept: &mut impl FnMut(Record) -> Result<()>,
) -> Result<VoyageSummary> {
    if header["kind"] != "header"
        || !(header["format"] == "kar-voyage/3"
            || (allow_oar && header["format"] == "oar-voyage/3"))
    {
        return Err(Error::Protocol(if allow_oar {
            "expected a kar-voyage/3 or oar-voyage/3 recording header".into()
        } else {
            "expected a kar-voyage/3 recording header".into()
        }));
    }
    let encoding = match header.get("recordEncoding") {
        None => RecordEncoding::Upstream,
        Some(enc) if enc == "oar-record/1" => RecordEncoding::OarRecordV1,
        Some(enc) if enc == "kar-record/1" => {
            RecordEncoding::KarRecordV1(serde_json::from_value(header["streamId"].clone())?)
        }
        Some(_) => {
            return Err(Error::Protocol(
                "unrecognized voyage record encoding".into(),
            ));
        }
    };
    let stream_id = match encoding {
        RecordEncoding::KarRecordV1(id) => id,
        RecordEncoding::Upstream | RecordEncoding::OarRecordV1 => header
            .get("streamId")
            .and_then(|v| serde_json::from_value::<Uuid>(v.clone()).ok())
            .unwrap_or_else(Uuid::new_v4),
    };
    let mut summary = VoyageSummary {
        header,
        stream_id,
        end: None,
        records: 0,
    };
    for line in lines {
        let value: Value = serde_json::from_str(&line?)?;
        if summary.end.is_some() {
            return Err(Error::Protocol("data after voyage end".into()));
        }
        match value["kind"].as_str() {
            Some("record") => {
                let rec = &value["record"];
                if rec["seq"].as_u64() != Some(summary.records) {
                    return Err(Error::Protocol(format!(
                        "expected upstream record {}",
                        summary.records
                    )));
                }
                let record = match encoding {
                    RecordEncoding::KarRecordV1(expected_stream) => {
                        let record: Record = serde_json::from_value(rec.clone())?;
                        if record.stream_id != expected_stream {
                            return Err(Error::Protocol(
                                "Kar recording changed stream identity".into(),
                            ));
                        }
                        record
                    }
                    RecordEncoding::OarRecordV1
                        if rec.get("stream_id").is_some() && rec.get("events").is_some() =>
                    {
                        let mut record: Record = serde_json::from_value(rec.clone())?;
                        record.stream_id = summary.stream_id;
                        record
                    }
                    RecordEncoding::Upstream | RecordEncoding::OarRecordV1 => {
                        convert(rec, summary.stream_id, summary.records)?
                    }
                };
                accept(record)?;
                summary.records += 1;
            }
            Some("end") => {
                if matches!(encoding, RecordEncoding::KarRecordV1(_))
                    && value["records"].as_u64() != Some(summary.records)
                {
                    return Err(Error::Protocol(
                        "Kar recording end count does not match".into(),
                    ));
                }
                if let Some(expected) = value.get("records").and_then(Value::as_u64)
                    && expected != summary.records
                {
                    return Err(Error::Protocol(
                        "voyage recording end count does not match".into(),
                    ));
                }
                summary.end = Some(value);
            }
            _ => return Err(Error::Protocol("unknown voyage line".into())),
        }
    }
    Ok(summary)
}

pub(crate) fn convert(v: &Value, stream_id: Uuid, seq: u64) -> Result<Record> {
    let kind: RecordKind = serde_json::from_value(v["kind"].clone())?;
    let body = &v["body"];
    let session_id = v["sessionId"]
        .as_str()
        .ok_or_else(|| Error::Protocol("missing upstream sessionId".into()))?;
    let direction = match (kind, v["direction"].as_str(), body["kind"].as_str()) {
        (RecordKind::Frame, _, _) | (_, Some("toApp"), _) => Direction::FromRuntime,
        (_, Some("toRuntime"), _) | (_, _, Some("answered")) => Direction::ToRuntime,
        _ => Direction::Host,
    };
    let mut events = vec![];
    let mut message_boundaries = std::collections::BTreeMap::new();
    for event in body["events"].as_array().into_iter().flatten() {
        if event["kind"] == "turn_ended"
            && event["outcome"]["kind"] == "completed"
            && let Some(value) = event["outcome"].get("structuredOutput")
        {
            events.push(Event::StructuredOutput {
                value: value.clone(),
            });
        }
        if event["kind"] == "turn_ended"
            && event["outcome"]["kind"] == "failed"
            && (event["outcome"].get("reason").is_some()
                || event["outcome"].get("failure").is_some())
        {
            let class = event["outcome"]
                .get("failure")
                .and_then(|c| serde_json::from_value::<crate::FailureClass>(c.clone()).ok())
                .unwrap_or(crate::FailureClass::Unknown);
            let reason = event["outcome"]["reason"].as_str().unwrap_or("turn failed");
            let mut failure = crate::TurnFailure::new(class, reason);
            failure.status = event["outcome"]["status"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok());
            failure.credential = event["outcome"]
                .get("credential")
                .and_then(|c| serde_json::from_value(c.clone()).ok());
            failure.resets_at_ms = event["outcome"]["resetsAtMs"].as_u64();
            failure.rate_limit_type = event["outcome"]["rateLimitType"]
                .as_str()
                .map(str::to_owned);
            events.push(Event::TurnFailure { failure });
        }
        if event["kind"] == "turn_ended"
            && let Some(anchor) = event["forkAnchor"].as_str()
        {
            events.push(Event::ForkAnchor {
                anchor: anchor.into(),
            });
        }
        if let Some(e) = project(event) {
            if matches!(
                &e,
                Event::TextDelta { .. } | Event::ReasoningDelta { .. } | Event::Message { .. }
            ) && let Some(boundary) = crate::stream::MessageBoundary::from_recorded(event)
            {
                message_boundaries.insert(events.len(), boundary);
            }
            events.push(e)
        }
        if event["kind"] == "usage"
            && let Some(value) = event.pointer("/usage/context").filter(|v| v.is_object())
            && let Ok(usage) = serde_json::from_value(value.clone())
        {
            events.push(Event::ContextUsage { usage });
        }
        project_tool_detail(event, &mut events);
    }
    if kind == RecordKind::Request
        && v["direction"] == "toApp"
        && let Some(id) = v.get("id")
    {
        events.push(Event::ApprovalRequested {
            id: id.clone(),
            method: body["type"].as_str().unwrap_or("native").into(),
        });
    }
    if kind == RecordKind::Response && body["kind"] == "released" {
        events.push(Event::Released);
    }
    if kind == RecordKind::Response && body["kind"] == "exited" {
        events.push(Event::Exited {
            reason: body["reason"].as_str().unwrap_or("runtime exited").into(),
        });
    }
    if kind == RecordKind::Response
        && body["kind"] == "answered"
        && let Some(id) = v.get("requestId")
    {
        events.push(Event::RequestResolved { id: id.clone() });
    }
    Ok(Record {
        stream_id,
        seq,
        timestamp_ms: v["receivedAt"]
            .as_u64()
            .ok_or_else(|| Error::Protocol("missing upstream receivedAt".into()))?,
        direction,
        kind,
        prompt: if kind == RecordKind::Request && body["kind"] == "prompt" {
            body["inputId"]
                .as_str()
                .map(|input_id| crate::PromptIdentity {
                    request_id: v["id"].as_str().map(str::to_owned),
                    input_id: input_id.into(),
                    native_admission: body["admission"] == "native",
                    submission: if body["images"].is_array() {
                        serde_json::from_value(body.clone()).ok()
                    } else {
                        None
                    },
                })
        } else {
            None
        },
        origin: if kind == RecordKind::Request
            && matches!(body["kind"].as_str(), Some("prompt" | "queue" | "steer"))
        {
            body.get("origin")
                .filter(|v| !v.is_null())
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| Error::Protocol(format!("invalid input origin: {e}")))?
        } else {
            None
        },
        carries: if kind == RecordKind::Request && body["kind"] == "prompt" {
            body["carries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        } else {
            Vec::new()
        },
        frame_type: (kind == RecordKind::Frame)
            .then(|| body["type"].as_str().map(str::to_owned))
            .flatten(),
        opening: match (kind, body["kind"].as_str()) {
            (RecordKind::Request, Some("open")) => Some(crate::Opening::Open {
                request_id: v["id"]
                    .as_str()
                    .ok_or_else(|| Error::Protocol("open record has no request ID".into()))?
                    .into(),
                placement: serde_json::from_value(body["placement"].clone())?,
            }),
            (RecordKind::Response, Some("opened")) => Some(crate::Opening::Opened {
                placement: serde_json::from_value(body["placement"].clone())?,
            }),
            _ => None,
        },
        response_to: body["responseTo"]
            .as_str()
            .or_else(|| {
                (kind == RecordKind::Response)
                    .then(|| v["requestId"].as_str())
                    .flatten()
            })
            .map(str::to_owned),
        session_id: Some(session_id.into()),
        span_id: v["spanId"].as_str().map(str::to_owned),
        agent_path: serde_json::from_value(v["agentPath"].clone())?,
        elided: body["elided"] == true,
        elided_input_echoes: vec![],
        message_boundaries,
        native: body.get("native").unwrap_or(body).clone(),
        upstream: Some(v.clone()),
        events,
    })
}

fn legacy_output_parts(output: &str) -> Vec<crate::OutputPart> {
    if let Ok(parsed) = serde_json::from_str::<Value>(output) {
        if let Some(arr) = parsed.as_array()
            && !arr.is_empty()
            && arr.iter().all(|b| b["type"].is_string())
        {
            return arr
                .iter()
                .map(|part| {
                    serde_json::from_value(part.clone()).unwrap_or_else(|_| {
                        crate::OutputPart::Other {
                            value: part.clone(),
                        }
                    })
                })
                .collect();
        }
        if let Some(text) = parsed.as_str() {
            return vec![crate::OutputPart::Text {
                text: text.to_owned(),
            }];
        }
    }
    vec![crate::OutputPart::Text {
        text: output.to_owned(),
    }]
}

fn project_tool_detail(v: &Value, events: &mut Vec<Event>) {
    let Some(id) = v["callId"].as_str() else {
        return;
    };
    match v["kind"].as_str() {
        Some("tool_call_started" | "tool_call_input") => {
            if v["kind"] == "tool_call_started"
                && v["tool"].is_string()
                && (v["server"].is_string() || v["toolName"].is_string())
            {
                events.push(Event::ToolIdentity {
                    mcp_app_uri: None,
                    id: id.into(),
                    native_type: v["tool"].as_str().unwrap().into(),
                    server: v["server"].as_str().map(str::to_owned),
                    tool_name: v["toolName"].as_str().map(str::to_owned),
                });
            }
            if let Some(input) = v.get("arguments").or_else(|| v.get("input")) {
                events.push(Event::ToolInput {
                    id: id.into(),
                    input: input.clone(),
                });
            }
        }
        Some("tool_call_input_delta") => {
            if let Some(delta) = v["delta"].as_str() {
                events.push(Event::ToolInputDelta {
                    id: id.into(),
                    delta: delta.into(),
                });
            }
        }
        Some("tool_call_progress") => {
            let output = v["output"].as_str().map(str::to_owned);
            let delta = v["outputDelta"].as_str().map(str::to_owned);
            if output.is_some() || delta.is_some() {
                events.push(Event::ToolProgress {
                    id: id.into(),
                    output,
                    delta,
                });
            }
        }
        Some("tool_call_ended") => {
            let status = match v["result"].as_str() {
                Some("ok") => Some(crate::ToolStatus::Succeeded),
                Some("failed") => Some(crate::ToolStatus::Failed),
                Some("cancelled") => Some(crate::ToolStatus::Cancelled),
                _ => None,
            };
            let content = v["content"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|part| {
                            serde_json::from_value(part.clone()).unwrap_or_else(|_| {
                                crate::OutputPart::Other {
                                    value: part.clone(),
                                }
                            })
                        })
                        .collect()
                })
                .or_else(|| v["output"].as_str().map(legacy_output_parts));
            let file_changes = v["fileChanges"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| serde_json::from_value(f.clone()).ok())
                .collect();
            let result = crate::ToolResult {
                status,
                exit_code: v["exitCode"].as_i64(),
                content,
                file_changes,
            };
            if result != crate::ToolResult::default() {
                events.push(Event::ToolResult {
                    id: id.into(),
                    result,
                });
            }
        }
        _ => {}
    }
}

fn project(v: &Value) -> Option<Event> {
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    Some(match v["kind"].as_str()? {
        "app_answered" => Event::RequestResolved {
            id: v.get("requestId")?.clone(),
        },
        "input_queued" => Event::InputQueued {
            input_id: text("inputId")?,
        },
        "input_removed" => Event::InputRemoved {
            input_id: text("inputId")?,
        },
        "session_linked" if v["via"] == "tool_call" => Event::SessionLinked {
            parent: text("parent")?,
            child: text("child")?,
        },
        "service_tier" => Event::ServiceTier {
            tier: text("serviceTier")?,
        },
        "mcp_server_startup" => Event::McpServerStartup {
            server: text("server")?,
            status: serde_json::from_value(v.get("status")?.clone()).ok()?,
        },
        "goal_changed" => Event::GoalChanged {
            goal: serde_json::from_value(v.get("goal")?.clone()).ok()?,
        },
        label @ ("task_started" | "task_updated" | "task_ended") => {
            let mut fields = v.clone();
            fields["action"] = label.strip_prefix("task_")?.into();
            let change = serde_json::from_value(fields).ok()?;
            Event::Task {
                id: text("taskId")?,
                change,
            }
        }
        "turn_active" => Event::TurnStarted,
        "model" => Event::Model {
            model: text("model")?,
        },
        "effort" => Event::Effort {
            effort: text("effort")?,
        },
        "input_dropped" => Event::InputDropped {
            input_id: text("inputId")?,
        },
        "retry" => Event::Retry {
            attempt: v["attempt"].as_u64(),
            max_attempts: v["maxAttempts"].as_u64(),
            delay_ms: v["delayMs"].as_u64(),
            reason: text("reason"),
        },
        "compaction_started" => Event::Compaction {
            state: "started".into(),
        },
        "compaction_ended" => Event::Compaction {
            state: text("outcome")?,
        },
        "user_message" => Event::UserMessage {
            text: text("input")?,
        },
        "text_delta" => Event::TextDelta {
            text: text("text")?,
        },
        "reasoning" => {
            let reasoning_text =
                if v.pointer("/content/kind").and_then(Value::as_str) == Some("text") {
                    v.pointer("/content/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                } else {
                    text("text")
                };
            Event::ReasoningDelta {
                text: reasoning_text?,
            }
        }
        "turn_diff" => Event::TurnDiff {
            turn_id: text("turnId")?,
            diff: text("diff")?,
        },
        "tool_call_started" => Event::ToolStarted {
            id: text("callId")?,
            name: text("tool")?,
        },
        "tool_call_progress" => Event::ToolUpdated {
            id: text("callId")?,
        },
        "tool_call_ended" => Event::ToolEnded {
            id: text("callId")?,
        },
        "usage" => Event::Usage {
            usage: v.get("usage")?.clone(),
        },
        "turn_started" => Event::TurnStarted,
        "turn_ended" => Event::TurnEnded {
            outcome: match v.pointer("/outcome/kind")?.as_str()? {
                "completed" => Outcome::Completed,
                "aborted" => Outcome::Aborted,
                "failed" => Outcome::Failed,
                _ => return None,
            },
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn imported_opening_links_the_actual_placement_and_legacy_keeps_its_first_frame() {
        let stream = Uuid::new_v4();
        let record = |kind: &str, body: Value| json!({"kind":kind,"sessionId":"s","receivedAt":1,"agentPath":[],"body":body});
        let mut first = record("request", json!({"kind":"open","placement":"auto"}));
        first["direction"] = "toRuntime".into();
        first["id"] = "opaque-open".into();
        let mut second = record("response", json!({"kind":"opened","placement":"shared"}));
        second["requestId"] = "opaque-open".into();
        let pair = [
            convert(&first, stream, 0).unwrap(),
            convert(&second, stream, 1).unwrap(),
        ];
        assert_eq!(
            crate::SessionPlacement::from_records(&pair),
            Some(crate::SessionPlacement::Shared)
        );
        assert_eq!(pair[0].control_request_id(), pair[1].response_to.as_deref());
        let legacy = convert(
            &record("frame", json!({"type":"ready","native":{},"events":[]})),
            stream,
            0,
        )
        .unwrap();
        assert_eq!(crate::SessionPlacement::from_records([&legacy]), None);
        assert!(legacy.opening.is_none());
    }

    #[test]
    fn imported_tool_result_and_pending_request_replay() {
        let header =
            json!({"kind":"header","format":"kar-voyage/3","sessionId":"s","runtime":"codex"});
        let bodies = [
            json!({"kind":"request","id":"ask","direction":"toApp","body":{"kind":"native","type":"item/tool/requestUserInput","native":{"questions":[{"id":"q"}]}}}),
            json!({"kind":"response","requestId":"ask","body":{"kind":"answered","native":{}}}),
            json!({"kind":"frame","body":{"events":[
                {"kind":"tool_call_started","callId":"c","tool":"shell","input":"exit 7"},
                {"kind":"tool_call_progress","callId":"c","output":"x","outputDelta":"y"},
                {"kind":"tool_call_started","callId":"nested","tool":"leaf","arguments":{"line":4}},
                {"kind":"tool_call_ended","callId":"c","result":"failed","exitCode":7,"content":[{"type":"text","text":"done"}],
                 "fileChanges":[{"path":"a","kind":"modified","moveTo":"b"}]},
                {"kind":"turn_ended","outcome":{"kind":"completed","structuredOutput":null}}
            ]}}),
        ];
        let mut source = format!("{header}\n");
        for (seq, mut r) in bodies.into_iter().enumerate() {
            r["seq"] = seq.into();
            r["sessionId"] = "s".into();
            r["agentPath"] = json!([]);
            r["receivedAt"] = 1.into();
            source.push_str(&format!("{}\n", json!({"kind":"record","record":r})));
        }
        let mut view = crate::observe::SessionView::default();
        let mut state = crate::observe::SessionReducer::new("s");
        import(source.as_bytes(), |r| {
            state.apply(&r)?;
            if r.seq == 0 {
                assert_eq!(
                    state.snapshot().pending_requests[0].kind,
                    crate::RequestKind::Question
                );
            }
            view.apply(&r)
        })
        .unwrap();
        assert!(state.snapshot().pending_requests.is_empty());
        let agent = view.agents.values().next().unwrap();
        assert_eq!(agent.last_structured_output, Some(Value::Null));
        assert_eq!(state.snapshot().last_structured_output, Some(Value::Null));
        assert_eq!(agent.tools["nested"].input, Some(json!({"line":4})));
        let tool = &agent.tools["c"];
        assert_eq!(tool.preview, "xy");
        let result = tool.result.as_ref().unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.file_changes[0].move_to.as_deref(), Some("b"));
    }

    #[test]
    fn preserves_attribution_unknown_native_data_and_interrupted_captures() {
        let header =
            json!({"kind":"header","format":"kar-voyage/3","sessionId":"root","runtime":"codex"});
        let native = json!({"kind":"frame","seq":0,"receivedAt":812,"sessionId":"child","agentPath":["tool-1"],"spanId":"turn-9","body":{"type":"custom","responseTo":"opaque-attempt","native":{"future":"field"},"events":[{"kind":"text_delta","text":"hello"},{"kind":"future_event"}]}});
        let row = json!({"kind":"record","record":native});
        let data = format!("{header}\n{row}\n");
        let mut converted = vec![];
        let summary = import(data.as_bytes(), |r| {
            converted.push(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.header, header);
        assert!(summary.end.is_none());
        let record = &converted[0];
        assert_eq!(record.upstream.as_ref(), Some(&native));
        assert_eq!(record.response_to.as_deref(), Some("opaque-attempt"));
        assert_eq!(record.agent_path, ["tool-1"]);
        assert_eq!(record.session_id.as_deref(), Some("child"));
        assert_eq!(
            record.events,
            [Event::TextDelta {
                text: "hello".into()
            }]
        );
        assert!(import(format!("{data}{row}").as_bytes(), |_| Ok(())).is_err());
        let end = json!({"kind":"end","at":850,"reason":"closed"});
        let summary = import(format!("{data}{end}").as_bytes(), |_| Ok(())).unwrap();
        assert_eq!(summary.end, Some(end.clone()));
        assert_eq!(summary.records, 1);
        assert!(import(format!("{data}{end}\n{row}").as_bytes(), |_| Ok(())).is_err());
        for format in ["kar-voyage/2", "kar-voyage/4", "oar-voyage/2"] {
            let mut invalid = header.clone();
            invalid["format"] = format.into();
            assert!(matches!(
                import(invalid.to_string().as_bytes(), |_| Ok(())),
                Err(Error::Protocol(_))
            ));
        }
    }

    #[test]
    fn imports_oar_voyage_v3_headers_reasoning_legacy_tools_and_failures() {
        for encoding in [None, Some("oar-record/1")] {
            let mut header = json!({
                "kind": "header",
                "format": "oar-voyage/3",
                "sessionId": "oar-root",
                "runtime": "dsh",
                "cwd": "/tmp/work",
                "startedAt": 100,
                "recorder": "oar-harbor"
            });
            if let Some(enc) = encoding {
                header["recordEncoding"] = enc.into();
            }
            let frame0 = json!({
                "kind": "frame",
                "seq": 0,
                "receivedAt": 110,
                "sessionId": "oar-root",
                "agentPath": [],
                "body": {
                    "type": "agent/assistant-stream",
                    "events": [
                        {"kind": "reasoning", "content": {"kind": "text", "text": "thinking..."}, "messageId": "m1"},
                        {"kind": "reasoning", "content": {"kind": "empty"}, "messageId": "m1"},
                        {"kind": "text_delta", "text": "answer", "messageId": "m1", "phase": "final_answer"},
                        {"kind": "tool_call_started", "callId": "t1", "tool": "bash"},
                        {"kind": "tool_call_ended", "callId": "t1", "result": "ok", "output": "legacy-out"}
                    ]
                }
            });
            let frame1 = json!({
                "kind": "frame",
                "seq": 1,
                "receivedAt": 120,
                "sessionId": "oar-root",
                "agentPath": [],
                "body": {
                    "type": "session/event:turn/end",
                    "events": [
                        {"kind": "turn_ended", "outcome": {"kind": "failed", "failure": "auth", "reason": "invalid key", "status": 401, "credential": "rejected"}}
                    ]
                }
            });
            let end = json!({"kind": "end", "at": 130, "reason": "down"});
            let payload = format!(
                "{header}\n{}\n{}\n{end}\n",
                json!({"kind": "record", "record": frame0}),
                json!({"kind": "record", "record": frame1}),
            );
            let mut records = Vec::new();
            let summary = import_records_or_voyage(payload.as_bytes(), |r| {
                records.push(r);
                Ok(())
            })
            .unwrap();
            assert_eq!(summary.records, 2);
            assert_eq!(summary.end, Some(end));
            assert_eq!(
                records[0].events[0],
                Event::ReasoningDelta {
                    text: "thinking...".into()
                }
            );
            assert_eq!(
                records[0]
                    .message_boundaries
                    .get(&0)
                    .and_then(|b| b.message_id.as_deref()),
                Some("m1")
            );
            assert_eq!(
                records[0]
                    .message_boundaries
                    .get(&1)
                    .and_then(|b| b.phase.as_deref()),
                Some("final_answer")
            );
            assert!(records[0].events.iter().any(|e| matches!(
                e,
                Event::ToolResult { id, result }
                    if id == "t1"
                    && result.content == Some(vec![crate::OutputPart::Text { text: "legacy-out".into() }])
            )));
            let failure = records[1].turn_failure().expect("projected failure");
            assert_eq!(failure.class, crate::FailureClass::Auth);
            assert_eq!(failure.reason, "invalid key");
            assert_eq!(failure.status, Some(401));
            assert_eq!(failure.credential, Some(crate::CredentialProblem::Rejected));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn voyage_recorder_and_headless_record_import_survive_unclosed_and_closed_streams() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::stream::Journal::new(None).unwrap();
        journal.bind_root("sess-1");
        journal.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            Some("sess-1".into()),
            vec![],
            json!({"type": "hello"}),
            vec![
                Event::TurnStarted,
                Event::TextDelta {
                    text: "chunk-1".into(),
                },
                Event::TurnEnded {
                    outcome: Outcome::Completed,
                },
            ],
        );
        let captured = journal.records();

        // 1. Crash-safe unclosed VoyageRecorder (simulates panic / SIGKILL before finish()).
        let unclosed_path = dir.path().join("crash.voyage.jsonl");
        let header = VoyageHeader::new("codex", dir.path(), "sess-1", journal.id())
            .with_model(Some("gpt-5".into()));
        {
            let mut rec = VoyageRecorder::create_new(&unclosed_path, &header).unwrap();
            for item in &captured {
                rec.record(item).unwrap();
            }
            assert_eq!(rec.records_written(), 1);
            // Drop without calling finish()!
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&unclosed_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let mut replayed = Vec::new();
        let summary = import_records_or_voyage(
            std::io::BufReader::new(File::open(&unclosed_path).unwrap()),
            |r| {
                replayed.push(r);
                Ok(())
            },
        )
        .unwrap();
        assert!(summary.end.is_none());
        assert_eq!(summary.records, 1);
        assert_eq!(replayed[0].events, captured[0].events);

        // 2. Cleanly finished VoyageRecorder.
        let closed_path = dir.path().join("clean.voyage.jsonl");
        let mut rec = VoyageRecorder::create(&closed_path, &header).unwrap();
        for item in &captured {
            rec.record(item).unwrap();
        }
        assert_eq!(rec.finish_with_reason("completed").unwrap(), 1);
        let finished = import(
            std::io::BufReader::new(File::open(&closed_path).unwrap()),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            finished.end.as_ref().and_then(|e| e["reason"].as_str()),
            Some("completed")
        );
        assert_eq!(finished.records, 1);

        // 3. Headless raw `Record` JSONL (legacy `kar run --record` format).
        let raw_path = dir.path().join("headless.jsonl");
        {
            let mut raw = File::create(&raw_path).unwrap();
            for item in &captured {
                serde_json::to_writer(&mut raw, item.as_ref()).unwrap();
                raw.write_all(b"\n").unwrap();
            }
        }
        let mut from_raw = Vec::new();
        let raw_summary = import_records_or_voyage(
            std::io::BufReader::new(File::open(&raw_path).unwrap()),
            |r| {
                from_raw.push(r);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(raw_summary.stream_id, journal.id());
        assert_eq!(raw_summary.records, 1);
        assert_eq!(from_raw.len(), 1);
    }
}
