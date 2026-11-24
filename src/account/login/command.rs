use super::{
    AccountUnsupported, LoginEvent, LoginFailure, LoginInteraction, LoginOptions, LoginResult,
    Reader, bounded, confirm_external, emit,
};
use crate::{installation::Probe, process::OwnedProcess};
use regex::Regex;
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    process::Command,
    time::Instant,
};

pub(super) fn no_browser() -> LoginResult {
    LoginResult::Unsupported {
        reason: AccountUnsupported::BrowserSuppressionUnavailable,
        detail: "The native browser launcher cannot be suppressed in this environment".into(),
    }
}
pub(super) fn inert_browser() -> Option<PathBuf> {
    #[cfg(windows)]
    let paths = std::env::var_os("SystemRoot")
        .map(|d| PathBuf::from(d).join("System32/hostname.exe"))
        .into_iter();
    #[cfg(not(windows))]
    let paths = ["/usr/bin/true", "/bin/true"]
        .map(PathBuf::from)
        .into_iter();
    paths.into_iter().find(|p| p.is_file())
}

fn suppress(reader: Reader, probe: &Probe, cmd: &mut Command) -> Option<Option<tempfile::TempDir>> {
    match reader {
        Reader::Cursor => {
            cmd.env("NO_OPEN_BROWSER", "1");
        }
        Reader::Qoder => {
            cmd.env("BROWSER", "www-browser");
        }
        Reader::Grok if !cfg!(windows) => {
            cmd.env("BROWSER", inert_browser()?);
        }
        Reader::Kimi => {
            let name = if cfg!(target_os = "macos") {
                "open"
            } else if cfg!(target_os = "linux") {
                "xdg-open"
            } else if cfg!(windows) {
                "powershell.exe"
            } else {
                return None;
            };
            let dir = tempfile::Builder::new()
                .prefix("kar-kimi-browser-")
                .tempdir()
                .ok()?;
            let cwd = std::fs::canonicalize(&probe.cwd).ok()?;
            let root = std::fs::canonicalize(dir.path()).ok()?;
            if cwd.parent().is_some() && root.starts_with(&cwd) {
                return None;
            }
            std::fs::copy(inert_browser()?, root.join(name)).ok()?;
            let path = std::env::join_paths(
                std::iter::once(root).chain(
                    probe
                        .get("PATH")
                        .into_iter()
                        .flat_map(std::env::split_paths),
                ),
            )
            .ok()?;
            cmd.env("PATH", path);
            return Some(Some(dir));
        }
        _ => return None,
    }
    Some(None)
}

pub(super) async fn login(
    reader: Reader,
    mut cmd: Command,
    probe: &Probe,
    path: &Path,
    options: &LoginOptions,
    host: &mut impl LoginInteraction,
    end: Instant,
) -> LoginResult {
    let _helper = if options.open_browser == Some(false) {
        let Some(h) = suppress(reader, probe, &mut cmd) else {
            return no_browser();
        };
        h
    } else {
        None
    };
    if let Some(region) = options.kimi_region {
        cmd.args(["--region", region.arg()]);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = OwnedProcess::spawn(cmd) else {
        return LoginResult::failed(LoginFailure::ProcessFailed, "Native login could not start");
    };
    let mut out = Lines::new(child.inner().stdout.take().expect("piped stdout"));
    let mut err = Lines::new(child.inner().stderr.take().expect("piped stderr"));
    let mut state = State::default();
    let mut report_cursor_key = reader == Reader::Cursor
        && probe
            .get("CURSOR_API_KEY")
            .is_some_and(|key| !key.is_empty());
    let read = async {
        while !(out.closed && err.closed) {
            let (line, stderr) = tokio::select! {
                l=out.next(), if !out.closed => (l,false),
                l=err.next(), if !err.closed => (l,true),
            };
            if let Some(line) = line.map_err(|_| {
                LoginResult::failed(
                    LoginFailure::ProcessFailed,
                    "Incomplete native sign-in output",
                )
            })? {
                state.observe(reader, &line, stderr, options, host)?;
                if report_cursor_key && state.url.is_some() {
                    report_cursor_key = false;
                    emit(host, &options.cancellation, LoginEvent::Info {
                        message: "Cursor will use CURSOR_API_KEY while that environment variable is set.".into(),
                    })?;
                }
            }
        }
        let exit = child.wait().await.map_err(|_| {
            LoginResult::failed(
                LoginFailure::ProcessFailed,
                "Could not read the native sign-in result",
            )
        })?;
        if reader == Reader::Grok {
            state.success = exit.success();
        }
        if state.success {
            Ok(())
        } else {
            Err(LoginResult::failed(
                if state.rejected {
                    LoginFailure::Rejected
                } else {
                    LoginFailure::ProcessFailed
                },
                "The native command ended without reporting a successful login",
            ))
        }
    };
    let result = bounded(read, options, end).await;
    child.stop().await;
    if state.success {
        return confirm_external(reader, probe, path, options, end).await;
    }
    match result {
        Err(r) | Ok(Err(r)) => r,
        Ok(Ok(())) => LoginResult::success(),
    }
}

struct Lines<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
    closed: bool,
}
impl<R: AsyncRead + Unpin> Lines<R> {
    fn new(r: R) -> Self {
        Self {
            reader: BufReader::new(r),
            buf: vec![],
            closed: false,
        }
    }
    async fn next(&mut self) -> io::Result<Option<String>> {
        const MAX: usize = 1024 * 1024;
        let n = (&mut self.reader)
            .take((MAX + 1 - self.buf.len()) as u64)
            .read_until(b'\n', &mut self.buf)
            .await?;
        if self.buf.len() > MAX {
            return Err(io::Error::other("Long sign-in output line"));
        }
        if n == 0 {
            self.closed = true;
            if self.buf.is_empty() {
                return Ok(None);
            }
        }
        static ESCAPES: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap()
        });
        let line = ESCAPES
            .replace_all(&String::from_utf8_lossy(&self.buf), "")
            .trim()
            .to_owned();
        self.buf.clear();
        Ok(Some(line))
    }
}

#[derive(Default)]
struct State {
    success: bool,
    rejected: bool,
    url: Option<String>,
    emitted: BTreeSet<String>,
    cursor_url_next: bool,
}
impl State {
    fn cursor_event(&mut self, line: &str, stderr: bool) -> Option<LoginEvent> {
        if stderr {
            self.rejected |=
                line.starts_with("Login failed or timed out.") || line.starts_with("Login error:");
            return None;
        }
        self.success |=
            line == "\u{2713} Login successful!" || line.starts_with("\u{2713} Logged in as ");
        let expect_url = std::mem::replace(
            &mut self.cursor_url_next,
            line == "If your browser didn't open, use this link:",
        );
        let url = line
            .strip_prefix("Open a browser and navigate to this link: ")
            .or_else(|| expect_url.then_some(line));
        if let Some(url) = url.filter(|s| http_link(s)) {
            self.url = Some(url.to_owned());
            return Some(LoginEvent::AuthUrl {
                url: url.into(),
                instructions: Some("Open this URL to sign in to Cursor.".into()),
            });
        }
        let code = line.strip_prefix("Matching code: ")?;
        let digits = |v: &str| v.bytes().all(|b| b.is_ascii_digit());
        let valid = code.len() == 6 && digits(code)
            || code
                .split_once(' ')
                .is_some_and(|(a, b)| a.len() == 3 && b.len() == 3 && digits(a) && digits(b));
        (self.url.is_some() && valid).then(|| LoginEvent::Info {
            message: format!("Check that your browser shows the same matching code: {code}"),
        })
    }

    fn observe(
        &mut self,
        reader: Reader,
        line: &str,
        stderr: bool,
        options: &LoginOptions,
        host: &mut impl LoginInteraction,
    ) -> std::result::Result<(), LoginResult> {
        let event = match reader {
            Reader::Cursor => match self.cursor_event(line, stderr) {
                Some(event) => event,
                None => return Ok(()),
            },
            Reader::Qoder => {
                self.success |= line.starts_with("Login successful!")
                    || line.starts_with("Already logged in as ");
                self.rejected |= line.starts_with("Login failed:");
                if !http_link(line) {
                    return Ok(());
                }
                LoginEvent::AuthUrl {
                    url: line.into(),
                    instructions: Some("Open this URL to sign in to Qoder.".into()),
                }
            }
            Reader::Kimi if stderr => {
                self.success |= line.starts_with("Logged in to ") && line.ends_with('.');
                self.rejected |= [
                    "Login failed.",
                    "Login failed:",
                    "Login cancelled.",
                    "Login cancelled:",
                ]
                .iter()
                .any(|s| line.starts_with(s));
                if let Some(url) = line.strip_prefix("Opening browser for Kimi device login: ") {
                    self.url = http_link(url).then(|| url.to_owned());
                }
                let Some(code) = line
                    .strip_prefix(
                        "If the browser did not open, paste the URL above and enter code: ",
                    )
                    .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
                else {
                    return Ok(());
                };
                let Some(url) = self.url.clone() else {
                    return Ok(());
                };
                LoginEvent::DeviceCode {
                    user_code: code.into(),
                    verification_uri: url,
                }
            }
            Reader::Grok => {
                if !line.starts_with("https://accounts.x.ai/oauth2/device?") || !http_link(line) {
                    self.rejected |= !line.is_empty();
                    return Ok(());
                }
                if !self.emitted.is_empty() {
                    return Ok(());
                }
                let code = reqwest::Url::parse(line).ok().and_then(|u| {
                    u.query_pairs().find_map(|(k, v)| {
                        (k == "user_code" && !v.is_empty()).then(|| v.into_owned())
                    })
                });
                let Some(code) = code else { return Ok(()) };
                LoginEvent::DeviceCode {
                    user_code: code,
                    verification_uri: line.into(),
                }
            }
            _ => return Ok(()),
        };
        let key = match &event {
            LoginEvent::AuthUrl { url, .. } => url.clone(),
            LoginEvent::DeviceCode {
                user_code,
                verification_uri,
            } => format!("{verification_uri}\n{user_code}"),
            LoginEvent::Info { message } => message.clone(),
        };
        if self.emitted.len() >= 256 && !self.emitted.contains(&key) {
            return Err(LoginResult::failed(
                LoginFailure::ProcessFailed,
                "Too many sign-in handoffs",
            ));
        }
        if self.emitted.insert(key) {
            emit(host, &options.cancellation, event)?;
        }
        Ok(())
    }
}
fn http_link(s: &str) -> bool {
    !s.chars().any(char::is_whitespace)
        && reqwest::Url::parse(s).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
}
