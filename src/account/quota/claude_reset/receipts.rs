//! Claude has no native idempotency key. Journal the possible spend before submitting the command.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

pub(super) struct Receipts {
    dir: PathBuf,
    _lock: File,
}
#[derive(Serialize, Deserialize)]
struct Record {
    kind: ResetKind,
    report: LimitResetReport,
}
impl Receipts {
    pub fn directory(probe: &Probe) -> Result<PathBuf> {
        Ok(crate::account::profile::path(probe, "CLAUDE_CONFIG_DIR")
            .or_else(|| crate::account::profile_home(probe).map(|h| h.join(".claude")))
            .ok_or_else(|| protocol("Cannot locate the Claude profile for limit reset receipts"))?
            .join("kar-limit-resets"))
    }
    pub async fn open(probe: &Probe, end: Instant) -> Result<Option<Self>> {
        let dir = Self::directory(probe)?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        let mut opts = File::options();
        opts.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        }
        let lock = opts.open(dir.join("lock"))?;
        if !lock.metadata()?.is_file() {
            return Err(protocol("Invalid Claude reset receipt lock"));
        }
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(Some(Self { dir, _lock: lock })),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < end => {
                    tokio::time::sleep_until((Instant::now() + Duration::from_millis(50)).min(end))
                        .await
                }
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }
    fn path(&self, key: &str) -> PathBuf {
        self.dir
            .join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
    }
    pub fn find(&self, key: &str, kind: ResetKind) -> Result<Option<LimitResetReport>> {
        let Some(raw) = crate::account::read_profile(&self.path(key), 32 * 1024)? else {
            return Ok(None);
        };
        let r: Record =
            serde_json::from_str(&raw).map_err(|_| protocol("Unreadable Claude reset receipt"))?;
        if r.kind != kind {
            return Err(protocol(
                "This idempotency key already names a different Claude reset kind",
            ));
        }
        Ok(Some(r.report))
    }
    pub fn save(&self, key: &str, kind: ResetKind, report: &LimitResetReport) -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(
            &mut tmp,
            &Record {
                kind,
                report: report.clone(),
            },
        )?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.path(key)).map_err(|e| e.error)?;
        self.sync()
    }
    pub fn forget(&self, key: &str) -> Result<()> {
        fs::remove_file(self.path(key))?;
        self.sync()
    }
    fn sync(&self) -> Result<()> {
        #[cfg(unix)]
        {
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }
}
