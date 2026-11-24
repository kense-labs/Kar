use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn cli_imports_and_replays_kar_v3_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("voyage.jsonl");
    let output = dir.path().join("imported.jsonl");
    let receipts = [
        json!({"kind":"request","id":"attempt/1","direction":"toRuntime","body":{"kind":"prompt","input":"ping","inputId":"client/1","admission":"native"}}),
        json!({"kind":"frame","body":{"type":"response","native":{"type":"response","id":"1","command":"prompt","success":true,"data":{"disposition":"handled"}},"events":[{"kind":"input_handled","requestId":"attempt/1","inputId":"client/1"}]}}),
        json!({"kind":"response","requestId":"attempt/1","body":{"kind":"accepted"}}),
    ];
    let header = json!({"kind":"header","format":"kar-voyage/3","runtime":"pi","sessionId":"s","startedAt":1});
    let end = json!({"kind":"end","reason":"closed","at":5});
    let mut lines = vec![header.to_string()];
    for (seq, mut record) in receipts.into_iter().enumerate() {
        record["sessionId"] = json!("s");
        record["agentPath"] = json!([]);
        record["seq"] = json!(seq);
        record["receivedAt"] = json!(seq + 1);
        lines.push(json!({"kind":"record","record":record}).to_string());
    }
    lines.push(end.to_string());
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    let read = |path| {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect::<Vec<_>>()
    };
    let original = read(&file);
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_kar"))
            .arg("import")
            .arg(&file)
            .arg(&output)
            .output()
            .unwrap()
    };
    let result = run();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let summary: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(summary["header"], original[0]);
    assert_eq!(summary["header"]["format"], "kar-voyage/3");
    assert_eq!(summary["end"], original[4]);
    assert_eq!(summary["records"], 3);
    let converted = read(&output);
    assert_eq!(converted.len(), 3);
    for (row, source) in converted.iter().zip(&original[1..4]) {
        assert_eq!(row["upstream"], source["record"]);
    }
    assert_eq!(converted[0]["prompt"]["request_id"], "attempt/1");
    assert_eq!(converted[0]["prompt"]["input_id"], "client/1");
    let replay = Command::new(env!("CARGO_BIN_EXE_kar"))
        .arg("replay")
        .arg(&output)
        .output()
        .unwrap();
    assert!(replay.status.success());
    assert_eq!(replay.stdout, fs::read(&output).unwrap());
    let logical = Command::new(env!("CARGO_BIN_EXE_kar"))
        .arg("replay")
        .arg(&output)
        .arg("--conversation")
        .output()
        .unwrap();
    assert!(
        logical.status.success(),
        "{}",
        String::from_utf8_lossy(&logical.stderr)
    );
    let history: Value = serde_json::from_slice(&logical.stdout).unwrap();
    let input = &history["inputs"][0];
    assert_eq!(history["inputs"].as_array().unwrap().len(), 1);
    assert_eq!(input["input_id"], "client/1");
    assert_eq!(input["handled"]["after_seq"], 1);
    assert_eq!(input["attempts"][0]["state"], "accepted");
    assert_eq!(input["attempts"][0]["response"]["seq"], 2);
    assert!(input["observations"].as_array().unwrap().is_empty());
    let suffix = Command::new(env!("CARGO_BIN_EXE_kar"))
        .arg("replay")
        .arg(&output)
        .args([
            "--stream",
            summary["stream_id"].as_str().unwrap(),
            "--after",
            "0",
        ])
        .output()
        .unwrap();
    assert!(suffix.status.success());
    let tail: Vec<Value> = String::from_utf8(suffix.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(tail, converted[1..]);
    assert!(!run().status.success());
    assert_eq!(read(&output), converted);
    fs::remove_file(&output).unwrap();
    fs::write(
        &file,
        format!("{}\n{}\n{}\n", original[0], original[1], original[3]),
    )
    .unwrap();
    assert!(!run().status.success());
    assert!(!output.exists());
}

#[test]
fn other_v3_header_is_rejected_before_publishing_any_output() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("foreign.jsonl");
    let output = dir.path().join("imported.jsonl");
    let header = json!({"kind":"header","format":"oar-voyage/3","sessionId":"s"});
    fs::write(&file, format!("{header}\n")).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_kar"))
            .arg("import")
            .arg(&file)
            .arg(&output)
            .output()
            .unwrap()
    };
    let result = run();
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("expected a kar-voyage/3 recording header")
    );
    assert!(!output.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    fs::write(&output, b"original contents").unwrap();
    assert!(!run().status.success());
    assert_eq!(fs::read(&output).unwrap(), b"original contents");
}
