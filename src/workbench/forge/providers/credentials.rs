use super::*;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Consult the vendor's own authenticated CLI only for its known public API.
/// A configured API proxy uses its configured credential and never inherits
/// a login for the original service.
pub(in crate::workbench::forge) async fn resolve(
    c: &Context,
) -> Result<Option<String>, ForgeError> {
    if c.token.is_some() {
        return Ok(None);
    }
    let Some((kind, base)) = super::super::default_host(&c.host) else {
        return Ok(None);
    };
    if kind != c.kind || Url::parse(&base).ok().as_ref() != Some(&c.api_base) {
        return Ok(None);
    }
    let host = &c.host;
    let cafe = format!("https://{host}/api");
    let (binary, args) = match c.kind.as_str() {
        "cursor-origin" => ("origin", vec!["auth", "token"]),
        "tanstack-source" => ("src", vec!["auth", "token", "--hostname", host]),
        "gitcafe" => (
            "cafe",
            vec!["auth", "http", "credential", "--host", &cafe, "get"],
        ),
        "gitlab" => ("glab", vec!["auth", "token", "--hostname", host]),
        _ => return Ok(None),
    };
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .env_remove("KAR_WORKBENCH_TOKEN")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if c.kind == "gitcafe" {
        command.env("CAFE_NO_UPDATE_CHECK", "1");
    }
    if c.kind == "tanstack-source" {
        command.env("SRC_NO_TELEMETRY", "1").env_remove("SRC_TOKEN");
    }
    let mut child = match crate::process::OwnedProcess::spawn(command) {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(ForgeError::remote(
                502,
                "unable to read the forge CLI login",
            ));
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stdin = child.inner().stdin.take().unwrap();
        if c.kind == "gitcafe" {
            stdin
                .write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())
                .await?;
        }
        drop(stdin);
        let mut bytes = Vec::new();
        child
            .inner()
            .stdout
            .take()
            .unwrap()
            .take(16385)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 16384 {
            return Err(std::io::Error::other("oversized credential"));
        }
        let ok = child.wait().await?.success();
        Ok::<_, std::io::Error>((ok, bytes))
    })
    .await
    .map_err(|_| ForgeError::remote(504, "forge CLI login lookup timed out"))?
    .map_err(|_| ForgeError::remote(502, "could not read forge CLI login"))?;
    if !result.0 {
        return Ok(None);
    }
    let raw =
        std::str::from_utf8(&result.1).map_err(|_| invalid("forge CLI credential encoding"))?;
    let token = if c.kind == "gitcafe" {
        raw.lines().find_map(|line| line.strip_prefix("password="))
    } else {
        Some(raw.trim())
    };
    Ok(token
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned))
}
