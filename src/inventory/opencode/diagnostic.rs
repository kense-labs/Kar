use crate::{SessionOptions, installation::Probe};
use std::io::SeekFrom;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};

/// Drop oversized stderr as a whole. Truncating before redaction could expose part of a credential.
pub(super) async fn excerpt(f: &std::fs::File, probe: &Probe) -> Option<String> {
    let mut f = File::from_std(f.try_clone().ok()?);
    f.seek(SeekFrom::Start(0)).await.ok()?;
    const LIMIT: u64 = 128 * 1024;
    let mut bytes = Vec::new();
    f.take(LIMIT + 1).read_to_end(&mut bytes).await.ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    let raw = std::str::from_utf8(&bytes).ok()?;
    let mut opts = SessionOptions::new(&probe.cwd);
    opts.env.clone_from(&probe.env);
    let mut secrets = crate::options::session_secrets(&opts);
    for (key, value) in &probe.env {
        let key = key.to_string_lossy().to_ascii_uppercase();
        let words = key.split(|c: char| !c.is_ascii_alphanumeric());
        if words.into_iter().any(|w| {
            matches!(
                w,
                "KEY"
                    | "APIKEY"
                    | "TOKEN"
                    | "PASSWORD"
                    | "PASSWD"
                    | "PASSPHRASE"
                    | "CREDENTIAL"
                    | "CREDENTIALS"
                    | "COOKIE"
                    | "SECRET"
                    | "AUTHORIZATION"
            )
        }) && let Some(value) = value.to_str().filter(|v| !v.is_empty())
        {
            secrets.push(value.to_owned());
            if let Some((_, auth)) = value.split_once(' ')
                && key.ends_with("AUTHORIZATION")
                && !auth.is_empty()
            {
                secrets.push(auth.to_owned());
            }
        }
    }
    crate::process::diagnostic::excerpt(raw, &secrets)
}
