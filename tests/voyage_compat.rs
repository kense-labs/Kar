use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};

#[test]
#[ignore = "requires Node 24 and KAR_OAR_SOURCE containing the v0.130.1 voyage writer"]
fn imports_files_from_upstream_open_voyage() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = PathBuf::from(std::env::var_os("KAR_OAR_SOURCE").unwrap())
        .join("packages/oar/src/voyage.ts");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&upstream).unwrap())),
        "5f34277e97ab468bba66347e456031ca6205162bb08e6ca8239ef7d6bd9b003e"
    );
    let file = dir.path().join("voyage.jsonl");
    let output = dir.path().join("imported.jsonl");
    let receipts = [
        json!({"kind":"request","id":"attempt/1","direction":"toRuntime","body":{"kind":"prompt","input":"ping","inputId":"client/1","admission":"native"}}),
        json!({"kind":"frame","body":{"type":"response","native":{"type":"response","id":"1","command":"prompt","success":true,"data":{"disposition":"handled"}},"events":[{"kind":"input_handled","requestId":"attempt/1","inputId":"client/1"}]}}),
        json!({"kind":"response","requestId":"attempt/1","body":{"kind":"accepted"}}),
    ];
    let writer = r#"
        import { pathToFileURL } from 'node:url';
        const {openVoyage} = await import(pathToFileURL(process.argv[1]));
        const sink = openVoyage(process.argv[2], {runtime:'pi', cwd:'.', sessionId:'s', startedAt:1, recorder:'kar-test'});
        for (const [seq,row] of JSON.parse(process.argv[3]).entries())
            sink.record({...row,sessionId:'s',agentPath:[],seq,receivedAt:seq+1});
        sink.end('closed');"#;
    let wrote = Command::new("node")
        .args(["--input-type=module", "-e", writer])
        .arg(&upstream)
        .arg(&file)
        .arg(serde_json::to_string(&receipts).unwrap())
        .output()
        .unwrap();
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
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
    assert_eq!(summary["header"]["format"], "oar-voyage/3");
    assert_eq!(summary["end"], original[4]);
    assert_eq!(summary["records"], 3);
    let converted = read(&output);
    assert_eq!(converted.len(), 3);
    for (row, source) in converted.iter().zip(&original[1..4]) {
        assert_eq!(row["upstream"], source["record"]);
    }
    assert_eq!(converted[0]["prompt"]["request_id"], "attempt/1");
    assert_eq!(converted[0]["prompt"]["input_id"], "client/1");
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
