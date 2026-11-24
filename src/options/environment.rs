//! The environment supplied to a native process and the values read on its behalf.
use crate::{Error, InstallationOptions, Result, SessionOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};
use tokio::process::Command;

pub(crate) fn same_name(a: &OsStr, b: &OsStr, windows: bool) -> bool {
    if !windows {
        return a == b;
    }
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    match (a.to_str(), b.to_str()) {
        (Some(a), Some(b)) => a
            .chars()
            .flat_map(char::to_uppercase)
            .eq(b.chars().flat_map(char::to_uppercase)),
        _ => false,
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0'])
}

impl SessionOptions {
    /// Remove an inherited variable for this session and its subprocesses.
    /// An explicit value added to `env` afterwards takes precedence for that launch.
    pub fn remove_env(&mut self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if !valid_name(&name) {
            return Err(Error::Protocol("invalid environment variable name".into()));
        }
        self.env
            .retain(|key, _| !same_name(key, OsStr::new(&name), cfg!(windows)));
        self.unset_env.insert(name);
        Ok(())
    }

    pub(crate) fn env_value(&self, name: &str) -> Option<OsString> {
        let key = OsStr::new(name);
        if let Some((_, value)) = self
            .env
            .iter()
            .rev()
            .find(|(k, _)| same_name(k, key, cfg!(windows)))
        {
            return Some(value.clone());
        }
        if self
            .unset_env
            .iter()
            .any(|n| same_name(OsStr::new(n), key, cfg!(windows)))
        {
            return None;
        }
        std::env::var_os(key)
    }

    /// Probe executables using this session's working directory, overrides and removed variables.
    pub fn installation_options(&self) -> InstallationOptions {
        let mut env: BTreeMap<OsString, Option<OsString>> = self
            .unset_env
            .iter()
            .map(|k| (OsString::from(k), None))
            .collect();
        for (key, value) in &self.env {
            #[cfg(windows)]
            env.retain(|name, _| !same_name(name, key, true));
            env.insert(key.clone(), Some(value.clone()));
        }
        InstallationOptions {
            cwd: Some(self.cwd.clone()),
            env,
        }
    }

    pub(crate) fn command(&self, program: &Path) -> Result<Command> {
        validate_removals(&self.unset_env)?;
        let mut cmd = Command::new(program);
        for name in &self.unset_env {
            cmd.env_remove(name);
        }
        cmd.envs(&self.env).current_dir(&self.cwd);
        Ok(cmd)
    }

    pub(crate) fn session_home(&self) -> Option<PathBuf> {
        super::env_path(self, if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .or_else(crate::installation::account_home)
    }
}

pub(crate) fn validate_removals(names: &BTreeSet<String>) -> Result<()> {
    if names.iter().all(|name| valid_name(name)) {
        Ok(())
    } else {
        Err(Error::Protocol("invalid environment variable name".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installation::Probe;
    #[test]
    fn explicit_empty_overrides_a_removal_and_invalid_names_never_launch() {
        let mut opts = SessionOptions::new(".");
        opts.env.insert("KAR_TEST_OVERLAY".into(), "first".into());
        opts.remove_env("KAR_TEST_OVERLAY").unwrap();
        assert!(opts.env_value("KAR_TEST_OVERLAY").is_none());
        assert!(
            Probe::for_session(&opts)
                .unwrap()
                .get("KAR_TEST_OVERLAY")
                .is_none()
        );

        opts.env.insert("KAR_TEST_OVERLAY".into(), "".into());
        assert_eq!(
            opts.env_value("KAR_TEST_OVERLAY").as_deref(),
            Some(OsStr::new(""))
        );
        assert_eq!(
            Probe::for_session(&opts).unwrap().get("KAR_TEST_OVERLAY"),
            Some(OsStr::new(""))
        );
        let cmd = opts.command(Path::new("fixture")).unwrap();
        assert!(
            cmd.as_std()
                .get_envs()
                .any(|(key, value)| key == "KAR_TEST_OVERLAY" && value == Some(OsStr::new("")))
        );

        for key in ["a=b", "", "zero\0byte"] {
            assert!(opts.remove_env(key).is_err());
        }
        opts.unset_env.insert("bad=name".into());
        assert!(opts.command(Path::new("fixture")).is_err());
        assert!(Probe::for_session(&opts).is_err());
    }
}
