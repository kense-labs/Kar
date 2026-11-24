//! Extract a bounded text companion for a document attached to a chat.
mod package;
mod rich_text;
use anyhow::{Result, ensure};
use std::{
    path::Path,
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

const MAX_UNITS: usize = 1_000_000;
#[derive(Debug, thiserror::Error)]
#[error("document extraction limit")]
struct Truncated;

struct Text {
    value: String,
    remaining: usize,
    deadline: Instant,
}
impl Text {
    fn check(&self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "document text extraction timed out"
        );
        Ok(())
    }
    fn push(&mut self, s: &str) -> Result<()> {
        self.check()?;
        for ch in s.chars() {
            if self.remaining < ch.len_utf16() {
                return Err(Truncated.into());
            }
            self.remaining -= ch.len_utf16();
            if ch != '\0' {
                self.value.push(ch);
            }
        }
        Ok(())
    }
}

static SLOTS: (Mutex<u8>, Condvar) = (Mutex::new(0), Condvar::new());
struct Slot;
impl Slot {
    fn enter() -> Self {
        let mut used = SLOTS
            .1
            .wait_while(SLOTS.0.lock().unwrap(), |n| *n >= 2)
            .unwrap();
        *used += 1;
        Self
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        *SLOTS.0.lock().unwrap() -= 1;
        SLOTS.1.notify_one();
    }
}

/// Returns no companion for unsupported files, invalid content, or an expired parse.
pub fn document_text(name: &str, bytes: &[u8]) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    if bytes.len() > 16 * 1024 * 1024
        || !matches!(
            ext.as_str(),
            "docx" | "pptx" | "xlsx" | "odt" | "ods" | "odp" | "epub" | "ipynb" | "rtf"
        )
    {
        return None;
    }
    let _slot = Slot::enter();
    let mut out = Text {
        value: String::new(),
        remaining: MAX_UNITS,
        deadline: Instant::now() + Duration::from_secs(15),
    };
    let result = match ext.as_str() {
        "ipynb" => rich_text::notebook(bytes, &mut out),
        "rtf" => rich_text::rtf(bytes, &mut out),
        _ => package::extract(&ext, bytes, &mut out),
    };
    out.check().ok()?;
    let truncated = match result {
        Ok(()) => false,
        Err(e) if e.is::<Truncated>() => true,
        Err(_) => return None,
    };
    let mut cleaned = String::new();
    let mut blank = false;
    for line in out.value.trim().lines() {
        if !line.trim().is_empty() || !blank {
            cleaned.push_str(line.trim_end());
            cleaned.push('\n');
        }
        blank = line.trim().is_empty();
    }
    cleaned.truncate(cleaned.trim_end().len());
    if truncated {
        cleaned.push_str("\n\n[Extracted text truncated at document extraction limits.]");
    }
    (!cleaned.is_empty()).then_some(cleaned)
}
