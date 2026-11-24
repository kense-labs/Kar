use super::{Project, Workbench, denied};
use crate::{Error, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub(super) const READ_LIMIT: usize = 1024 * 1024;

fn checked(path: &Path) -> Result<PathBuf> {
    let mut relative = PathBuf::new();
    for segment in path.components() {
        match segment {
            Component::CurDir => {}
            Component::Normal(s) => {
                if s.eq_ignore_ascii_case(".git") {
                    return Err(denied());
                }
                relative.push(s);
            }
            Component::ParentDir if relative.pop() => {}
            _ => return Err(denied()),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(Error::Refused("expected a file path".into()));
    }
    Ok(relative)
}

pub(super) struct Workspace {
    pub root: PathBuf,
    pub private: PathBuf,
    dir: Dir,
}

impl Workbench {
    pub(super) fn workspace(&self, project: &Project) -> Result<Workspace> {
        let allowed = self.allowed(&project.root)?;
        if project.root.canonicalize()? != project.root {
            return Err(denied());
        }
        let mut dir = allowed.dir.try_clone()?;
        for part in project
            .root
            .strip_prefix(&allowed.path)
            .map_err(|_| denied())?
            .components()
        {
            dir = dir.open_dir_nofollow(part.as_os_str())?;
        }
        Ok(Workspace {
            root: project.root.clone(),
            private: self.0.chats.directory().into(),
            dir,
        })
    }
}

impl Workspace {
    // Resolve existing links before checking the target. A missing suffix can be created
    // under its nearest existing parent. Final operations use the pinned directory handle.
    fn target(&self, relative: &Path, create: bool) -> Result<PathBuf> {
        let requested = checked(relative)?;
        let path = self.root.join(&requested);
        let mut existing = path.as_path();
        let resolved = loop {
            match fs::symlink_metadata(existing) {
                Ok(_) => {
                    break existing
                        .canonicalize()?
                        .join(path.strip_prefix(existing).expect("ancestor"));
                }
                Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or_else(denied)?;
                }
                Err(e) => return Err(e.into()),
            }
        };
        if resolved.starts_with(&self.private) {
            return Err(denied());
        }
        checked(resolved.strip_prefix(&self.root).map_err(|_| denied())?)
    }

    fn parent(&self, path: &Path, create: bool) -> Result<Dir> {
        let mut dir = self.dir.try_clone()?;
        for part in path.parent().ok_or_else(denied)?.components() {
            let name = part.as_os_str();
            dir = match dir.open_dir_nofollow(name) {
                Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                    match dir.create_dir(name) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                    dir.open_dir_nofollow(name)?
                }
                result => result?,
            };
        }
        Ok(dir)
    }

    pub fn read(&self, path: &Path) -> Result<String> {
        let path = self.target(path, false)?;
        let dir = self.parent(&path, false)?;
        let name = path.file_name().ok_or_else(denied)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NONBLOCK);
        }
        let file = dir.open_with(name, &options)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > READ_LIMIT as u64 {
            return Err(Error::Refused(
                "expected a regular text file of at most 1 MiB".into(),
            ));
        }
        let mut data = Vec::new();
        file.take((READ_LIMIT + 1) as u64).read_to_end(&mut data)?;
        if data.len() > READ_LIMIT || data.contains(&0) {
            return Err(Error::Refused(
                "file is too large or contains binary data".into(),
            ));
        }
        String::from_utf8(data).map_err(|_| Error::Refused("file is not UTF-8 text".into()))
    }

    pub fn write(&self, path: &Path, text: &str) -> Result<()> {
        if text.contains('\0') {
            return Err(Error::Refused("expected text content".into()));
        }
        let path = self.target(path, true)?;
        let dir = self.parent(&path, true)?;
        let name = path.file_name().ok_or_else(denied)?;
        let permissions = match dir.symlink_metadata(name) {
            Ok(meta) if meta.is_file() => Some(meta.permissions()),
            Ok(_) => return Err(Error::Refused("expected a regular file".into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let tmp = format!(".kar-save-{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = dir.open_with(&tmp, &options)?;
        let result = (|| -> io::Result<()> {
            file.write_all(text.as_bytes())?;
            if let Some(mode) = permissions {
                file.set_permissions(mode)?;
            }
            file.sync_all()?;
            drop(file);
            dir.rename(&tmp, &dir, name)?;
            #[cfg(unix)]
            dir.try_clone()?.into_std_file().sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = dir.remove_file(tmp);
        }
        Ok(result?)
    }
}
