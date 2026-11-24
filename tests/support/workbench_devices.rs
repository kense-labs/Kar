//! Device controls use the installed tools in live mode and a separate Rust argv fixture otherwise.
use super::workbench_server::{client, json, serve_with_env};
use serde_json::{Value, json};
use std::{fs, io::Write, path::PathBuf, time::Duration};
use uuid::Uuid;

const UDID: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
pub(super) fn peer(args: &[String]) {
    assert!(std::env::var_os("KAR_WORKBENCH_TOKEN").is_none());
    let temp = PathBuf::from(std::env::var_os("KAR_DEVICE_TEST_DIR").unwrap());
    let appearance = temp.join("appearance");
    let booted = temp.join("booted");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(temp.join("device-args"))
        .unwrap();
    writeln!(file, "{}", json!(args)).unwrap();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["simctl", "list", "devices", "available", "--json"] => println!(
            "{}",
            json!({"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-19-0":[{"udid":UDID,"name":"Kar iPhone","state":if booted.exists(){"Booted"}else{"Shutdown"},"isAvailable":true}]}})
        ),
        ["simctl", "bootstatus", UDID, "-b"] => fs::write(booted, "on").unwrap(),
        ["simctl", "shutdown", UDID] => {
            let _ = fs::remove_file(booted);
        }
        ["simctl", "ui", UDID, "appearance", v] => fs::write(appearance, v).unwrap(),
        ["simctl", "ui", UDID, "appearance"] => println!(
            "{}",
            fs::read_to_string(appearance).unwrap_or_else(|_| "light".into())
        ),
        ["simctl", "ui", UDID, "content_size"] => println!("large"),
        ["simctl", "ui", UDID, "increase_contrast"] => println!("disabled"),
        ["simctl", "io", UDID, "screenshot", "--type=png", p] => {
            use base64::Engine as _;
            fs::write(p,base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aVxkAAAAASUVORK5CYII=").unwrap()).unwrap();
        }
        ["devices", "-l"] => {
            println!("List of devices attached\nserial42 device product:fixture model:fixture")
        }
        ["-list-avds"] => {}
        [
            "-s",
            "serial42",
            "shell",
            "getprop",
            "ro.build.version.release",
        ] => println!("14"),
        ["-s", "serial42", "shell", ..] => {}
        ["-T", ..] => println!("Darwin\nios"),
        other => panic!("unexpected device command {other:?}"),
    }
}

#[tokio::main]
pub(super) async fn run(live: bool) {
    tokio::time::timeout(Duration::from_secs(240), scenario(live))
        .await
        .unwrap();
}
async fn scenario(live: bool) {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let work = temp.path().join("work");
    let native = temp.path().join("native");
    for p in [&work, &native] {
        fs::create_dir(p).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let vars: Vec<(&str, &std::ffi::OsStr)> = if live {
        vec![]
    } else {
        vec![
            ("KAR_DEVICE_XCRUN_BIN", exe.as_os_str()),
            ("KAR_DEVICE_ADB_BIN", exe.as_os_str()),
            ("KAR_DEVICE_EMULATOR_BIN", exe.as_os_str()),
            ("KAR_DEVICE_SSH_BIN", exe.as_os_str()),
            ("KAR_DEVICE_TEST_DIR", native.as_os_str()),
        ]
    };
    let (mut server, url) = serve_with_env(&state, &work, &native, &vars).await;
    let a = client();
    let b = client();
    json(
        a.post(format!("{url}/devices/configure"))
            .json(&json!({"enabled":true,"onboarding_completed":true})),
    )
    .await;
    let listed = json(a.get(format!("{url}/devices?inspect_only=true"))).await;
    let id = if live {
        std::env::var("KAR_DEVICE_LIVE_ID").expect("provide a dedicated booted iOS simulator ID")
    } else {
        UDID.into()
    };
    let found = listed["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == id)
        .expect("device absent");
    assert_eq!(found["platform"], "ios");
    if live {
        assert_eq!(
            found["booted"], true,
            "start the dedicated simulator before live acceptance"
        );
    }
    if !live {
        let probe = json(
            a.post(format!("{url}/devices/test-host"))
                .json(&json!({"id":"lab","label":"Lab","target":"user@host"})),
        )
        .await;
        assert_eq!(probe["platforms"][0]["available"], true);
    }
    let project = json(
        a.post(format!("{url}/projects"))
            .json(&json!({"root":work,"name":"device"})),
    )
    .await;
    let p = project["id"].as_str().unwrap();
    let mut chats = vec![];
    for _ in 0..2 {
        let chat = Uuid::new_v4();
        json(
            a.post(format!("{url}/projects/{p}/chats"))
                .json(&json!({"id":chat,"runtime":"codex"})),
        )
        .await;
        let open = json(
            a.post(format!("{url}/devices/open"))
                .json(&json!({"project":p,"chat":chat,"platform":"ios","device_id":id})),
        )
        .await;
        assert_eq!(open["chat"], chat.to_string());
        chats.push(chat);
    }
    let detail = format!("{url}/devices/detail?device_id={id}");
    let before = json(a.get(&detail)).await;
    let value = if live {
        before["settings"]["appearance"]
            .as_str()
            .expect("simulator appearance unavailable")
    } else {
        "dark"
    };
    let applied = json(
        a.post(format!("{url}/devices/action"))
            .json(&json!({"device_id":id,"type":"setAppearance","value":value})),
    )
    .await;
    assert_eq!(applied["settings"]["appearance"], value);
    let shot = b
        .get(format!("{url}/devices/screenshot?device_id={id}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(shot.starts_with(b"\x89PNG\r\n\x1a\n") && shot.len() > 40);
    let mut events = b.get(format!("{url}/devices/events")).send().await.unwrap();
    assert!(String::from_utf8_lossy(&events.chunk().await.unwrap().unwrap()).contains("devices"));
    json(
        a.post(format!("{url}/devices/close"))
            .json(&json!({"chat":chats[0],"device_id":id})),
    )
    .await;
    let state1 = json(b.get(format!("{url}/devices"))).await;
    assert_eq!(state1["sessions"].as_array().unwrap().len(), 1);
    if live {
        json(
            a.post(format!("{url}/devices/close"))
                .json(&json!({"chat":chats[1],"device_id":id})),
        )
        .await;
    } else {
        json(
            a.post(format!("{url}/devices/shutdown"))
                .json(&json!({"device_id":id})),
        )
        .await;
        let done = json(a.get(format!("{url}/devices"))).await;
        assert!(done["sessions"].as_array().unwrap().is_empty());
        assert!(!native.join("booted").exists());
        let device_url = "https://example.test/a?q=x&z='y'";
        json(
            a.post(format!("{url}/devices/action"))
                .json(&json!({"device_id":"serial42","type":"openUrl","url":device_url})),
        )
        .await;
        let args = fs::read_to_string(native.join("device-args")).unwrap();
        let action = args
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|line| line[3] == "am")
            .unwrap();
        assert_eq!(
            action.as_array().unwrap().last().unwrap(),
            "'https://example.test/a?q=x&z='\\''y'\\'''"
        );
    }
    server.kill().await.unwrap();
    let (mut restarted, url) = serve_with_env(&state, &work, &native, &vars).await;
    assert_eq!(
        json(a.get(format!("{url}/devices"))).await["settings"]["onboarding_completed"],
        true
    );
    restarted.kill().await.unwrap();
}
