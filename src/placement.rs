//! A runtime's requested placement and the placement observed for one session.
use crate::{Direction, Error, Record, RecordKind, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    Process,
    InProcess,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementRequest {
    #[default]
    Auto,
    Shared,
    Isolated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlacement {
    Isolated,
    Shared,
    InProcess,
}

impl PlacementRequest {
    pub(crate) fn resolve(self, execution: Option<Execution>) -> Result<Option<SessionPlacement>> {
        match (execution, self) {
            (Some(Execution::Process), Self::Auto | Self::Isolated) => {
                Ok(Some(SessionPlacement::Isolated))
            }
            (Some(Execution::InProcess), Self::Auto) => Ok(Some(SessionPlacement::InProcess)),
            (None, Self::Auto) => Ok(None),
            (Some(Execution::Process), Self::Shared) => Err(Error::Unsupported(
                "placement: this runtime has no verified shared process implementation".into(),
            )),
            (Some(Execution::InProcess), _) => Err(Error::Unsupported(
                "placement: this runtime executes in the host process".into(),
            )),
            (None, _) => Err(Error::Unsupported(
                "placement: this runtime does not control native process placement".into(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Opening {
    Open {
        request_id: String,
        placement: PlacementRequest,
    },
    Opened {
        placement: SessionPlacement,
    },
}

impl SessionPlacement {
    /// Read the paired opening records of a live or recorded stream. Older
    /// recordings and adapters without an execution declaration have no pair.
    pub fn from_records<'a>(records: impl IntoIterator<Item = &'a Record>) -> Option<Self> {
        let mut records = records.into_iter();
        Self::from_pair(records.next()?, records.next()?)
    }

    pub(crate) fn from_pair(first: &Record, second: &Record) -> Option<Self> {
        let Opening::Open { request_id, .. } = first.opening.as_ref()? else {
            return None;
        };
        let Opening::Opened { placement } = second.opening.as_ref()? else {
            return None;
        };
        (!request_id.is_empty()
            && first.agent_path.is_empty()
            && second.agent_path.is_empty()
            && first.seq == 0
            && second.seq == 1
            && first.stream_id == second.stream_id
            && first.kind == RecordKind::Request
            && first.direction == Direction::ToRuntime
            && second.kind == RecordKind::Response
            && second.response_to.as_deref() == Some(request_id)
            && first.session_id == second.session_id)
            .then_some(*placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Journal;
    #[test]
    fn paired_opening_is_distinct_from_legacy_or_unrelated_records() {
        let old = Journal::new(None).unwrap();
        old.push(
            Direction::FromRuntime,
            RecordKind::Frame,
            None,
            vec![],
            serde_json::json!({"type":"ready"}),
            vec![],
        );
        assert_eq!(old.placement(), None);
        let opened =
            Journal::for_open(Some(1), PlacementRequest::Auto, Some(Execution::InProcess)).unwrap();
        opened.bind_root("original");
        assert_eq!(opened.placement(), Some(SessionPlacement::InProcess));
        let records = opened.records();
        let mut wrong = (*records[1]).clone();
        wrong.response_to = Some("unrelated".into());
        assert_eq!(
            SessionPlacement::from_records([records[0].as_ref(), &wrong]),
            None
        );
        assert!(
            PlacementRequest::Isolated
                .resolve(Some(Execution::InProcess))
                .is_err()
        );
        assert!(PlacementRequest::Shared.resolve(None).is_err());
        assert_eq!(PlacementRequest::Auto.resolve(None).unwrap(), None);
    }
}

#[cfg(test)]
mod acp_admission_tests {
    use super::*;

    #[tokio::test]
    async fn production_shared_refusals_precede_open_resume_and_fork() {
        let dir = tempfile::tempdir().unwrap();
        for (name, reason) in [
            ("opencode", "configured plugins"),
            ("grok", "configured stdio MCP servers"),
        ] {
            let mut rt = crate::runtime(name)
                .unwrap()
                .with_placement(PlacementRequest::Shared);
            rt.program = dir.path().join("never-started");
            let opts = crate::SessionOptions::new(dir.path());
            let mut resume = opts.clone();
            resume.resume = Some("previous".into());
            for result in [
                rt.open(opts.clone()).await,
                rt.open(resume).await,
                rt.fork("parent", None, opts).await,
            ] {
                assert!(
                    matches!(result, Err(Error::Unsupported(detail)) if detail.contains("placement") && detail.contains(reason))
                );
            }
        }
    }
}

/// A controller released its lease. The native task may still be running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Dispose,
    AbortFallback,
    ConnectionLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRelease {
    pub reason: ReleaseReason,
    #[serde(alias = "nativeTurnMayContinue")]
    pub native_turn_may_continue: bool,
    #[serde(alias = "workStartedAfterCleanupMayContinue")]
    pub work_started_after_cleanup_may_continue: bool,
}
impl SessionRelease {
    pub(crate) fn new(reason: ReleaseReason) -> Self {
        Self {
            reason,
            native_turn_may_continue: true,
            work_started_after_cleanup_may_continue: true,
        }
    }
}
impl crate::Record {
    pub fn session_release(&self) -> Option<SessionRelease> {
        if !self.events.contains(&crate::Event::Released) {
            return None;
        }
        let native = self.native.get("body").unwrap_or(&self.native);
        serde_json::from_value(native.get("release").unwrap_or(native).clone()).ok()
    }
}
