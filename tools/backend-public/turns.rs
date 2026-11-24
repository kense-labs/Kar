use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use kar::{Error, Event, Outcome, RecordKind, Session};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use uuid::Uuid;

#[path = "../../tests/support/abort_turn.rs"]
mod abort;
#[path = "../../tests/support/image_input.rs"]
mod image;
#[path = "../../tests/support/public_readbacks.rs"]
mod readbacks;

fn root_ends(session: &Session, after: u64) -> usize {
    session
        .records()
        .iter()
        .filter(|r| {
            r.seq > after
                && r.kind == RecordKind::Frame
                && r.agent_path.is_empty()
                && r.events
                    .iter()
                    .any(|e| matches!(e, Event::TurnEnded { .. }))
        })
        .count()
}

fn decision(
    session: &Session,
    from: u64,
    control: &str,
    id: Option<Uuid>,
) -> Result<(String, Value)> {
    let records = session.records();
    let attempt = records
        .iter()
        .find(|r| {
            r.seq > from
                && r.kind == RecordKind::Request
                && r.native["type"] == control
                && id.is_none_or(|id| r.native["inputId"] == id.to_string())
        })
        .ok_or_else(|| anyhow::anyhow!("no correlated {control} attempt"))?;
    let key = attempt
        .control_request_id()
        .ok_or_else(|| anyhow::anyhow!("{control} has no request ID"))?;
    let receipt = session
        .control_receipt(key)
        .ok_or_else(|| anyhow::anyhow!("{control} has no reply"))?;
    ensure!(
        receipt.response.seq > attempt.seq,
        "reply preceded its request"
    );
    Ok((key.to_owned(), receipt.response.native.clone()))
}

pub(super) async fn run(case: &str, session: &Session, cwd: &Path) -> Result<Value> {
    let mut evidence = json!({"native_session":session.id(),"case":case});
    match case {
        "session.query-readbacks-carry-stream-seq" => readbacks::check(session).await?,
        "session.framing-and-attribution"
        | "session.observer-isolation"
        | "session.multi-turn-conversation"
        | "session.steer-after-end"
        | "session.cursor-replays-without-loss-or-duplication"
        | "session.dispose-is-recorded" => {
            readbacks::stream::check(session).await?;
            evidence["also_checked"] = json!([
                "session.multi-turn-conversation",
                "session.observer-isolation",
                "session.cursor-replays-without-loss-or-duplication",
                "session.dispose-is-recorded"
            ]);
            if session.capabilities().steering {
                evidence["late_steer"] = "rejected".into();
            }
            return Ok(evidence);
        }
        "session.dispose-mid-turn-settles" => {
            readbacks::stream::dispose_mid_turn(session).await?;
            return Ok(evidence);
        }
        "session.images-travel-with-input" => {
            image::empty_controls(session).await;
            let path = cwd.join("image.png");
            std::fs::write(&path, STANDARD.decode(image::PNG)?)?;
            if session.capabilities().image_input {
                image::refused_files(
                    session,
                    "describe the image",
                    &[cwd.join("missing.png")],
                    "error",
                )
                .await;
                let turn = session
                    .prompt_image_paths("", std::slice::from_ref(&path))
                    .await?;
                let receipt = turn.admission().expect("image prompt admission");
                let submission = receipt
                    .request
                    .prompt
                    .as_ref()
                    .and_then(|p| p.submission.as_ref())
                    .expect("image path");
                ensure!(
                    submission.input.is_empty()
                        && submission.images.len() == 1
                        && submission.images[0].path.as_deref() == path.to_str()
                );
                ensure!(turn.wait().await? == Outcome::Completed);
                evidence["image_only"] = "completed".into();
            } else {
                image::refused_files(session, "describe the image", &[path], "unsupported").await;
                evidence["image_only"] = "native capability absent, whole input refused".into();
            }
        }
        case => {
            let slow = session
                .prompt("Please answer slowly, then reply OK.")
                .await?;
            let since = slow.admission().expect("admitted prompt").request.seq;
            match case {
                "session.single-active-turn" => {
                    ensure!(session.status().is_running());
                    ensure!(matches!(
                        session.prompt("second active prompt").await,
                        Err(Error::Busy)
                    ));
                    ensure!(slow.wait().await? == Outcome::Completed);
                    ensure!(!session.status().is_running());
                }
                "session.abort-settles-and-is-idempotent" => {
                    evidence["outcome"] = json!(abort::check(session, slow).await);
                }
                "session.steer-mid-turn" => {
                    if !session.capabilities().steering {
                        ensure!(slow.wait().await? == Outcome::Completed);
                        evidence["steering"] = "not_declared".into();
                    } else {
                        let id = Uuid::new_v4();
                        let response = session.steer_with_id(id, "mid-turn note").await;
                        ensure!(
                            matches!(
                                response,
                                Ok(()) | Err(Error::Refused(_)) | Err(Error::Unsupported(_))
                            ),
                            "steer transport failed: {response:?}"
                        );
                        let (_, reply) = decision(session, since, "steer", Some(id))?;
                        ensure!(matches!(
                            reply["type"].as_str(),
                            Some("accepted" | "rejected")
                        ));
                        evidence["steering"] = reply;
                        ensure!(slow.wait().await? != Outcome::Failed);
                    }
                }
                "session.queue-runs-after-turn" | "session.withdraw-before-dispatch" => {
                    let (input, queued) = session.queue("and then this").await?;
                    let admission = queued.admission().expect("queue admission").clone();
                    ensure!(admission.response.native["type"] == "accepted");
                    let withdrawn = if case == "session.withdraw-before-dispatch" {
                        let result = session.withdraw(input).await;
                        let (_, reply) = decision(session, since, "withdraw", Some(input))?;
                        ensure!(
                            result.is_ok()
                                || matches!(
                                    reply["code"].as_str(),
                                    Some("not_queued" | "unsupported")
                                ),
                            "unexpected withdraw response {reply}"
                        );
                        evidence["withdraw"] = reply;
                        result.is_ok()
                    } else {
                        false
                    };
                    ensure!(slow.wait().await? == Outcome::Completed);
                    if withdrawn {
                        ensure!(queued.wait().await? == Outcome::Aborted);
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        ensure!(root_ends(session, since) == 1 && !session.status().is_running());
                        ensure!(!session.records().iter().any(|r| r.events.contains(
                            &Event::UserMessage {
                                text: "and then this".into()
                            }
                        )));
                        ensure!(matches!(
                            session.withdraw(input).await,
                            Err(Error::Refused(_))
                        ));
                    } else {
                        ensure!(queued.wait().await? == Outcome::Completed);
                        ensure!(root_ends(session, since) == 2);
                    }
                    ensure!(
                        session
                            .records()
                            .iter()
                            .any(|r| std::sync::Arc::ptr_eq(r, &admission.request))
                    );
                    if case == "session.withdraw-before-dispatch" {
                        let unknown = Uuid::new_v4();
                        ensure!(session.withdraw(unknown).await.is_err());
                        let (_, reply) = decision(session, since, "withdraw", Some(unknown))?;
                        ensure!(matches!(
                            reply["code"].as_str(),
                            Some("not_queued" | "unsupported")
                        ));
                        session.close().await?;
                        let after = session.records().last().unwrap().seq;
                        ensure!(session.withdraw(input).await.is_err());
                        let (_, reply) = decision(session, after, "withdraw", Some(input))?;
                        ensure!(matches!(
                            reply["code"].as_str(),
                            Some("disposed" | "runtime_exited")
                        ));
                        return Ok(evidence);
                    }
                }
                _ => anyhow::bail!("unknown turn case {case}"),
            }
        }
    }
    session.close().await?;
    Ok(evidence)
}
