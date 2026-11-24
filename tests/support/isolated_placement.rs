//! Native macOS controls for two independent ACP sessions in the same effective home.
use kar::{
    Direction, Event, Outcome, PlacementRequest, Runtime, Session, SessionOptions, SessionPlacement,
};
use std::{collections::BTreeSet, path::Path, process::Command, time::Duration};

fn children() -> BTreeSet<u32> {
    // Inspect only numeric process identities (no command lines or environment values).
    let ps = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let observer = ps.id();
    let out = ps.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|row| {
            let mut it = row.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            (pid != observer && it.next()?.parse::<u32>().ok()? == std::process::id())
                .then_some(pid)
        })
        .collect()
}

async fn prompt(session: &Session, text: &str) {
    let outcome = tokio::time::timeout(Duration::from_secs(60), async {
        session.prompt(text).await.unwrap().wait().await.unwrap()
    })
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Completed);
}

fn records(session: &Session, cwd: &Path, mine: &str, peer: &str) {
    let records = session.records();
    assert!(records.iter().any(|r| {
        r.direction == Direction::ToRuntime
            && r.native["method"] == "session/new"
            && r.native
                .pointer("/params/cwd")
                .and_then(serde_json::Value::as_str)
                == cwd.to_str()
    }));
    let prompts: Vec<_> = records
        .iter()
        .filter(|r| r.native["method"] == "session/prompt" && r.direction == Direction::ToRuntime)
        .collect();
    assert!(prompts.iter().any(|r| r.native.to_string().contains(mine)));
    assert!(prompts.iter().all(|r| !r.native.to_string().contains(peer)));
    assert!(
        records
            .iter()
            .any(|r| r.session_id.as_deref() == Some(session.id())
                && r.events.iter().any(|e| matches!(
                    e,
                    Event::TurnEnded {
                        outcome: Outcome::Completed
                    }
                )))
    );
}

pub async fn verify(rt: Runtime, mut options: SessionOptions) {
    let first_cwd = options.cwd.canonicalize().unwrap();
    options.cwd = first_cwd.clone();
    let second_cwd = first_cwd.parent().unwrap().join("second-workspace");
    std::fs::create_dir(&second_cwd).unwrap();
    let mut second = options.clone();
    second.cwd = second_cwd.clone();
    let shared = rt
        .with_placement(PlacementRequest::Shared)
        .open(options.clone())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(shared, kar::Error::Unsupported(detail) if detail.contains("shared process HOME"))
    );
    let before = children();
    let a = rt
        .with_placement(PlacementRequest::Auto)
        .open(options.clone())
        .await
        .unwrap();
    let first = children()
        .difference(&before)
        .copied()
        .collect::<BTreeSet<_>>();
    let before_b = children();
    let b = rt
        .with_placement(PlacementRequest::Isolated)
        .open(second)
        .await
        .unwrap();
    let second = children()
        .difference(&before_b)
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(!first.is_empty() && !second.is_empty() && first.is_disjoint(&second));
    for session in [&a, &b] {
        assert_eq!(session.placement(), Some(SessionPlacement::Isolated));
        assert_eq!(session.resources().await.unwrap().scope, None);
    }
    assert_ne!(a.id(), b.id());
    let a_id = a.id().to_owned();
    tokio::join!(
        prompt(&a, "kar-isolation-left"),
        prompt(&b, "kar-isolation-right")
    );
    records(&a, &first_cwd, "kar-isolation-left", "kar-isolation-right");
    records(&b, &second_cwd, "kar-isolation-right", "kar-isolation-left");
    a.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let live = children();
            assert!(
                second.is_subset(&live),
                "closing the left session stopped the right process"
            );
            if first.is_disjoint(&live) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("left session processes remained after closing");
    assert!(a.resources().await.is_none());
    prompt(&b, "kar-isolation-survivor").await;
    options.resume = Some(a_id.clone());
    let resumed = rt.open(options).await.unwrap();
    assert_eq!(resumed.id(), a_id);
    assert_eq!(resumed.placement(), Some(SessionPlacement::Isolated));
    b.close().await.unwrap();
    prompt(&resumed, "kar-isolation-resumed").await;
    resumed.close().await.unwrap();
}
