use super::{
    onboarding::{self, Page},
    screen, *,
};
use crate::terminal::Terminal;
use portable_pty::{CommandBuilder, PtySize};
use std::time::Duration;
use tokio::time::{Instant, sleep_until};
const ROWS: u16 = 45;
const COLS: u16 = 120;
const STABLE: Duration = Duration::from_millis(600);
enum Startup {
    Login,
    Onboarding(Page),
    Trust(bool),
    Dialog,
    Prompt,
}
pub(super) struct Ui {
    terminal: Terminal,
    // Drop the terminal before removing its working directory, also on cancellation.
    _directory: tempfile::TempDir,
    parser: vt100::Parser,
    text: String,
    changed: Instant,
    end: Instant,
    pub onboarding_completed: bool,
    pub version: Option<String>,
}
impl Ui {
    pub fn open(probe: &Probe, path: &Path, end: Instant) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("kar-claude-limit-reset-")
            .tempdir()?;
        let mut cmd = CommandBuilder::new(path);
        cmd.env_clear();
        let native = super::super::native::command(Reader::Claude, probe, path);
        for (k, v) in native.as_std().get_envs() {
            if let Some(v) = v {
                cmd.env(k, v);
            }
        }
        for key in [
            "HOME",
            "USERPROFILE",
            "CLAUDE_CONFIG_DIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_DATA_HOME",
        ] {
            if let Some(p) = crate::account::profile::path(probe, key) {
                cmd.env(key, p);
            }
        }
        cmd.env("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "false");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLUMNS", COLS.to_string());
        cmd.env("LINES", ROWS.to_string());
        cmd.cwd(directory.path());
        let terminal = Terminal::open(
            cmd,
            PtySize {
                rows: ROWS,
                cols: COLS,
                ..Default::default()
            },
        )?;
        Ok(Self {
            terminal,
            _directory: directory,
            parser: vt100::Parser::new(ROWS, COLS, 0),
            text: String::new(),
            changed: Instant::now(),
            end,
            onboarding_completed: false,
            version: None,
        })
    }
    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let text = self
            .parser
            .screen()
            .rows(0, COLS)
            .collect::<Vec<_>>()
            .join("\n");
        if self.text != text {
            self.changed = Instant::now();
            self.text = text;
        }
    }
    fn drain(&mut self) -> bool {
        // Refuse to type while a backlog of redraws has not been processed.
        for _ in 0..64 {
            match self.terminal.output.try_recv() {
                Ok(Ok(bytes)) => self.process(&bytes),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    return self.terminal.exit.borrow().is_none();
                }
                _ => return false,
            }
        }
        false
    }
    fn alive(&self) -> bool {
        self.terminal.exit.borrow().is_none() && Instant::now() < self.end
    }
    fn key(&self, s: &str) -> bool {
        self.alive() && self.terminal.write(s.into()).is_ok()
    }
    pub fn remains(&self, time: Duration) -> bool {
        self.end.saturating_duration_since(Instant::now()) >= time
    }
    async fn seen<T>(
        &mut self,
        cutoff: Instant,
        quiet: Duration,
        test: impl Fn(&str) -> Option<T>,
    ) -> Option<T> {
        let deadline = cutoff.min(self.end);
        loop {
            if !self.drain() || Instant::now() >= deadline {
                return None;
            }
            let mut wait = deadline;
            if let Some(found) = test(&self.text) {
                let stable = self.changed + quiet;
                if Instant::now() >= stable {
                    return Some(found);
                }
                wait = wait.min(stable);
            }
            tokio::select! {
                chunk=self.terminal.output.recv()=>match chunk {Some(Ok(bytes))=>self.process(&bytes),_=>return None},
                _=self.terminal.exit.changed()=>return None,
                _=sleep_until(wait)=>{}
            }
        }
    }
    fn failed(&self) -> LimitResetResult {
        LimitResetResult::NotAttempted {
            reason: NotAttemptedReason::StartupFailed,
            startup: Some(StartupDiagnostic {
                screen: if self.terminal.exit.borrow().is_none() {
                    screen::diagnostic(&self.text)
                } else {
                    StartupScreen::Exited
                },
                version: screen::version(&self.text),
            }),
        }
    }
    pub async fn startup(&mut self) -> std::result::Result<(), LimitResetResult> {
        let mut until = Instant::now() + Duration::from_secs(30);
        let (mut theme, mut security, mut trust) = (false, false, false);
        loop {
            let shown = self
                .seen(until, Duration::from_secs(3), |s| {
                    if let Some(page) = onboarding::page(s) {
                        Some(Startup::Onboarding(page))
                    } else if screen::sign_in(s) {
                        Some(Startup::Login)
                    } else if let Some(yes) = screen::trust(s) {
                        Some(Startup::Trust(yes))
                    } else if screen::footer(s) {
                        Some(Startup::Dialog)
                    } else if screen::prompt(s)
                        .is_some_and(|line| line.is_empty() || screen::version(s).is_some())
                    {
                        Some(Startup::Prompt)
                    } else {
                        None
                    }
                })
                .await;
            match shown {
                Some(Startup::Login) => {
                    return Err(LimitResetResult::ReauthRequired {
                        reason: ReauthReason::NotAuthenticated,
                    });
                }
                Some(Startup::Onboarding(page)) => {
                    match page {
                        Page::Theme if !theme && !security => theme = true,
                        Page::Security if !security => security = true,
                        _ => return Err(self.failed()),
                    };
                    if !self.key("\r")
                        || self
                            .seen(until, Duration::ZERO, |s| {
                                (onboarding::page(s) != Some(page)).then_some(())
                            })
                            .await
                            .is_none()
                    {
                        return Err(self.failed());
                    }
                }
                Some(Startup::Trust(yes)) if !trust => {
                    if !yes
                        && (!self.key("\x1b[B")
                            || self
                                .seen(Instant::now() + Duration::from_secs(5), STABLE, |s| {
                                    (screen::trust(s) == Some(true)).then_some(())
                                })
                                .await
                                .is_none())
                    {
                        return Err(self.failed());
                    }
                    if !self.drain() || screen::trust(&self.text) != Some(true) || !self.key("\r") {
                        return Err(self.failed());
                    }
                    trust = true;
                    self.onboarding_completed = security;
                    until = Instant::now() + Duration::from_secs(30);
                    if self
                        .seen(until, Duration::ZERO, |s| {
                            screen::trust(s).is_none().then_some(())
                        })
                        .await
                        .is_none()
                    {
                        return Err(self.failed());
                    }
                }
                Some(Startup::Prompt) => {
                    self.version = screen::version(&self.text);
                    if self.version.is_none() {
                        return Err(unsupported(UnsupportedReason::UnsupportedVersion));
                    }
                    if screen::prompt(&self.text).as_deref() != Some("") {
                        return Err(self.failed());
                    }
                    self.onboarding_completed = security;
                    return Ok(());
                }
                _ => return Err(self.failed()),
            }
        }
    }
    /// Type the complete command without Enter, then verify what the terminal actually displays.
    pub async fn prepare(&mut self, cmd: &str) -> bool {
        if !self.drain() || screen::prompt(&self.text).as_deref() != Some("") || !self.key(cmd) {
            return false;
        }
        self.seen(
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(400),
            |s| (screen::input(s).as_deref() == Some(cmd)).then_some(()),
        )
        .await
        .is_some()
    }
    pub fn submit(&mut self, cmd: &str) -> bool {
        self.drain() && screen::input(&self.text).as_deref() == Some(cmd) && self.key("\r")
    }
    pub async fn result(&mut self, cmd: &str, timeout: Duration) -> Option<screen::Shown> {
        self.seen(Instant::now() + timeout, Duration::from_millis(100), |s| {
            screen::command(s, cmd)
        })
        .await
    }
    pub async fn cancel(&mut self, cmd: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        if self
            .seen(until, STABLE, |s| screen::footer(s).then_some(()))
            .await
            .is_none()
            || !self.key("\x1b")
        {
            return false;
        }
        self.seen(until, Duration::ZERO, |s| {
            matches!(
                screen::command(s, cmd),
                Some(screen::Shown::Result(screen::Line::Kept))
            )
            .then_some(())
        })
        .await
        .is_some()
    }
    pub async fn stable_dialog(&mut self, kind: ResetKind) -> bool {
        self.seen(Instant::now() + Duration::from_secs(3), STABLE, |s| {
            screen::dialog(s).filter(|d| d.kind == kind)
        })
        .await
        .is_some()
    }
    pub fn confirm(&mut self, kind: ResetKind) -> bool {
        self.drain()
            && self.changed.elapsed() >= STABLE
            && screen::dialog(&self.text).is_some_and(|d| d.kind == kind)
            && self.key("\r")
    }
    pub async fn claim_result(&mut self, cmd: &str) -> Option<screen::Line> {
        self.seen(
            Instant::now() + Duration::from_secs(30),
            Duration::ZERO,
            |s| match screen::command(s, cmd) {
                Some(screen::Shown::Result(l)) => Some(l),
                _ => None,
            },
        )
        .await
    }
    pub async fn close(&mut self) {
        self.terminal.terminate();
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.terminal.exit.wait_for(Option::is_some),
        )
        .await;
    }
}
