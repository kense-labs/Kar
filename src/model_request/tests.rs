use super::*;
use serde_json::json;
use std::{fs, io::Write};

fn owner(home: &Path) -> Owner {
    Owner::start(
        files::create_directory(home).unwrap(),
        "root",
        vec!["scripted-secret".into()],
    )
    .unwrap()
}
fn frame(id: &str) -> Value {
    json!({"type":"assistant","message":{"id":id},"session_id":"root"})
}
fn row(id: &str) -> Value {
    json!({"session_id":"root","query_source":"sdk","message_id":id,"response_file":format!("{id}.response.json"),"request_file":format!("{id}.request.json"),"timestamp":"2026-01-12T00:01:00Z"})
}
fn append(dir: &Path, row: &Value) {
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("index.jsonl"))
        .unwrap();
    writeln!(out, "{row}").unwrap();
}
fn put(dir: &Path, id: &str, body: &Value) {
    fs::write(dir.join(format!("{id}.request.json")), body.to_string()).unwrap();
    fs::write(
        dir.join(format!("{id}.response.json")),
        r#"{"model":"response-only","content":"must not escape"}"#,
    )
    .unwrap();
}
#[tokio::test]
async fn attributes_only_root_calls_and_waits_for_a_complete_body() {
    let home = tempfile::tempdir().unwrap();
    let owner = owner(home.path());
    let r = &owner.reader;
    let path = owner._directory.path();
    let options = json!({"response":{"subtype":"success","response":{"effective":{"env":{SETTING:"file:private-other"}},"sources":[{"source":"policySettings","settings":{"env":{SETTING:"file:private-other"}}}]}}});
    let error = r.verify(&options).unwrap_err().to_string();
    assert!(error.contains("policySettings") && !error.contains("private-other"));
    // Neither non-SDK traffic nor a child, synthetic or API-error assistant frame establishes a wait.
    for (id, session, source) in [("child", "other", "sdk"), ("title", "root", "title")] {
        let mut wrong = row(id);
        wrong["query_source"] = source.into();
        wrong["session_id"] = session.into();
        put(path, id, &json!({"model":"auxiliary"}));
        append(path, &wrong);
    }
    for (key, value) in [
        ("parent_tool_use_id", "tool".into()),
        ("session_id", "other".into()),
        ("is_api_error_message", true.into()),
    ] {
        let mut v = frame("skip");
        v[key] = value;
        r.observe(&v);
    }
    let mut synthetic = frame("synthetic");
    synthetic["message"]["model"] = "<synthetic>".into();
    r.observe(&synthetic);
    assert!(r.read().await.unwrap().is_none());
    assert!(
        !path.join("child.request.json").exists() && !path.join("title.response.json").exists()
    );
    let body = json!({"model":"scripted-secret-primary","messages":[{"content":[{"type":"thinking","thinking":"<REDACTED>"}]}],"tools":[{"properties":{"password":{"type":"string"},"scripted-secret":{"type":"object"}}}]});
    put(path, "first", &body);
    append(path, &row("first"));
    let first = r.read().await.unwrap().unwrap(); // index can arrive before the assistant
    r.observe(&frame("first"));
    r.ended(&Ok(Outcome::Completed));
    assert_eq!(first.model.as_deref(), Some("[redacted]-primary"));
    assert_eq!(first.withheld, ["thinking"]);
    let tools = &first.body.as_ref().unwrap()["tools"][0]["properties"];
    assert_eq!(tools["password"]["type"], "string");
    assert_eq!(tools["[redacted]"]["type"], "object");

    r.observe(&frame("second"));
    assert!(
        tokio::time::timeout(Duration::from_millis(40), r.read())
            .await
            .is_err()
    );
    // Index bytes, including a partial UTF-8 sequence, cannot take effect before the newline.
    let mut second = row("second");
    second["annotation"] = "测试".into();
    let encoded = second.to_string();
    let cut = encoded.find('测').unwrap() + 1;
    let mut out = fs::OpenOptions::new()
        .append(true)
        .open(path.join("index.jsonl"))
        .unwrap();
    out.write_all(&encoded.as_bytes()[..cut]).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), r.read())
            .await
            .is_err()
    );
    fs::write(path.join("second.request.json"), b"{").unwrap();
    out.write_all(&encoded.as_bytes()[cut..]).unwrap();
    out.write_all(b"\n").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), r.read())
            .await
            .is_err()
    );
    put(path, "second", &json!({"not_recorded":true}));
    let second = r.read().await.unwrap().unwrap();
    assert_eq!(second.withheld, ["body"]);
    assert!(second.body.is_none());
    assert!(!path.join("first.request.json").exists());

    let mut old = row("older");
    old["timestamp"] = "2026-01-01T00:00:00Z".into();
    put(path, "older", &json!({"model":"obsolete"}));
    append(path, &old);
    assert_eq!(r.read().await.unwrap(), Some(second));
    let mut no_body = row("missing");
    no_body["request_file"] = Value::Null;
    fs::write(
        path.join("missing.response.json"),
        r#"{"model":"response-only","content":"private answer"}"#,
    )
    .unwrap();
    append(path, &no_body);
    let missing = r.read().await.unwrap().unwrap();
    assert_eq!(missing.withheld, ["not_captured"]);
    assert!(missing.body.is_none());
    assert_eq!(missing.model.as_deref(), Some("response-only"));
    // A failed turn discards only its own unconfirmed answers.
    r.observe(&frame("unanswered"));
    r.ended(&Ok(Outcome::Failed));
    assert_eq!(r.read().await.unwrap(), Some(missing));
}
#[tokio::test]
async fn missing_attribution_times_out_and_close_wins_pending_reads() {
    let home = tempfile::tempdir().unwrap();
    let owner = owner(home.path());
    let path = owner._directory.path().to_owned();
    let r = owner.reader.clone();
    r.observe(&frame("lost"));
    let result = r.read().await;
    assert!(matches!(result, Err(Error::Timeout(_))));
    assert!(matches!(r.read().await, Err(Error::Timeout(_))));
    r.ended(&Ok(Outcome::Aborted));
    assert!(r.read().await.unwrap().is_none());
    r.observe(&frame("waiting"));
    let read = tokio::spawn({
        let r = r.clone();
        async move { r.read().await }
    });
    tokio::task::yield_now().await;
    drop(owner);
    assert!(read.await.unwrap().unwrap().is_none());
    assert!(!path.exists());
}
#[test]
fn only_sweeps_abandoned_marked_directories_and_validates_native_names() {
    let home = tempfile::tempdir().unwrap();
    let first = files::create_directory(home.path()).unwrap();
    let stale = home.path().join("oar-rs-claude-requests-old");
    fs::create_dir(&stale).unwrap();
    fs::copy(first.path().join(".owner"), stale.join(".owner")).unwrap();
    let other = home.path().join("oar-rs-claude-requests-unrelated");
    fs::create_dir(&other).unwrap();
    fs::write(other.join(".owner"), "another application").unwrap();
    let _second = files::create_directory(home.path()).unwrap();
    assert!(other.is_dir() && first.path().is_dir() && !stale.exists());
    for bad in [
        "../foreign.request.json",
        "/elsewhere.request.json",
        "one.response.json",
        "a.b.request.json",
    ] {
        let mut v = row("id");
        v["request_file"] = bad.into();
        assert!(Entry::from_row(&v).is_none());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        assert_eq!(
            fs::metadata(first.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let outside = home.path().join("foreign.json");
        fs::write(&outside, r#"{"model":"wrong"}"#).unwrap();
        symlink(&outside, first.path().join("id.request.json")).unwrap();
        assert!(
            Entry::from_row(&row("id"))
                .unwrap()
                .read(first.path(), &[])
                .is_none()
        );
    }
}
