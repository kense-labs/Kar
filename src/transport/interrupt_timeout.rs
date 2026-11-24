//! A native interrupt can be accepted while its turn remains stuck. Keep the
//! deadline until a terminal frame, even after the RPC acknowledgement.
use std::{collections::BTreeSet, time::Duration};
use tokio::time::Instant;

#[derive(Default)]
pub(super) struct InterruptTimeout {
    attempts: BTreeSet<u64>,
    deadline: Option<Instant>,
}

impl InterruptTimeout {
    pub(super) fn arm(&mut self, id: u64) {
        self.attempts.insert(id);
        self.deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(10));
    }
    pub(super) fn refused(&mut self, id: u64) {
        self.attempts.remove(&id);
        if self.attempts.is_empty() {
            self.deadline = None;
        }
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub(super) fn take(&mut self) -> BTreeSet<u64> {
        self.deadline = None;
        std::mem::take(&mut self.attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn later_requests_cannot_postpone_the_deadline() {
        let mut t = InterruptTimeout::default();
        t.arm(1);
        let due = t.deadline().unwrap();
        tokio::time::advance(Duration::from_secs(8)).await;
        t.arm(2);
        t.refused(1);
        assert_eq!(t.deadline(), Some(due));
        assert_eq!(t.take(), BTreeSet::from([2]));
        assert_eq!(t.deadline(), None);
        t.arm(3);
        assert!(t.deadline().unwrap() > due);
        t.refused(3);
        assert_eq!(t.deadline(), None);
    }
}
