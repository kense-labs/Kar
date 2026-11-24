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
    pub(super) fn root_handle(&self) -> Result<fs::File> {
        Ok(self.dir.try_clone()?.into_std_file())
    }

    // Resolve existing links before checking the target. A missing suffix can be created
    // under its nearest existing parent. Final operations use the pinned directory handle.
    pub(super) fn target(&self, relative: &Path, create: bool) -> Result<PathBuf> {
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
        preview(self.read_bytes(&path, READ_LIMIT)?)
    }

    /// The caller has resolved this path once. Never follow a replacement link.
    pub(super) fn read_bytes(&self, path: &Path, limit: usize) -> Result<Vec<u8>> {
        let dir = self.parent(path, false)?;
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
        if !meta.is_file() || meta.len() > limit as u64 {
            return Err(Error::Refused(format!(
                "expected a regular file of at most {limit} bytes"
            )));
        }
        let mut data = Vec::new();
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut data)?;
        if data.len() > limit {
            return Err(Error::Refused(format!("file exceeds {limit} bytes")));
        }
        Ok(data)
    }

    pub(super) fn write_bytes(&self, path: &Path, data: &[u8]) -> Result<()> {
        self.save(path, data, true)
    }

    pub(super) fn publish_attachment(&self, path: &Path, data: &[u8]) -> Result<()> {
        if self.target(path, true)? != path {
            return Err(denied());
        }
        match self.save(path, data, false) {
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                if self.read_bytes(path, data.len())? == data {
                    Ok(())
                } else {
                    Err(Error::Refused("attachment destination has changed".into()))
                }
            }
            result => result,
        }
    }

    fn save(&self, path: &Path, data: &[u8], replace: bool) -> Result<()> {
        let dir = self.parent(path, true)?;
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
            file.write_all(data)?;
            if let Some(mode) = permissions {
                file.set_permissions(mode)?;
            }
            file.sync_all()?;
            drop(file);
            if replace {
                dir.rename(&tmp, &dir, name)?;
            } else {
                dir.hard_link(&tmp, &dir, name)?;
                dir.remove_file(&tmp)?;
            }
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

pub(super) fn preview(data: Vec<u8>) -> Result<String> {
    if data.len() > READ_LIMIT || data.contains(&0) {
        return Err(Error::Refused(
            "file is too large or contains binary data".into(),
        ));
    }
    String::from_utf8(data).map_err(|_| Error::Refused("file is not UTF-8 text".into()))
}
