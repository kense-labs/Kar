//! Parse one JSONL record at a time. Unselected tool arguments and binary image data are streamed
//! through the JSON parser without materializing their bodies. A malformed record ends at its LF.
use serde_json::{Map, Number, Value};
use std::{
    cell::Cell,
    io::{self, BufRead, Read},
};
use struson::reader::{JsonReader, JsonStreamReader, ReaderSettings, ValueType};

const MAX_HISTORY: usize = 32 * 1024 * 1024;

struct Line<'a, R> {
    source: &'a mut R,
    ended: bool,
    bytes: u64,
    available: u64,
    key_allowance: &'a Cell<Option<usize>>,
    limit_hit: &'a Cell<bool>,
}
impl<R: BufRead> Read for Line<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.ended || out.is_empty() {
            return Ok(0);
        }
        let buf = self.source.fill_buf()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let len = out
            .len()
            .min(buf.len())
            .min(self.available.min(usize::MAX as u64) as usize);
        let len = self.key_allowance.get().map_or(len, |left| left.min(len));
        if len == 0 {
            self.limit_hit.set(true);
            return Err(io::Error::other("transcript parser budget exceeded"));
        }
        let n = match buf[..len].iter().position(|b| *b == b'\n') {
            Some(end) => {
                self.ended = true;
                end + 1
            }
            None => len,
        };
        out[..n].copy_from_slice(&buf[..n]);
        self.source.consume(n);
        self.bytes += n as u64;
        self.available -= n as u64;
        if let Some(left) = self.key_allowance.get() {
            self.key_allowance.set(Some(left - n))
        }
        Ok(n)
    }
}

#[derive(Copy, Clone)]
enum Field {
    Root,
    Payload,
    Message,
    Content,
    Block,
    Metadata,
    Base,
    Source,
    Scalar,
    Present,
}
impl Field {
    fn child(self, name: &str) -> Option<Self> {
        use Field::*;
        Some(match (self, name) {
            (Root, "payload") => Payload,
            (Root, "message") => Message,
            (
                Root,
                "id" | "parentId" | "type" | "timestamp" | "cwd" | "sessionId" | "isSidechain"
                | "isMeta" | "isCompactSummary" | "aiTitle" | "customTitle" | "uuid" | "name",
            ) => Scalar,
            (Payload, "id" | "session_id" | "cwd" | "type" | "role" | "message" | "model") => {
                Scalar
            }
            (Payload, "history_base") => Base,
            (Payload, "source") => Source,
            (Payload, "internal_chat_message_metadata_passthrough") => Metadata,
            (Payload | Message, "content") => Content,
            (Message, "role" | "model") => Scalar,
            (Metadata, "turn_id") => Scalar,
            (Base, "thread_id" | "end_byte_offset") => Scalar,
            (Source, "subagent" | "subAgent") => Present,
            (Block, "type" | "text") => Scalar,
            _ => return None,
        })
    }
}
struct Budget<'a> {
    retained: usize,
    cap: usize,
    key: &'a Cell<Option<usize>>,
    limit_hit: &'a Cell<bool>,
}
impl Budget<'_> {
    fn charge(&mut self, n: usize) -> Option<()> {
        self.retained = self.retained.checked_add(n)?;
        if self.retained > self.cap {
            self.limit_hit.set(true);
            return None;
        }
        Some(())
    }
    fn value(&mut self, r: &mut impl JsonReader, field: Field) -> Option<Value> {
        self.charge(64)?;
        Some(match (r.peek().ok()?, field) {
            (_, Field::Present) => {
                r.skip_value().ok()?;
                Value::Bool(true)
            }
            (
                ValueType::Object,
                Field::Root
                | Field::Message
                | Field::Payload
                | Field::Block
                | Field::Metadata
                | Field::Base
                | Field::Source,
            ) => {
                let mut result = Map::new();
                r.begin_object().ok()?;
                while r.has_next().ok()? {
                    self.key
                        .set(Some(self.cap.checked_sub(self.retained)? + 8192));
                    let key = r.next_name().ok()?;
                    self.key.set(None);
                    self.charge(key.len())?;
                    if let Some(child) = field.child(key) {
                        let name = key.to_owned();
                        result.insert(name, self.value(r, child)?);
                    } else {
                        r.skip_value().ok()?;
                    }
                }
                r.end_object().ok()?;
                Value::Object(result)
            }
            (ValueType::Array, Field::Content) => {
                let mut items = Vec::new();
                r.begin_array().ok()?;
                while r.has_next().ok()? {
                    items.push(self.value(r, Field::Block)?);
                }
                r.end_array().ok()?;
                Value::Array(items)
            }
            (ValueType::String, Field::Content | Field::Metadata | Field::Scalar) => {
                let left = self.cap - self.retained;
                let mut bytes = Vec::new();
                r.next_string_reader()
                    .ok()?
                    .take(left as u64 + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                self.charge(bytes.len())?;
                Value::String(String::from_utf8(bytes).ok()?)
            }
            (ValueType::Boolean, Field::Scalar) => Value::Bool(r.next_bool().ok()?),
            (ValueType::Number, Field::Scalar) => {
                let n = r.next_number_as_str().ok()?.parse::<Number>().ok()?;
                Value::Number(n)
            }
            _ => {
                r.skip_value().ok()?;
                Value::Null
            }
        })
    }
}
/// EOF returns zero bytes, a malformed record returns a missing value. Resource exhaustion
/// aborts this transcript so that a partial projection cannot be certified as a successful import.
pub(super) fn record(
    reader: &mut impl BufRead,
    remaining: &mut u64,
    history_bytes: usize,
) -> io::Result<(u64, Option<Value>)> {
    let key = Cell::new(None);
    let limit_hit = Cell::new(false);
    let mut line = Line {
        source: reader,
        ended: false,
        bytes: 0,
        available: *remaining,
        key_allowance: &key,
        limit_hit: &limit_hit,
    };
    let opts = ReaderSettings {
        track_path: false,
        max_nesting_depth: Some(128),
        ..Default::default()
    };
    let mut stream = JsonStreamReader::new_custom(&mut line, opts);
    let value = Budget {
        retained: 0,
        cap: MAX_HISTORY.saturating_sub(history_bytes),
        key: &key,
        limit_hit: &limit_hit,
    }
    .value(&mut stream, Field::Root);
    let value = if value.is_some() && stream.consume_trailing_whitespace().is_ok() {
        value
    } else {
        None
    };
    key.set(None);
    if limit_hit.get() {
        *remaining = remaining.saturating_sub(line.bytes);
        return Err(io::Error::other("native history exceeds import limits"));
    }
    io::copy(&mut line, &mut io::sink())?;
    *remaining -= line.bytes;
    Ok((line.bytes, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    #[test]
    fn skips_large_unselected_fields_then_reads_text_after_them() {
        let start =
            br#"{"type":"assistant","message":{"content":[{"type":"image","source":{"data":""#;
        let end = br#""}},{"type":"text","text":"after the image"}]}}
{"type":"user","message":{"content":"next record"}}
"#;
        let input = (&start[..])
            .chain(io::repeat(b'A').take(36 * 1024 * 1024))
            .chain(&end[..]);
        let mut input = BufReader::new(input);
        let mut remaining = 40 * 1024 * 1024;
        let (n, row) = record(&mut input, &mut remaining, 0).unwrap();
        assert!(n > 32 * 1024 * 1024);
        let row = row.unwrap();
        assert!(row["message"]["content"][0].get("source").is_none());
        assert_eq!(row["message"]["content"][1]["text"], "after the image");
        assert_eq!(
            record(&mut input, &mut remaining, 0).unwrap().1.unwrap()["message"]["content"],
            "next record"
        );
    }
    #[test]
    fn visible_text_that_exceeds_the_budget_is_not_certified() {
        let mut input = &br#"{"message":{"content":"text beyond the available budget"}}"#[..];
        assert!(record(&mut input, &mut 1_000, MAX_HISTORY - 128).is_err());
    }
    #[test]
    fn malformed_records_cannot_consume_the_following_record() {
        let mut input = &b"{broken\n{\"type\":\"ok\"}\n"[..];
        let mut remaining = 1000;
        assert!(record(&mut input, &mut remaining, 0).unwrap().1.is_none());
        assert_eq!(
            record(&mut input, &mut remaining, 0).unwrap().1.unwrap()["type"],
            "ok"
        );
    }
}
