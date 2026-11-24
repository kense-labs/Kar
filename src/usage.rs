//! Running token counts are scoped to one native open. A derived session owns a separate ledger.
use crate::{
    Cursor, Event, Record,
    observe::{SessionEdge, SessionGraph},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input: i64,
    pub output: i64,
    #[serde(default, alias = "cacheRead", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<i64>,
    #[serde(default, alias = "cacheWrite", skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<i64>,
}
impl TokenTotals {
    pub(crate) fn plus(self, other: Self) -> Self {
        let part = |a: Option<i64>, b: Option<i64>| {
            a.or(b)
                .map(|_| a.unwrap_or(0).saturating_add(b.unwrap_or(0)))
        };
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cache_read: part(self.cache_read, other.cache_read),
            cache_write: part(self.cache_write, other.cache_write),
            reasoning: part(self.reasoning, other.reasoning),
        }
    }
    pub(crate) fn minus(self, base: Self) -> Self {
        let part = |n: Option<i64>, b: Option<i64>| n.map(|v| v.saturating_sub(b.unwrap_or(0)));
        Self {
            input: self.input.saturating_sub(base.input),
            output: self.output.saturating_sub(base.output),
            cache_read: part(self.cache_read, base.cache_read),
            cache_write: part(self.cache_write, base.cache_write),
            reasoning: part(self.reasoning, base.reasoning),
        }
    }
}

/// Cache creation counts by native lifetime, already included in input and cache-write tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWriteByTtl {
    #[serde(rename = "5m", default, skip_serializing_if = "Option::is_none")]
    pub five_minutes: Option<i64>,
    #[serde(rename = "1h", default, skip_serializing_if = "Option::is_none")]
    pub one_hour: Option<i64>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenReading {
    #[serde(
        default,
        alias = "cacheWriteByTtl",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write_by_ttl: Option<CacheWriteByTtl>,
    /// Latest running total for the record's agent path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenTotals>,
    /// Native session-wide total, including unattributed spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<TokenTotals>,
}
impl TokenReading {
    /// Read current Rust events and normalized usage records from upstream Voyages.
    pub fn from_event(event: &Event) -> Option<Self> {
        match event {
            Event::TokenUsage { reading } => Some(*reading),
            Event::Usage { usage } => {
                let reading = Self {
                    tokens: usage
                        .get("tokens")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    total: usage
                        .get("total")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    cache_write_by_ttl: usage
                        .get("cacheWriteByTtl")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                };
                (reading.tokens.is_some()
                    || reading.total.is_some()
                    || reading.cache_write_by_ttl.is_some())
                .then_some(reading)
            }
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTokens {
    pub agent_path: Vec<String>,
    pub tokens: TokenTotals,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub total: Option<TokenTotals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_agent: Option<Vec<AgentTokens>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed: Option<TokenTotals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_children: Option<TokenTotals>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub cursor: Cursor,
    pub usage: SessionUsage,
    /// Each reachable child session's own figures, excluding its descendants.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub children: BTreeMap<String, SessionUsage>,
}
#[derive(Clone, Debug, Default, PartialEq)]
struct SessionTokens {
    agents: Vec<AgentTokens>,
    reported: Option<TokenTotals>,
}
impl SessionTokens {
    fn attributed(&self) -> TokenTotals {
        self.agents
            .iter()
            .map(|agent| agent.tokens)
            .fold(TokenTotals::default(), TokenTotals::plus)
    }
    fn total(&self) -> Option<TokenTotals> {
        self.reported
            .or_else(|| (!self.agents.is_empty()).then(|| self.attributed()))
    }
    fn usage(&self) -> SessionUsage {
        let unattributed = self
            .reported
            .map(|v| v.minus(self.attributed()))
            .filter(|v| v.input > 0 || v.output > 0);
        let breakdown =
            unattributed.is_some() || self.agents.iter().any(|a| !a.agent_path.is_empty());
        SessionUsage {
            total: self.total(),
            by_agent: breakdown.then(|| self.agents.clone()),
            unattributed,
            with_children: None,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Ledger {
    sessions: BTreeMap<String, SessionTokens>,
}
impl Ledger {
    pub(crate) fn observe(
        &mut self,
        root: &str,
        graph: &mut SessionGraph,
        r: &Record,
    ) -> Option<UsageSnapshot> {
        let source = r.session_id.as_deref().unwrap_or(root);
        let mut changed = false;
        for event in &r.events {
            if let Event::SessionLinked { parent, child } = event {
                if parent != child && !parent.is_empty() && !child.is_empty() {
                    graph.nodes.extend([parent.clone(), child.clone()]);
                    changed |= graph.edges.insert(SessionEdge {
                        parent: parent.clone(),
                        child: child.clone(),
                    });
                }
                continue;
            }
            let Some(reading) = TokenReading::from_event(event) else {
                continue;
            };
            if reading.tokens.is_none() && reading.total.is_none() {
                continue;
            }
            let session = self.sessions.entry(source.to_owned()).or_default();
            if let Some(tokens) = reading.tokens {
                if let Some(agent) = session
                    .agents
                    .iter_mut()
                    .find(|a| a.agent_path == r.agent_path)
                {
                    changed |= agent.tokens != tokens;
                    agent.tokens = tokens;
                } else {
                    session.agents.push(AgentTokens {
                        agent_path: r.agent_path.clone(),
                        tokens,
                    });
                    changed = true;
                }
            }
            if let Some(total) = reading.total {
                changed |= session.reported.replace(total) != Some(total);
            }
        }
        changed.then(|| {
            let (usage, children) = self.summary(root, graph);
            UsageSnapshot {
                usage,
                children,
                cursor: r.cursor(),
            }
        })
    }
    pub(crate) fn summary(
        &self,
        root: &str,
        graph: &SessionGraph,
    ) -> (SessionUsage, BTreeMap<String, SessionUsage>) {
        let mut usage = self
            .sessions
            .get(root)
            .map(SessionTokens::usage)
            .unwrap_or_default();
        let mut children = BTreeMap::new();
        let mut visited = BTreeSet::from([root]);
        let mut parents = vec![root];
        while let Some(parent) = parents.pop() {
            for edge in graph
                .edges
                .range(
                    SessionEdge {
                        parent: parent.to_owned(),
                        child: String::new(),
                    }..,
                )
                .take_while(|e| e.parent == parent)
            {
                if visited.insert(&edge.child) {
                    parents.push(&edge.child);
                    if let Some(session) = self.sessions.get(&edge.child) {
                        let child = session.usage();
                        if let (Some(own), Some(total)) = (usage.total, child.total) {
                            usage.with_children =
                                Some(usage.with_children.unwrap_or(own).plus(total));
                        }
                        children.insert(edge.child.clone(), child);
                    }
                }
            }
        }
        (usage, children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, RecordKind, observe::SessionReducer, stream::Journal};
    use serde_json::json;

    #[test]
    fn session_agent_remainders_and_diamond_descendants_survive_record_replay() {
        let journal = Journal::new(Some(1)).unwrap();
        journal.bind_root("root");
        let emit = |source: &str, path: &[&str], event| {
            journal.push(
                Direction::FromRuntime,
                RecordKind::Frame,
                Some(source.into()),
                path.iter().map(|p| p.to_string()).collect(),
                serde_json::Value::Null,
                vec![event],
            )
        };
        let report =
            |source: &str, path: &[&str], value| emit(source, path, Event::Usage { usage: value });
        let edge = |a: &str, b: &str| {
            emit(
                a,
                &[],
                Event::SessionLinked {
                    parent: a.into(),
                    child: b.into(),
                },
            )
        };
        report(
            "leaf",
            &[],
            json!({"tokens":{"input":8,"output":4,"reasoning":2}}),
        );
        report(
            "unlinked",
            &[],
            json!({"tokens":{"input":100,"output":100}}),
        );
        edge("a", "leaf");
        edge("b", "leaf");
        edge("leaf", "root");
        edge("root", "a");
        edge("root", "b");
        assert!(journal.snapshot().usage.total.is_none());
        assert_eq!(
            journal.snapshot().child_usage["leaf"].total.unwrap().input,
            8
        );
        report(
            "root",
            &[],
            json!({"tokens":{"input":5,"output":1,"cacheRead":2}}),
        );
        report(
            "root",
            &["job", "agent"],
            json!({"tokens":{"input":7,"output":2,"cacheWrite":3}}),
        );
        report(
            "root",
            &[],
            json!({"total":{"input":20,"output":8,"cacheRead":4,"cacheWrite":3}}),
        );
        let snap = journal.snapshot();
        let usage = &snap.usage;
        assert_eq!(usage.total.unwrap().output, 8);
        let agents = usage.by_agent.as_ref().unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[1].agent_path, ["job", "agent"]);
        assert_eq!(
            usage.unattributed,
            Some(TokenTotals {
                input: 8,
                output: 5,
                cache_read: Some(2),
                cache_write: Some(0),
                reasoning: None
            })
        );
        assert_eq!(usage.with_children.unwrap().input, 28);
        assert_eq!(usage.with_children.unwrap().reasoning, Some(2));
        let mut replay = SessionReducer::new("root");
        for r in journal.records() {
            replay.apply(&r).unwrap();
        }
        assert_eq!(replay.snapshot(), &snap);
    }
}
