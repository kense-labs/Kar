//! Public input shapes shared by the per-forge review adapters.
use super::*;
use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Verdict {
    Comment,
    Approve,
    RequestChanges,
}
impl Verdict {
    pub(super) fn event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Side {
    Left,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum Position {
    Added {
        new_line: NonZeroU32,
    },
    Deleted {
        old_line: NonZeroU32,
    },
    Context {
        old_line: NonZeroU32,
        new_line: NonZeroU32,
        side: Side,
    },
}
impl Position {
    pub(super) fn point(self) -> (Side, u32) {
        match self {
            Self::Added { new_line }
            | Self::Context {
                side: Side::Right,
                new_line,
                ..
            } => (Side::Right, new_line.get()),
            Self::Deleted { old_line }
            | Self::Context {
                side: Side::Left,
                old_line,
                ..
            } => (Side::Left, old_line.get()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Comment {
    pub(super) path: String,
    pub(super) old_path: Option<String>,
    pub(super) position: Position,
    pub(super) start_position: Option<Position>,
    pub(super) body: String,
}
pub(super) fn capabilities(kind: &str) -> Value {
    let writes = !matches!(kind, "azure-devops" | "linear");
    json!({"submit":writes,"inline":writes && kind!="gitcafe","range":matches!(kind,"github"|"gitlab"|"tanstack-source"),"reply":writes && kind!="forgejo","resolve":writes && kind!="forgejo"})
}
pub(super) fn validate(op: &Operation) -> Result<(), ForgeError> {
    let id =
        |s: &str| !s.trim().is_empty() && s.trim() == s && s.len() <= 4096 && s != "." && s != "..";
    let body = |s: &str| !s.trim().is_empty() && s.encode_utf16().count() <= 65536;
    let ok = match op {
        Operation::SubmitReview {
            number,
            body: summary,
            verdict,
            comments,
        } => {
            *number > 0
                && summary.encode_utf16().count() <= 65536
                && (*verdict == Verdict::Approve
                    || !comments.is_empty()
                    || !summary.trim().is_empty())
                && comments.iter().all(|c| {
                    body(&c.body)
                        && !c.path.trim().is_empty()
                        && c.old_path.as_deref().is_none_or(|s| !s.trim().is_empty())
                })
        }
        Operation::ReplyToThread {
            number,
            thread_id,
            body: text,
        } => *number > 0 && id(thread_id) && body(text),
        Operation::SetThreadResolution {
            number, thread_id, ..
        } => *number > 0 && id(thread_id),
        Operation::ReviewThreads { number, cursor } => {
            *number > 0 && cursor.as_deref().is_none_or(id)
        }
        Operation::ThreadComments {
            number,
            thread_id,
            cursor,
        } => *number > 0 && id(thread_id) && id(cursor),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(ForgeError::bad(
            "invalid review, line comment, thread ID or page cursor",
        ))
    }
}
