//! Coalesced refresh epochs over authenticated HTTP, with two independent consumers.
use super::*;
use reqwest::Response;
use serde_json::{Value, json};

async fn event(response: &mut Response) -> u64 {
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut bytes = Vec::new();
        loop {
            bytes.extend(response.chunk().await.unwrap().expect("SSE stream"));
            if let Some(v) = std::str::from_utf8(&bytes)
                .ok()
                .filter(|s| s.contains("\n\n"))
            {
                let id = v
                    .lines()
                    .find_map(|l| l.strip_prefix("id:"))
                    .unwrap()
                    .trim()
                    .parse::<u64>()
                    .unwrap();
                let data = v
                    .lines()
                    .find_map(|l| l.strip_prefix("data:"))
                    .unwrap()
                    .trim()
                    .parse::<u64>()
                    .unwrap();
                assert_eq!(id, data);
                return id;
            }
        }
    })
    .await
    .expect("forge refresh event")
}
async fn stream(c: &reqwest::Client, base: &str, kind: &str, after: u64) -> Response {
    c.get(format!("{base}/forge/refreshes/{kind}"))
        .header("Last-Event-ID", after.to_string())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
}
fn reference(project: &str) -> Value {
    json!({"projectId":project,"host":"review.example","repository":"team/repo","number":1})
}
pub(super) async fn before(c: &reqwest::Client, base: &str, project: &str) -> u64 {
    let target = format!("{base}/forge/pulls/invalidate");
    let first = ok(c.post(&target).json(&json!({}))).await["revision"]
        .as_u64()
        .unwrap();
    let second = server::client();
    let mut a = stream(c, base, "pulls", 0).await;
    let mut b = stream(&second, base, "pulls", 0).await;
    assert_eq!(event(&mut a).await, first);
    assert_eq!(event(&mut b).await, first);
    let viewed = ok(c
        .post(&target)
        .json(&json!({"filesViewedOnly":true,"reference":reference(project)})))
    .await;
    assert_eq!(viewed["revision"], first);
    let updated = ok(c
        .post(&target)
        .json(&json!({"reference":reference(project)})))
    .await["revision"]
        .as_u64()
        .unwrap();
    assert_eq!(updated, first + 1);
    assert_eq!(event(&mut a).await, updated);
    assert_eq!(event(&mut b).await, updated);
    let data = json!({"reference":reference(project),"state":"merged","isDraft":false});
    let reports = format!("{base}/forge/pulls/report-state");
    let native = ok(c.post(&reports).json(&data)).await;
    assert_eq!(native["duplicate"], false);
    let duplicate = ok(second.post(&reports).json(&data)).await;
    assert_eq!(duplicate["revision"], native["revision"]);
    assert_eq!(duplicate["duplicate"], true);
    let mut state = stream(&second, base, "reported-states", 0).await;
    assert_eq!(
        event(&mut state).await,
        native["revision"].as_u64().unwrap()
    );
    let truth = ok(c
        .post(format!("{base}/projects/{project}/forge/query"))
        .json(&json!({"op":"pull","number":1})))
    .await;
    assert_eq!(
        truth["state"], "open",
        "client reports cannot become hosted truth"
    );
    let future = c
        .get(format!("{base}/forge/refreshes/pulls"))
        .header("last-event-id", u64::MAX.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(future.status().as_u16(), 400);
    let mut links = stream(c, base, "linked-chats", 0).await;
    event(&mut links).await
}
pub(super) async fn after(c: &reqwest::Client, base: &str, project: &str, links_before: u64) {
    let mut links = stream(c, base, "linked-chats", links_before).await;
    assert!(
        event(&mut links).await > links_before,
        "subsequent persisted link changes after restart"
    );
    let data = json!({"reference":reference(project),"state":"merged","isDraft":false});
    let duplicate = ok(c
        .post(format!("{base}/forge/pulls/report-state"))
        .json(&data))
    .await;
    assert_eq!(duplicate["duplicate"], true);
}
