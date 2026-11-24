use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn query(root: &Path, kind: &str, runtime: &str, stall: bool, array: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kar"));
    cmd.args([kind, runtime, "--cwd"])
        .arg(root)
        .args(["--timeout-ms", if stall { "1500" } else { "5000" }])
        .env("KAR_INVENTORY_BATCH", "1")
        .env("ACCOUNT_CASE", "skills_partial")
        .env("HOME", root);
    for provider in kar::runtimes()
        .into_iter()
        .chain([kar::runtime("morph").unwrap()])
    {
        let binary = if matches!(provider.id.as_str(), "claude" | "codex") {
            root.join(&provider.id)
        } else {
            root.join("absent-provider")
        };
        cmd.env(
            format!("KAR_{}_BIN", provider.id.to_ascii_uppercase()),
            binary,
        );
    }
    if array {
        cmd.arg("--json");
    }
    if stall {
        cmd.env("KAR_STALL_INVENTORY", "1");
    }
    cmd.output().unwrap()
}

fn rows(out: &Output) -> Vec<Value> {
    std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn row<'a>(rows: &'a [Value], id: &str) -> &'a Value {
    rows.iter().find(|v| v["id"] == id).unwrap()
}

pub(super) fn exercise() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    for id in ["codex", "claude"] {
        fs::copy(std::env::current_exe().unwrap(), cwd.join(id)).unwrap();
    }
    for kind in ["skills", "commands", "mcp-servers", "tools"] {
        let all = query(&cwd, kind, "all", false, false);
        assert!(
            all.status.success(),
            "{}",
            String::from_utf8_lossy(&all.stderr)
        );
        let reports = rows(&all);
        assert_eq!(reports.len(), kar::runtimes().len() + 2);
        assert_eq!(row(&reports, "pi")["installation"]["kind"], "missing");
        for id in ["claude", "codex"] {
            let single = query(&cwd, kind, id, false, false);
            assert!(single.status.success());
            let single_array = query(&cwd, kind, id, false, true);
            assert!(single_array.status.success());
            let array: Vec<Value> = serde_json::from_slice(&single_array.stdout).unwrap();
            assert_eq!(array.len(), 1);
            assert_eq!(array[0]["id"], id);
            assert_eq!(
                array[0]["inventory"]["kind"],
                row(&reports, id)["inventory"]["kind"]
            );
            let mut want = serde_json::from_slice::<Value>(&single.stdout).unwrap();
            let mut actual = row(&reports, id)["inventory"].clone();
            if id == "codex" && kind == "commands" {
                assert_eq!(actual["kind"], "unsupported");
            } else {
                assert_eq!(actual["kind"], "ok", "{actual}");
                assert_eq!(actual["scope"]["cwd"], json!(cwd));
                assert!(!actual["items"].as_array().unwrap().is_empty());
            }
            for value in [&mut want, &mut actual] {
                value.as_object_mut().unwrap().remove("observedAt");
            }
            assert_eq!(actual, want);
        }
        assert_ne!(
            row(&reports, "codex")["inventory"]["items"],
            row(&reports, "claude")["inventory"]["items"]
        );
        let delayed = query(&cwd, kind, "all", true, true);
        assert_eq!(delayed.status.code(), Some(1));
        let partial: Vec<Value> = serde_json::from_slice(&delayed.stdout).unwrap();
        assert_eq!(row(&partial, "pi").get("inventory"), Some(&Value::Null));
        assert_eq!(partial.len(), reports.len());
        assert!(
            row(&partial, "claude")["error"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
        assert_eq!(
            row(&partial, "codex")["inventory"]["kind"],
            row(&reports, "codex")["inventory"]["kind"]
        );
        assert_eq!(row(&partial, "pi")["installation"]["kind"], "missing");
    }
}
