use super::*;
use std::{fs, path::Path};

fn titles(home: &Path, id: &str) -> Vec<String> {
    // Use DSH's plaintext JSONL option for this fixture. Each protocol writes
    // its own native format, independent of Kar's saved ChatInfo.
    fn logs(dir: &Path, id: &str, matches_id: bool, output: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let file = entry.path();
            let name = entry.file_name();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                logs(&file, id, matches_id || name == id, output);
            } else if kind.is_file()
                && file.extension().is_some_and(|ext| ext == "jsonl")
                && (matches_id || file.file_stem().is_some_and(|stem| stem == id))
            {
                output.push(file);
            }
        }
    }
    let mut files = Vec::new();
    logs(&home.join("sessions"), id, false, &mut files);
    assert_eq!(files.len(), 1, "native log for {id}: {files:?}");
    fs::read_to_string(&files[0])
        .unwrap()
        .lines()
        .filter_map(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            (event["type"] == "session/title")
                .then(|| event["data"]["title"].as_str().unwrap().to_owned())
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires real DSH Web"]
async fn native_saved_title() {
    saved_title(Protocol::Dsh).await;
}

#[tokio::test]
#[ignore = "requires real DSH ACP"]
async fn legacy_saved_title() {
    saved_title(Protocol::Acp).await;
}

async fn saved_title(protocol: Protocol) {
    let tb = testbed(protocol).await;
    let home = tb.dir.path().join("home/.dsh");
    fs::create_dir_all(&home).unwrap();
    let patch = json!([{"id":"session-persistence-jsonl","config":{"root":home.join("sessions"),"compression":"none"}}]);
    fs::write(home.join("cordis.patch.yml"), patch.to_string()).unwrap();
    let mut named = tb.opts.clone();
    named.settings.name = Some("  Original 聊天  ".into());
    let chat = tb
        .store
        .create("dsh", Some(&tb.runtime.program), named)
        .await
        .unwrap();
    let id = chat.info().id;
    let native = chat.info().native_id.clone();
    chat.close().await.unwrap();
    assert_eq!(titles(&home, &native), ["Original 聊天"]);

    let store = ChatStore::open(tb.dir.path().join("store")).unwrap();
    let resumed = store
        .resume_with(id, |o| {
            o.env = tb.opts.env.clone();
            o.settings.name = Some("request to rename".into());
        })
        .await
        .unwrap();
    assert_eq!(resumed.info().native_id, native);
    resumed.close().await.unwrap();
    assert_eq!(titles(&home, &native), ["Original 聊天"]);

    let fresh = tb.runtime.open(tb.opts.clone()).await.unwrap();
    let empty = fresh.id().to_owned();
    fresh.close().await.unwrap();
    assert!(titles(&home, &empty).is_empty());
    let mut fill = tb.opts.clone();
    fill.resume = Some(empty.clone());
    fill.settings.name = Some("Filled on resume".into());
    // Preserve the title even when the requested name comes together with
    // other scope-bound settings, and apply a name to an untitled session.
    fill.settings.system_prompt = Some("root persona".into());
    if protocol == Protocol::Dsh {
        let mut invalid = fill.clone();
        invalid.model = Some("missing-provider/missing-model".into());
        assert!(tb.runtime.open(invalid).await.is_err());
        assert!(titles(&home, &empty).is_empty());
    }
    tb.runtime
        .open(fill.clone())
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    fill.settings.name = Some("Second title".into());
    tb.runtime.open(fill).await.unwrap().close().await.unwrap();
    assert_eq!(titles(&home, &empty), ["Filled on resume"]);
    assert!(tb.data.requests.lock().unwrap().is_empty());

    let automatic = tb.runtime.open(tb.opts.clone()).await.unwrap();
    let auto_id = automatic.id().to_owned();
    assert_eq!(
        automatic
            .prompt("DSH_PROMPT_TITLE describe this workspace")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap(),
        kar::Outcome::Completed
    );
    automatic.close().await.unwrap();
    let original = titles(&home, &auto_id);
    assert!(!original.is_empty(), "expected DSH's generated title");
    let mut keep = tb.opts.clone();
    keep.resume = Some(auto_id.clone());
    keep.settings.name = Some("manual override".into());
    let count = tb.data.requests.lock().unwrap().len();
    tb.runtime.open(keep).await.unwrap().close().await.unwrap();
    assert_eq!(titles(&home, &auto_id), original);
    assert_eq!(tb.data.requests.lock().unwrap().len(), count);
}
