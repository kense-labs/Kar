mod inspection;
pub(super) use inspection::routes;

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
        self.workspace_at(&project.root)
    }

    pub(super) fn workspace_at(&self, root: &Path) -> Result<Workspace> {
        let allowed = self.allowed(root)?;
        if root.canonicalize()? != root {
            return Err(denied());
        }
        let mut dir = allowed.dir.try_clone()?;
        for part in root
            .strip_prefix(&allowed.path)
            .map_err(|_| denied())?
            .components()
        {
            dir = dir.open_dir_nofollow(part.as_os_str())?;
        }
        Ok(Workspace {
            root: root.to_owned(),
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

    pub(super) fn parent(&self, path: &Path, create: bool) -> Result<Dir> {
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
        let file = self.open_regular(path)?;
        if file.metadata()?.len() > limit as u64 {
            return Err(Error::Refused(format!("file exceeds {limit} bytes")));
        }
        let mut data = Vec::new();
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut data)?;
        if data.len() > limit {
            return Err(Error::Refused(format!("file exceeds {limit} bytes")));
        }
        Ok(data)
    }

    pub(super) fn open_regular(&self, path: &Path) -> Result<cap_std::fs::File> {
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
        if !file.metadata()?.is_file() {
            return Err(Error::Refused("expected a regular file".into()));
        }
        Ok(file)
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
        result?;
        inspection::invalidate_workspace_index(&self.root);
        Ok(())
    }
}

fn plausible_decoded_text(text: &str) -> bool {
    let mut total = 0usize;
    let mut bad_ctrl = 0usize;
    for ch in text.chars() {
        total += 1;
        if (ch as u32) < 0x20 && !matches!(ch, '\t' | '\n' | '\r' | '\x0c' | '\x1b') {
            bad_ctrl += 1;
        }
    }
    bad_ctrl == 0 || bad_ctrl.saturating_mul(20) <= total.max(1)
}

pub(super) fn decode_text_bytes(
    bytes: &[u8],
    truncated: bool,
) -> Result<(std::borrow::Cow<'_, str>, &'static str)> {
    use std::borrow::Cow;
    if bytes.contains(&0) {
        return Err(Error::Refused("file contains binary data".into()));
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            return Ok((
                Cow::Borrowed(s.strip_prefix('\u{feff}').unwrap_or(s)),
                "utf-8",
            ));
        }
        Err(e) if truncated && e.error_len().is_none() => {
            let s = std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("valid prefix");
            return Ok((
                Cow::Borrowed(s.strip_prefix('\u{feff}').unwrap_or(s)),
                "utf-8",
            ));
        }
        Err(_) => {}
    }
    let max_trim = if truncated { 3 } else { 0 };
    for trim in 0..=max_trim {
        if bytes.len() < trim {
            break;
        }
        let slice = &bytes[..bytes.len() - trim];
        if let Some(decoded) =
            encoding_rs::GB18030.decode_without_bom_handling_and_without_replacement(slice)
            && plausible_decoded_text(&decoded)
        {
            let clean = match decoded {
                Cow::Borrowed(s) => Cow::Borrowed(s.strip_prefix('\u{feff}').unwrap_or(s)),
                Cow::Owned(mut s) => {
                    if s.starts_with('\u{feff}') {
                        s.drain(..'\u{feff}'.len_utf8());
                    }
                    Cow::Owned(s)
                }
            };
            return Ok((clean, "gb18030"));
        }
    }
    Err(Error::Refused("file is not UTF-8 text".into()))
}

pub(super) fn preview(data: Vec<u8>) -> Result<String> {
    if data.len() > READ_LIMIT || data.contains(&0) {
        return Err(Error::Refused(
            "file is too large or contains binary data".into(),
        ));
    }
    decode_text_bytes(&data, false).map(|(text, _)| text.into_owned())
}

pub(super) fn text_preview(bytes: &[u8], size: u64) -> Result<serde_json::Value> {
    let truncated = size > READ_LIMIT as u64;
    let bytes = &bytes[..bytes.len().min(READ_LIMIT)];
    let (text, encoding) = decode_text_bytes(bytes, truncated)?;
    Ok(
        serde_json::json!({"text":text,"encoding":encoding,"byte_length":size,"truncated":truncated,"exists":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_preview_decodes_utf8_and_gb18030_and_rejects_binary() -> Result<()> {
        let utf8 = text_preview(b"\xef\xbb\xbfhello world", 14)?;
        assert_eq!(utf8["text"], "hello world");
        assert_eq!(utf8["encoding"], "utf-8");
        assert_eq!(utf8["byte_length"], 14);
        assert_eq!(utf8["truncated"], false);
        assert_eq!(utf8["exists"], true);

        let chinese = "// 中文注释：支持 GBK 与 GB18030 源码读取\nint main() { return 0; }\n";
        let (gbk_bytes, _, had_errors) = encoding_rs::GB18030.encode(chinese);
        assert!(!had_errors);
        assert!(std::str::from_utf8(&gbk_bytes).is_err());

        let gbk = text_preview(&gbk_bytes, gbk_bytes.len() as u64)?;
        assert_eq!(gbk["text"], chinese);
        assert_eq!(gbk["encoding"], "gb18030");
        assert_eq!(gbk["exists"], true);
        assert_eq!(preview(gbk_bytes.into_owned())?, chinese);

        // NUL bytes must be rejected as binary data.
        assert!(text_preview(b"abc\0def", 7).is_err());
        // Invalid trailing high byte when not truncated must be rejected.
        assert!(text_preview(&[0xff, 0xfe, 0xfd], 3).is_err());
        Ok(())
    }
}
