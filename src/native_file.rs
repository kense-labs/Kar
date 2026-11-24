use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    if !meta.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

/// Native project directories use provider-specific path encodings. Inspect only
/// direct project children and only a regular file named for the exact session.
pub(crate) fn project_transcripts(dir: &Path, session: uuid::Uuid) -> io::Result<Vec<PathBuf>> {
    let name = format!("{session}.jsonl");
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path().join(&name);
            match fs::symlink_metadata(&path) {
                Ok(m) if m.is_file() => files.push(path),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(files)
}
