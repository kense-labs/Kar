use crate::chat::AntigravityProfile;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};
pub(super) struct Profile {
    pub path: PathBuf,
    base: Dir,
}
pub(super) fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
impl Profile {
    pub fn open(profile: &AntigravityProfile) -> io::Result<Self> {
        let mut base = profile.directory()?;
        for part in profile.app_data_dir.components() {
            let std::path::Component::Normal(name) = part else {
                return Err(invalid());
            };
            base = base.open_dir_nofollow(name)?;
        }
        Ok(Self {
            path: profile.path.join(&profile.app_data_dir),
            base,
        })
    }
    pub fn dir(&self, parts: &[&str]) -> io::Result<Dir> {
        let mut dir = self.base.try_clone()?;
        for part in parts {
            dir = dir.open_dir_nofollow(part)?;
        }
        Ok(dir)
    }
}
pub(super) fn file(dir: &Dir, name: impl AsRef<Path>) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        o.custom_flags(nix::libc::O_NONBLOCK);
    }
    if !dir.symlink_metadata(&name)?.is_file() {
        return Err(invalid());
    }
    let f = dir.open_with(name, &o)?.into_std();
    if !f.metadata()?.is_file() {
        return Err(invalid());
    }
    Ok(f)
}
pub(super) fn bytes(f: File, limit: u64) -> io::Result<Vec<u8>> {
    let len = f.metadata()?.len();
    if len > limit {
        return Err(invalid());
    }
    let mut data = Vec::with_capacity(len as usize);
    f.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 != len {
        return Err(invalid());
    }
    Ok(data)
}
