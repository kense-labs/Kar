use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAKElEQVR4nO3NsQ0AAAzCMP5/un0CNkuZ41wybXsHAAAAAAAAAAAAxR4yw/wuPL6QkAAAAABJRU5ErkJggg==";

#[tokio::test]
#[ignore = "requires official DSH Web 0.2.2-rc.5, native addons and the macOS image codec; uses a loopback model"]
async fn uploads_images_to_the_selected_route_and_recovers_native_references() {
    let tb = testbed(Protocol::Dsh).await;
    tb.data
        .image_routes
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let input = kar::InputImage::from_bytes(&STANDARD.decode(PNG).unwrap()).unwrap();
    let opts = tb.opts.clone();
    let session = tb.runtime.open(opts.clone()).await.unwrap();
    assert!(session.capabilities().image_input);
    assert!(
        session.info()["imageTypes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|it| it == "image/png")
    );
    let count = session.info()["imageLimits"]["maxImagesPerMessage"]
        .as_u64()
        .unwrap();
    assert!((1..=4096).contains(&count));
    let excess = vec![input.clone(); count as usize + 1];
    assert!(
        matches!(session.prompt_images("too many",excess).await,Err(kar::Error::Refused(reason)) if reason.contains("native attachment limits"))
    );
    assert!(tb.data.requests.lock().unwrap().is_empty());
    let one = Uuid::new_v4();
    let first = session
        .prompt_images_with_id(one, "DSH_IMAGE_ROUTE", vec![input.clone()])
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(first, kar::Outcome::Completed);
    let two = Uuid::new_v4();
    let second = session
        .queue_images_with_id(two, "", vec![input.clone()])
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(second, kar::Outcome::Completed);
    let file = session
        .native_session_file()
        .await
        .unwrap()
        .expect("DSH session file");
    let records = session.records();
    let mut references = Vec::new();
    for id in [one, two] {
        let echoed: Vec<_> = records
            .iter()
            .filter(|r| {
                r.native["event"]["type"] == "user/message"
                    && r.native
                        .pointer("/event/data/source/rpcId")
                        .is_some_and(|v| v == &json!(id))
            })
            .collect();
        assert_eq!(echoed.len(), 1);
        let content = echoed[0].native["event"]["data"]["content"]
            .as_array()
            .unwrap();
        assert!(
            content
                .iter()
                .any(|p| p["type"] == "image" && p["attachment"]["attachmentId"].is_string()),
            "{content:?}"
        );
        references.extend(
            content
                .iter()
                .filter_map(|part| part.pointer("/attachment/attachmentId")?.as_str())
                .map(str::to_owned),
        );
        if id == two {
            assert!(content.iter().all(|p| p["type"] != "text"));
        }
    }
    assert!(
        records
            .iter()
            .all(|row| !row.native.to_string().contains(PNG))
    );
    assert!(
        tb.data
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["messages"].to_string().contains("\"image\""))
    );
    let native = session.id().to_owned();
    session.close().await.unwrap();

    // Changing to the text-only model on the same native session must change Kar's opened capability.
    let mut opts = opts;
    opts.resume = Some(native);
    opts.model = Some("deepseek-official/deepseek-v4-pro".into());
    let session = tb.runtime.open(opts).await.unwrap();
    assert!(!session.capabilities().image_input);
    assert_eq!(
        session.native_session_file().await.unwrap(),
        Some(file.clone())
    );
    let persisted = std::fs::read_to_string(file).unwrap();
    assert_eq!(references.len(), 2);
    for reference in references {
        assert!(persisted.contains(&reference));
    }
    assert!(!persisted.contains(PNG));
    let before = tb.data.requests.lock().unwrap().len();
    for queue in [false, true] {
        let attempt = if queue {
            session
                .queue_images_with_id(Uuid::new_v4(), "", vec![input.clone()])
                .await
        } else {
            session
                .prompt_images("unsupported", vec![input.clone()])
                .await
        };
        assert!(matches!(attempt, Err(kar::Error::Unsupported(_))));
    }
    assert_eq!(before, tb.data.requests.lock().unwrap().len());
    session.close().await.unwrap();
}
