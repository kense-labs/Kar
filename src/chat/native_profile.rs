//! The host assigns this namespace when a native session attaches. Inherited turns retain it.
use super::*;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntigravityProfile {
    pub path: PathBuf,
    pub app_data_dir: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeChildOrigin {
    pub chat: Uuid,
    pub profile: AntigravityProfile,
    pub root_session: String,
}
// agy accepts Go-style -flag and --flag, including a value joined by "=".
fn argument<'a>(opts: &'a SessionOptions, name: &str) -> std::result::Result<Option<&'a str>, ()> {
    let mut found = None;
    for (index, arg) in opts.launch_args.iter().enumerate() {
        let Some(arg) = arg.strip_prefix('-') else {
            continue;
        };
        let arg = arg.strip_prefix('-').unwrap_or(arg);
        let (key, value) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        if key != name {
            continue;
        }
        let value = value
            .or_else(|| opts.launch_args.get(index + 1).map(String::as_str))
            .filter(|v| !v.is_empty())
            .ok_or(())?;
        if found.replace(value).is_some_and(|old| old != value) {
            return Err(());
        }
    }
    Ok(found)
}
impl AntigravityProfile {
    fn from_dir(path: PathBuf, app_data_dir: PathBuf, file: &File) -> Option<Self> {
        let m = file.metadata().ok()?;
        if !m.is_dir() {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            path,
            app_data_dir,
            #[cfg(unix)]
            device: m.dev(),
            #[cfg(unix)]
            inode: m.ino(),
        })
    }
    pub(super) fn at_launch(runtime: &str, opts: &SessionOptions) -> Option<Self> {
        if runtime != "antigravity" {
            return None;
        }
        let arg = argument(opts, "gemini-dir").ok()?.map(PathBuf::from);
        let env = crate::options::env_path(opts, "GEMINI_HOME");
        if let (Some(arg), Some(env)) = (&arg, &env)
            && opts.cwd.join(arg).canonicalize().ok()? != opts.cwd.join(env).canonicalize().ok()?
        {
            return None;
        }
        let app_data_dir = PathBuf::from(
            argument(opts, "app-data-dir")
                .ok()?
                .unwrap_or("antigravity-cli"),
        );
        if app_data_dir.components().next().is_none()
            || app_data_dir
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return None;
        }
        let path = arg.or(env).or_else(|| {
            crate::options::env_path(opts, if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .or_else(std::env::home_dir)
                .map(|p| p.join(".gemini"))
        })?;
        let path = opts.cwd.join(path).canonicalize().ok()?;
        let dir = cap_std::fs::Dir::open_ambient_dir(&path, cap_std::ambient_authority()).ok()?;
        Self::from_dir(path, app_data_dir, &dir.into_std_file())
    }
    pub(crate) fn directory(&self) -> std::io::Result<cap_std::fs::Dir> {
        let dir = cap_std::fs::Dir::open_ambient_dir(&self.path, cap_std::ambient_authority())?;
        let file = dir.into_std_file();
        if Self::from_dir(self.path.clone(), self.app_data_dir.clone(), &file).as_ref()
            != Some(self)
            || self.path.canonicalize()? != self.path
        {
            return Err(std::io::Error::other(
                "saved Antigravity profile directory has changed",
            ));
        }
        Ok(cap_std::fs::Dir::from_std_file(file))
    }
    pub(super) fn configure(&self, opts: &mut SessionOptions) -> Result<()> {
        self.directory()?;
        if Self::at_launch("antigravity", opts).as_ref() == Some(self) {
            return Ok(());
        }
        for (name, saved) in [
            ("gemini-dir", self.path.as_path()),
            ("app-data-dir", self.app_data_dir.as_path()),
        ] {
            let arg = argument(opts, name)
                .map_err(|_| Error::Refused(format!("ambiguous Antigravity --{name}")))?;
            if let Some(arg) = arg {
                let same = if name == "gemini-dir" {
                    opts.cwd.join(arg).canonicalize().ok().as_deref() == Some(saved)
                } else {
                    Path::new(arg) == saved
                };
                if !same {
                    return Err(Error::Refused(format!(
                        "Antigravity --{name} conflicts with saved native profile"
                    )));
                }
            } else if name == "app-data-dir" && saved != Path::new("antigravity-cli") {
                opts.launch_args.extend([
                    "--app-data-dir".into(),
                    saved.to_string_lossy().into_owned(),
                ]);
            }
        }
        opts.env
            .insert("GEMINI_HOME".into(), self.path.clone().into_os_string());
        Ok(())
    }
}
