use super::*;
use native::{run, text};

fn field<'a>(q: &'a Value, k: &str) -> Result<&'a str> {
    q[k].as_str()
        .filter(|v| !v.is_empty() && v.len() <= 8192 && !v.contains('\0'))
        .ok_or_else(|| Error::Refused(format!("invalid {k}")))
}
fn choice<'a>(q: &'a Value, k: &str, allowed: &[&str]) -> Result<&'a str> {
    let v = field(q, k)?;
    if allowed.contains(&v) {
        Ok(v)
    } else {
        Err(Error::Refused(format!("invalid {k}")))
    }
}
fn coordinate(q: &Value, k: &str, max: f64) -> Result<String> {
    q[k].as_f64()
        .filter(|v| v.is_finite() && v.abs() <= max)
        .map(|v| v.to_string())
        .ok_or_else(|| Error::Refused(format!("invalid {k}")))
}
fn app(q: &Value) -> Result<&str> {
    let id = field(q, "appId")?;
    if id.starts_with('-')
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(Error::Refused("invalid application identifier".into()));
    }
    Ok(id)
}
fn unsupported() -> Error {
    Error::Unsupported("this operation is unavailable on this device platform".into())
}
async fn ios(id: &str, op: &str, args: &[&str]) -> Result<String> {
    let mut v = vec!["simctl", op, id];
    v.extend_from_slice(args);
    text("xcrun", &v).await
}
async fn adb(id: &str, args: &[&str]) -> Result<String> {
    let mut v = vec!["-s", id];
    v.extend_from_slice(args);
    text("adb", &v).await
}
pub(super) fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}
async fn shell(id: &str, args: &[&str]) -> Result<String> {
    let quoted: Vec<_> = args.iter().map(|w| quote(w)).collect();
    let mut words = vec!["shell"];
    words.extend(quoted.iter().map(String::as_str));
    adb(id, &words).await
}
fn size(q: &Value) -> Result<usize> {
    let sizes = ["small", "default", "large", "extra-large"];
    sizes
        .iter()
        .position(|s| q["value"] == *s)
        .ok_or_else(|| Error::Refused("invalid text size".into()))
}
const IOS_SIZE: [&str; 4] = ["small", "large", "extra-extra-large", "accessibility-large"];
const SIZE: [&str; 4] = ["small", "default", "large", "extra-large"];
async fn helper(name: &str, purpose: &str) -> Result<String> {
    let path = if remote::active() {
        text("printenv", &[name]).await.ok()
    } else {
        std::env::var(name).ok()
    };
    path.filter(|p| Path::new(p).is_absolute())
        .ok_or_else(|| Error::Unsupported(format!("the simulator {purpose} helper is unavailable")))
}
fn ax_toggle(k: &str) -> Result<&'static str> {
    match k {
        "reduceMotion" => Ok("reduce-motion"),
        "reduceTransparency" => Ok("reduce-transparency"),
        "showBorders" => Ok("show-borders"),
        "voiceOver" => Ok("voiceover"),
        _ => Err(unsupported()),
    }
}
pub(super) async fn apply(d: &Device, q: &Value) -> Result<()> {
    if !d.booted {
        return Err(Error::Refused(
            "boot the device before applying an action".into(),
        ));
    }
    let op = field(q, "type")?;
    if d.platform == "android" {
        return android(&d.id, op, q).await;
    }
    let id = &d.id;
    match op {
        "setAppearance" => {
            ios(
                id,
                "ui",
                &["appearance", choice(q, "value", &["light", "dark"])?],
            )
            .await?;
        }
        "setTextSize" => {
            ios(id, "ui", &["content_size", IOS_SIZE[size(q)?]]).await?;
        }
        "setToggle" => {
            let b = q["value"]
                .as_bool()
                .ok_or_else(|| Error::Refused("invalid toggle".into()))?;
            let setting = field(q, "setting")?;
            if setting == "increaseContrast" {
                ios(
                    id,
                    "ui",
                    &["increase_contrast", if b { "enabled" } else { "disabled" }],
                )
                .await?;
            } else {
                ios(
                    id,
                    "spawn",
                    &[
                        &helper("KAR_DEVICE_IOS_AX_HELPER", "accessibility").await?,
                        "set",
                        ax_toggle(setting)?,
                        if b { "on" } else { "off" },
                    ],
                )
                .await?;
            }
        }
        "setLiquidGlass" | "setColorFilter" => {
            let (k, v) = if op == "setLiquidGlass" {
                ("liquid-glass", choice(q, "value", &["clear", "tinted"])?)
            } else {
                (
                    "color-filter",
                    choice(
                        q,
                        "value",
                        &["none", "grayscale", "red-green", "green-red", "blue-yellow"],
                    )?,
                )
            };
            ios(
                id,
                "spawn",
                &[
                    &helper("KAR_DEVICE_IOS_AX_HELPER", "accessibility").await?,
                    "set",
                    k,
                    v,
                ],
            )
            .await?;
        }
        "setLocation" => {
            ios(
                id,
                "location",
                &[
                    "set",
                    &format!(
                        "{},{}",
                        coordinate(q, "latitude", 90.)?,
                        coordinate(q, "longitude", 180.)?
                    ),
                ],
            )
            .await?;
        }
        "clearLocation" => {
            ios(id, "location", &["clear"]).await?;
        }
        "setPermission" => {
            let verb = choice(q, "decision", &["grant", "revoke", "reset"])?;
            let permission = field(q, "permission")?;
            if permission == "notifications" {
                let program = helper(
                    "KAR_DEVICE_IOS_PERMISSION_HELPER",
                    "notification permission",
                )
                .await?;
                text(
                    &program,
                    &["permissions", verb, permission, app(q)?, "-d", id],
                )
                .await?;
            } else {
                choice(
                    q,
                    "permission",
                    &[
                        "camera",
                        "microphone",
                        "photos",
                        "contacts",
                        "calendar",
                        "reminders",
                        "location",
                        "motion",
                        "media-library",
                        "faceid",
                    ],
                )?;
                ios(id, "privacy", &[verb, permission, app(q)?]).await?;
            }
        }
        "openUrl" => {
            let url = field(q, "url")?;
            url::Url::parse(url).map_err(|_| Error::Refused("invalid URL".into()))?;
            ios(id, "openurl", &[url]).await?;
        }
        "launchApp" => {
            ios(id, "launch", &[app(q)?]).await?;
        }
        "terminateApp" => {
            ios(id, "terminate", &[app(q)?]).await?;
        }
        "sendPush" => {
            let payload = match &q["payload"] {
                v @ Value::String(_) => json!({"aps":{"alert":v}}),
                v @ Value::Object(_) => v.clone(),
                _ => return Err(Error::Refused("invalid push payload".into())),
            };
            run(
                "xcrun",
                &["simctl", "push", id, app(q)?, "-"],
                Some(serde_json::to_vec(&payload)?),
            )
            .await?;
        }
        _ => return Err(unsupported()),
    }
    Ok(())
}
async fn android(id: &str, op: &str, q: &Value) -> Result<()> {
    match op {
        "setAppearance" => {
            let v = choice(q, "value", &["light", "dark"])?;
            shell(
                id,
                &[
                    "cmd",
                    "uimode",
                    "night",
                    if v == "dark" { "yes" } else { "no" },
                ],
            )
            .await?;
        }
        "setTextSize" => {
            shell(
                id,
                &[
                    "settings",
                    "put",
                    "system",
                    "font_scale",
                    ["0.85", "1.0", "1.15", "1.3"][size(q)?],
                ],
            )
            .await?;
        }
        "setToggle" => {
            let b = q["value"]
                .as_bool()
                .ok_or_else(|| Error::Refused("invalid toggle".into()))?;
            match field(q, "setting")? {
                "networkEnabled" => {
                    for service in ["wifi", "data"] {
                        shell(id, &["svc", service, if b { "enable" } else { "disable" }]).await?;
                    }
                }
                "reduceMotion" => {
                    for k in [
                        "animator_duration_scale",
                        "transition_animation_scale",
                        "window_animation_scale",
                    ] {
                        shell(
                            id,
                            &["settings", "put", "global", k, if b { "0" } else { "1" }],
                        )
                        .await?;
                    }
                }
                _ => return Err(unsupported()),
            }
        }
        "setLocation" => {
            adb(
                id,
                &[
                    "emu",
                    "geo",
                    "fix",
                    &coordinate(q, "longitude", 180.)?,
                    &coordinate(q, "latitude", 90.)?,
                ],
            )
            .await?;
        }
        "clearLocation" => {}
        "setOrientation" => {
            let modes = [
                "portrait",
                "landscape_left",
                "portrait_upside_down",
                "landscape_right",
            ];
            let mut n = modes
                .iter()
                .position(|v| q["value"] == *v)
                .ok_or_else(|| Error::Refused("invalid orientation".into()))?;
            let natural = shell(id, &["wm", "size"]).await.unwrap_or_default();
            let dims = natural
                .lines()
                .filter_map(|s| s.split_once("size: ")?.1.split_once('x'))
                .next_back();
            if dims.and_then(|(w, h)| Some(w.parse::<u32>().ok()? > h.parse::<u32>().ok()?))
                == Some(true)
            {
                n = (n + 3) % 4;
            }
            if id.starts_with("emulator-") {
                shell(
                    id,
                    &["settings", "put", "system", "accelerometer_rotation", "1"],
                )
                .await?;
                shell(id, &["cmd", "window", "user-rotation", "free"]).await?;
                adb(
                    id,
                    &[
                        "emu",
                        "sensor",
                        "set",
                        "acceleration",
                        ["0:9.81:0", "9.81:0:0", "0:-9.81:0", "-9.81:0:0"][n],
                    ],
                )
                .await?;
            } else {
                shell(
                    id,
                    &["cmd", "window", "user-rotation", "lock", &n.to_string()],
                )
                .await?;
            }
        }
        "setPermission" => {
            let id_app = app(q)?;
            let verb = choice(q, "decision", &["grant", "revoke", "reset"])?;
            let perms: &[&str] = match field(q, "permission")? {
                "camera" => &["CAMERA"],
                "microphone" => &["RECORD_AUDIO"],
                "photos" => &["READ_MEDIA_IMAGES", "READ_EXTERNAL_STORAGE"],
                "contacts" => &["READ_CONTACTS", "WRITE_CONTACTS"],
                "calendar" => &["READ_CALENDAR", "WRITE_CALENDAR"],
                "location" => &["ACCESS_FINE_LOCATION", "ACCESS_COARSE_LOCATION"],
                "notifications" => &["POST_NOTIFICATIONS"],
                "motion" => &["ACTIVITY_RECOGNITION"],
                _ => return Err(unsupported()),
            };
            let mut success = false;
            let mut last = None;
            for perm in perms {
                match shell(
                    id,
                    &[
                        "pm",
                        if verb == "grant" { "grant" } else { "revoke" },
                        id_app,
                        &format!("android.permission.{perm}"),
                    ],
                )
                .await
                {
                    Ok(_) => success = true,
                    Err(e) => last = Some(e),
                }
            }
            if !success {
                return Err(last.unwrap_or_else(unsupported));
            }
        }
        "openUrl" => {
            let url = field(q, "url")?;
            url::Url::parse(url).map_err(|_| Error::Refused("invalid URL".into()))?;
            shell(
                id,
                &["am", "start", "-a", "android.intent.action.VIEW", "-d", url],
            )
            .await?;
        }
        "launchApp" => {
            shell(
                id,
                &[
                    "monkey",
                    "-p",
                    app(q)?,
                    "-c",
                    "android.intent.category.LAUNCHER",
                    "1",
                ],
            )
            .await?;
        }
        "terminateApp" => {
            shell(id, &["am", "force-stop", app(q)?]).await?;
        }
        _ => return Err(unsupported()),
    }
    Ok(())
}
pub(super) async fn detail(d: &Device) -> Result<Value> {
    let id = &d.id;
    let mut settings = json!({});
    let mut foreground = Value::Null;
    if d.platform == "ios" {
        let (look, size, contrast) = tokio::join!(
            ios(id, "ui", &["appearance"]),
            ios(id, "ui", &["content_size"]),
            ios(id, "ui", &["increase_contrast"])
        );
        if let Ok(v) = look
            && matches!(v.as_str(), "light" | "dark")
        {
            settings["appearance"] = json!(v);
        }
        if let Ok(v) = size {
            let category = if let Some(i) = IOS_SIZE.iter().position(|k| *k == v) {
                SIZE[i]
            } else if v.starts_with("accessibility") {
                "extra-large"
            } else if matches!(v.as_str(), "extra-small" | "medium") {
                "small"
            } else if v.contains("extra") {
                "large"
            } else {
                "default"
            };
            settings["textSize"] = json!(category);
        }
        if let Ok(v) = contrast {
            settings["increaseContrast"] = json!(v == "enabled");
        }
        if let Ok(helper) = helper("KAR_DEVICE_IOS_AX_HELPER", "accessibility").await
            && let Ok(v) = ios(id, "spawn", &[&helper, "status"]).await
            && let Ok(ax) = serde_json::from_str::<Value>(&v)
        {
            for (key, name) in [
                ("reduceMotion", "reduce-motion"),
                ("reduceTransparency", "reduce-transparency"),
                ("showBorders", "show-borders"),
                ("voiceOver", "voiceover"),
            ] {
                if matches!(ax[name].as_str(), Some("on" | "off")) {
                    settings[key] = json!(ax[name] == "on");
                }
            }
            for (key, name) in [
                ("colorFilter", "color-filter"),
                ("liquidGlass", "liquid-glass"),
            ] {
                if ax[name].is_string() {
                    settings[key] = ax[name].clone();
                }
            }
        }
    } else {
        let (mode, scale, motion, wifi, window) = tokio::join!(
            shell(id, &["cmd", "uimode", "night"]),
            shell(id, &["settings", "get", "system", "font_scale"]),
            shell(
                id,
                &["settings", "get", "global", "animator_duration_scale"]
            ),
            shell(id, &["settings", "get", "global", "wifi_on"]),
            shell(id, &["dumpsys", "window"])
        );
        if let Ok(v) = mode {
            if v.contains("yes") {
                settings["appearance"] = json!("dark");
            } else if v.contains("no") {
                settings["appearance"] = json!("light");
            }
        }
        if let Some(n) = scale.ok().and_then(|v| v.parse::<f64>().ok()) {
            settings["textSize"] = json!(if n <= 0.9 {
                "small"
            } else if n >= 1.25 {
                "extra-large"
            } else if n >= 1.1 {
                "large"
            } else {
                "default"
            });
        }
        if let Some(n) = motion.ok().and_then(|v| v.parse::<f64>().ok()) {
            settings["reduceMotion"] = json!(n == 0.0);
        }
        if let Ok(v) = wifi
            && matches!(v.as_str(), "0" | "1")
        {
            settings["networkEnabled"] = json!(v == "1");
        }
        if let Ok(v) = window
            && let Some(pkg) = v
                .lines()
                .filter(|l| l.contains("mCurrentFocus=") || l.contains("mFocusedApp="))
                .find_map(|l| {
                    l.split_whitespace()
                        .find_map(|s| s.split_once('/').map(|v| v.0))
                })
        {
            foreground = json!({"id":pkg});
        }
    }
    Ok(
        json!({"host_id":d.host_id,"device_id":id,"settings":settings,"foreground_app":foreground,"read_at":jiff::Timestamp::now().to_string()}),
    )
}
