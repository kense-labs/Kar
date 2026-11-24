use super::{Candidate, Probe};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    hash::{BuildHasher, RandomState},
    io::Read,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

#[derive(Hash, PartialEq, Eq)]
pub(super) struct Key {
    runtime: String,
    command: PathBuf,
    cwd: PathBuf,
    environment: [u64; 2],
}
#[derive(PartialEq, Eq)]
pub(super) struct Identity {
    target: PathBuf,
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    unix: (u64, u64, i64, i64),
    #[cfg(windows)]
    created: u64,
}
impl Identity {
    pub fn read(path: &Path) -> Option<Self> {
        let target = path.canonicalize().ok()?;
        for p in [path, target.as_path()] {
            let parent = p.parent()?;
            let dir = parent.file_name()?.to_string_lossy().to_ascii_lowercase();
            let volta = parent.parent().and_then(Path::file_name).is_some_and(|s| {
                s.eq_ignore_ascii_case("volta") || s.eq_ignore_ascii_case(".volta")
            });
            let name = p.file_stem()?.to_string_lossy().to_ascii_lowercase();
            if dir == "shims"
                || (dir == "bin" && volta)
                || matches!(name.as_str(), "volta-shim" | "mise" | "rtx" | "asdf")
            {
                return None;
            }
        }
        // A script or package-manager shim can hand off to a different program
        // without its own timestamp changing. Only native executables are cached.
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NONBLOCK);
        }
        let mut file = options.open(&target).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut header = [0; 4];
        file.read_exact(&mut header).ok()?;
        if !header.starts_with(b"MZ")
            && header != *b"\x7fELF"
            && !matches!(
                u32::from_be_bytes(header),
                0xfeedface
                    | 0xcefaedfe
                    | 0xfeedfacf
                    | 0xcffaedfe
                    | 0xcafebabe
                    | 0xbebafeca
                    | 0xcafebabf
                    | 0xbfbafeca
            )
        {
            return None;
        }
        let meta = file.metadata().ok()?;
        Some(Self {
            target,
            size: meta.len(),
            modified: meta.modified().ok()?,
            #[cfg(unix)]
            unix: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
            },
            #[cfg(windows)]
            created: {
                use std::os::windows::fs::MetadataExt;
                meta.creation_time()
            },
        })
    }
}
type Entries = HashMap<Key, (Identity, String)>;
static VERSIONS: LazyLock<Mutex<Entries>> = LazyLock::new(Default::default);
static SALTS: LazyLock<[RandomState; 2]> = LazyLock::new(Default::default);
impl Key {
    pub fn new(probe: &Probe, runtime: &str, candidate: &Candidate) -> Self {
        Self {
            runtime: runtime.into(),
            command: candidate.command.clone(),
            cwd: probe.cwd.clone(),
            environment: [SALTS[0].hash_one(&probe.env), SALTS[1].hash_one(&probe.env)],
        }
    }
    pub fn version(&self, identity: &Identity) -> Option<String> {
        let cache = VERSIONS.lock().unwrap();
        let (old, version) = cache.get(self)?;
        (old == identity).then(|| version.clone())
    }
    pub fn remember(self, before: Identity, version: &str) {
        if Identity::read(&self.command).as_ref() != Some(&before) {
            return;
        }
        let mut cache = VERSIONS.lock().unwrap();
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(self, (before, version.into()));
    }
}
pub(super) fn clear() {
    VERSIONS.lock().unwrap().clear();
}
