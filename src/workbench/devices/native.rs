use super::*;
use std::{ffi::OsString, path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

fn tool(program: &str) -> PathBuf {
    let key = format!(
        "KAR_DEVICE_{}_BIN",
        program.replace('-', "_").to_ascii_uppercase()
    );
    if let Some(path) = std::env::var_os(key) {
        return path.into();
    }
    if matches!(program, "adb" | "emulator") {
        let sdk = std::env::var_os("ANDROID_HOME")
            .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Android/sdk"))
            });
        if let Some(sdk) = sdk {
            let candidate = sdk.join(if program == "adb" {
                "platform-tools/adb"
            } else {
                "emulator/emulator"
            });
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    program.into()
}
pub(super) fn local_command(program: &str) -> Command {
    let mut c = Command::new(tool(program));
    c.env_remove("KAR_WORKBENCH_TOKEN")
        .env_remove("EXPO_TOKEN")
        .env_remove("AGENT_DEVICE_DAEMON_AUTH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}
pub(super) async fn run(program: &str, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let limit = if program == "adb" && args.ends_with(&["exec-out", "screencap", "-p"]) {
        32 * 1024 * 1024
    } else {
        crate::process::OUTPUT_LIMIT
    };
    run_limited(program, args, input, limit).await
}
async fn run_limited(
    program: &str,
    args: &[&str],
    input: Option<Vec<u8>>,
    limit: u64,
) -> Result<Vec<u8>> {
    let mut command = remote::command(program, args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut proc = crate::process::OwnedProcess::spawn(command)
        .map_err(|e| Error::Refused(format!("could not start {program}: {e}")))?;
    let stdin = proc.inner().stdin.take();
    let out = tokio::time::timeout(Duration::from_secs(90), async {
        let (_, out) = tokio::try_join!(
            async {
                if let (Some(mut pipe), Some(data)) = (stdin, input) {
                    pipe.write_all(&data).await?;
                }
                Ok::<(), std::io::Error>(())
            },
            proc.output_limited(limit)
        )?;
        Ok::<_, std::io::Error>(out)
    })
    .await
    .map_err(|_| Error::Refused(format!("{program} timed out")))??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .filter(|s| !s.trim().is_empty())
            .next_back()
            .unwrap_or("native operation failed");
        return Err(Error::Refused(format!(
            "{program} {}: {}",
            out.status,
            last.chars().take(300).collect::<String>()
        )));
    }
    Ok(out.stdout)
}
pub(super) async fn text(program: &str, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run(program, args, None).await?)
        .trim()
        .into())
}
pub(super) async fn ios_list() -> Result<Vec<Device>> {
    let v: Value = serde_json::from_slice(
        &run(
            "xcrun",
            &["simctl", "list", "devices", "available", "--json"],
            None,
        )
        .await?,
    )?;
    let mut found = vec![];
    for (runtime, rows) in v["devices"]
        .as_object()
        .ok_or_else(|| Error::Protocol("invalid simctl list".into()))?
    {
        let Some(version) = runtime.strip_prefix("com.apple.CoreSimulator.SimRuntime.iOS-") else {
            continue;
        };
        for row in rows.as_array().into_iter().flatten() {
            if row["isAvailable"] == false {
                continue;
            }
            let (Some(id), Some(name)) = (row["udid"].as_str(), row["name"].as_str()) else {
                continue;
            };
            if Uuid::parse_str(id).is_err() {
                continue;
            }
            found.push(Device::new(
                id,
                "ios",
                name,
                version.replace('-', "."),
                row["state"] == "Booted",
                false,
            ));
        }
    }
    Ok(found)
}
pub(super) async fn android_list() -> Result<Vec<Device>> {
    let listed = text("adb", &["devices", "-l"]).await?;
    let mut devices = vec![];
    let mut running = std::collections::HashSet::new();
    for row in listed.lines() {
        let mut fields = row.split_whitespace();
        let Some(serial) = fields.next() else {
            continue;
        };
        if fields.next() != Some("device") || serial.starts_with('-') {
            continue;
        }
        let model = fields
            .find_map(|f| f.strip_prefix("model:"))
            .unwrap_or(serial)
            .replace('_', " ");
        let emulated = serial.starts_with("emulator-");
        let avd = if emulated {
            text("adb", &["-s", serial, "emu", "avd", "name"])
                .await
                .ok()
                .and_then(|s| s.lines().next().map(str::to_string))
        } else {
            None
        };
        let label = avd.unwrap_or(model);
        if emulated {
            running.insert(label.clone());
        }
        let version = text(
            "adb",
            &["-s", serial, "shell", "getprop", "ro.build.version.release"],
        )
        .await
        .unwrap_or_default();
        devices.push(Device::new(
            serial, "android", &label, version, true, !emulated,
        ));
    }
    if let Ok(names) = text("emulator", &["-list-avds"]).await {
        for name in names
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('-') && !running.contains(*s))
        {
            devices.push(Device::new(
                &format!("avd:{name}"),
                "android",
                name,
                String::new(),
                false,
                false,
            ));
        }
    }
    Ok(devices)
}
pub(super) async fn boot(d: &Device) -> Result<String> {
    if d.booted {
        return Ok(d.id.clone());
    }
    if d.platform == "ios" {
        text("xcrun", &["simctl", "bootstatus", &d.id, "-b"]).await?;
        return Ok(d.id.clone());
    }
    let avd =
        d.id.strip_prefix("avd:")
            .ok_or_else(|| Error::Refused("not an installed Android AVD".into()))?;
    let mut cmd = remote::command("emulator", &["-avd", avd]);
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let serial = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Refused(format!("emulator exited {status}")));
            }
            if let Ok(list) = android_list().await
                && let Some(device) = list.into_iter().find(|d| d.booted && d.name == avd)
                && text(
                    "adb",
                    &["-s", &device.id, "shell", "getprop", "sys.boot_completed"],
                )
                .await
                .is_ok_and(|v| v == "1")
            {
                return Ok(device.id);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .map_err(|_| Error::Refused("Android emulator boot timed out".into()))??;
    // The session no longer owns the emulator process after boot; closing without shutdown leaves it available.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(serial)
}
pub(super) async fn shutdown(d: &Device) -> Result<()> {
    if d.physical {
        return Err(Error::Unsupported(
            "physical devices cannot be shut down through this service".into(),
        ));
    }
    if !d.booted {
        return Ok(());
    }
    if d.platform == "ios" {
        text("xcrun", &["simctl", "shutdown", &d.id]).await?;
    } else {
        text("adb", &["-s", &d.id, "emu", "kill"]).await?;
    }
    Ok(())
}
pub(super) async fn screenshot(d: &Device) -> Result<Vec<u8>> {
    let bytes = if d.platform == "ios" && remote::active() {
        // The PNG belongs to the simulator host. Return its bytes over SSH and remove the temporary file there.
        let script = r#"f=$(mktemp -t kar-device-screen) || exit 1
trap 'rm -f "$f"' EXIT
xcrun simctl io "$1" screenshot --type=png "$f" >/dev/null && cat "$f""#;
        run_limited(
            "sh",
            &["-c", script, "kar-device-screen", &d.id],
            None,
            32 * 1024 * 1024,
        )
        .await?
    } else if d.platform == "ios" {
        let tmp = tempfile::tempdir()?;
        let path: OsString = tmp.path().join("screen.png").into();
        text(
            "xcrun",
            &[
                "simctl",
                "io",
                &d.id,
                "screenshot",
                "--type=png",
                path.to_str()
                    .ok_or_else(|| Error::Refused("invalid screenshot path".into()))?,
            ],
        )
        .await?;
        let path = PathBuf::from(path);
        if std::fs::metadata(&path)?.len() > 32 * 1024 * 1024 {
            return Err(Error::Refused("device screenshot exceeds 32 MiB".into()));
        }
        std::fs::read(path)?
    } else {
        run("adb", &["-s", &d.id, "exec-out", "screencap", "-p"], None).await?
    };
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || !bytes.ends_with(b"IEND\xaeB\x60\x82") {
        return Err(Error::Protocol("device returned an incomplete PNG".into()));
    }
    Ok(bytes)
}
