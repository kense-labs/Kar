use super::{ApprovalMode, Duration, Protocol, rig};
use kar::{
    Error, Record, Session,
    agents::{Crew, Landed, SendMode},
};
use uuid::Uuid;

async fn take(crew: &Crew, id: Uuid, number: u64) {
    let reports = crew.wait(&[id], Duration::from_secs(3)).await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].turn, number);
}

fn calls(session: &Session, method: &str, text: &str) -> usize {
    session
        .records()
        .iter()
        .filter(|r| {
            r.native["method"] == method
                && r.native
                    .pointer("/params/input/0/text")
                    .or_else(|| r.native.pointer("/params/prompt/0/text"))
                    .is_some_and(|s| s == text)
        })
        .count()
}

async fn until(session: &Session, predicate: impl Fn(&Record) -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !session.records().iter().any(|r| predicate(r)) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native input did not arrive");
}

pub(super) async fn exercise() -> kar::Result<()> {
    for protocol in [Protocol::Codex, Protocol::Acp] {
        let (_tmp, runtime, opts) = rig(protocol, ApprovalMode::Deny);
        let crew = Crew::new(3).with_running_limit(1);
        let agent = crew
            .spawn(&runtime, opts.clone(), None, "normal".into())
            .await?;
        take(&crew, agent.id, 1).await;
        let first = crew
            .send_mode(agent.id, "wait".into(), SendMode::Followup)
            .await?;
        assert_eq!((first.landed, first.turns_sent), (Landed::Prompted, 2));
        let session = crew.session(agent.id)?;
        let method = if protocol == Protocol::Codex {
            "turn/start"
        } else {
            "session/prompt"
        };
        if protocol == Protocol::Codex {
            until(&session, |r| {
                r.native["method"] == "item/started" && r.native["params"]["item"]["id"] == "tool-1"
            })
            .await;
        }
        let second = crew
            .send_mode(agent.id, "more".into(), SendMode::Followup)
            .await?;
        assert!(matches!(
            crew.spawn(&runtime, opts.clone(), None, "blocked".into())
                .await,
            Err(Error::Refused(_))
        ));
        if protocol == Protocol::Codex {
            assert_eq!((second.landed, second.turns_sent), (Landed::Steered, 2));
            assert_eq!(calls(&session, "turn/steer", "more"), 1);
            assert_eq!(calls(&session, method, "more"), 0);
            assert_eq!(
                crew.send_mode(agent.id, "explicit".into(), SendMode::Steer)
                    .await?
                    .landed,
                Landed::Steered
            );
            let fallback = crew
                .send_mode(agent.id, "fallback".into(), SendMode::Followup)
                .await?;
            assert_eq!((fallback.landed, fallback.turns_sent), (Landed::Queued, 3));
            assert_eq!(calls(&session, "turn/steer", "fallback"), 1);
        } else {
            assert_eq!((second.landed, second.turns_sent), (Landed::Queued, 3));
            assert!(matches!(
                crew.send_mode(agent.id, "explicit".into(), SendMode::Steer)
                    .await,
                Err(Error::Unsupported(_))
            ));
        }
        assert_eq!(calls(&session, method, "wait"), 1);
        crew.interrupt(agent.id).await?;
        let mut reports = Vec::new();
        while reports.len() < 2 {
            let batch = crew.wait(&[agent.id], Duration::from_secs(3)).await;
            assert!(!batch.is_empty());
            reports.extend(batch.into_iter().map(|r| r.turn));
        }
        reports.sort();
        assert_eq!(reports, [2, 3]);
        assert_eq!(
            calls(
                &session,
                method,
                if protocol == Protocol::Codex {
                    "fallback"
                } else {
                    "more"
                }
            ),
            1
        );
        let other = crew.spawn(&runtime, opts, None, "wait".into()).await?;
        assert!(matches!(
            crew.send_mode(agent.id, "blocked".into(), SendMode::Queue)
                .await,
            Err(Error::Refused(_))
        ));
        if protocol == Protocol::Codex {
            until(&crew.session(other.id)?, |r| {
                r.native["method"] == "item/started"
            })
            .await;
        }
        crew.interrupt(other.id).await?;
        take(&crew, other.id, 1).await;
        assert_eq!(
            crew.send_mode(agent.id, "queued".into(), SendMode::Queue)
                .await?
                .landed,
            Landed::Queued
        );
        take(&crew, agent.id, 4).await;
        assert_eq!(calls(&session, method, "queued"), 1);
        crew.close(agent.id).await?;
        crew.close(other.id).await?;
    }
    Ok(())
}
