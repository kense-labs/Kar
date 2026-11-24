use crate::{Result, native_file::open_regular};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

/// A marked directory remains leased until its runtime exits. The next open
/// only reclaims abandoned directories belonging to the same operation.
pub(crate) struct PrivateDir {
    _lock: File,
    dir: tempfile::TempDir,
}
impl PrivateDir {
    pub(crate) fn sweep(root: &Path, prefix: &str, signature: &[u8]) -> Result<()> {
        Self::sweep_with(root, prefix, signature, |_| {})
    }

    pub(crate) fn sweep_with(
        root: &Path,
        prefix: &str,
        signature: &[u8],
        before_remove: impl Fn(&Path),
    ) -> Result<()> {
        for entry in fs::read_dir(root)?.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(prefix)
                || !entry.file_type().is_ok_and(|f| f.is_dir())
            {
                continue;
            }
            let path = entry.path();
            if let Ok(file) = open_regular(&path.join(".owner"))
                && file.try_lock().is_ok()
                && file.metadata()?.len() == signature.len() as u64
            {
                let mut marker = Vec::new();
                if (&file)
                    .take(signature.len() as u64 + 1)
                    .read_to_end(&mut marker)
                    .is_ok()
                    && marker == signature
                {
                    before_remove(&path);
                    let _ = fs::remove_dir_all(path);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn create(root: &Path, prefix: &str, signature: &[u8]) -> Result<Self> {
        Self::sweep(root, prefix, signature)?;
        Self::create_after_sweep(root, prefix, signature)
    }

    pub(crate) fn create_after_sweep(root: &Path, prefix: &str, signature: &[u8]) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(prefix);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let dir = builder.tempdir_in(root)?;
        let mut marker = File::create_new(dir.path().join(".owner"))?;
        marker.lock()?;
        marker.write_all(signature)?;
        Ok(Self { _lock: marker, dir })
    }
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}
