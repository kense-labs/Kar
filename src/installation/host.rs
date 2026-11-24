//! Report host failures only when the selected launcher or its actual diagnostic identifies one.
use super::*;
use std::io::Read;

pub(super) fn missing_interpreter(probe: &Probe, c: &Candidate) -> Option<InstallationSnapshot> {
    #[cfg(not(unix))]
    {
        let _ = (probe, c);
        None
    }
    #[cfg(unix)]
    {
        let mut bytes = Vec::new();
        std::fs::File::open(&c.command)
            .ok()?
            .take(512)
            .read_to_end(&mut bytes)
            .ok()?;
        let line = bytes.split(|b| *b == b'\n').next()?.strip_prefix(b"#!")?;
        let line = std::str::from_utf8(line).ok()?;
        let mut fields = line.split_ascii_whitespace();
        let interpreter = Path::new(fields.next()?);
        if !interpreter.is_absolute() {
            return None;
        }
        let missing = if probe.locate(interpreter, true).is_none() {
            interpreter
        } else if interpreter == Path::new("/usr/bin/env") {
            let name = fields.next()?;
            let name = if name == "-S" { fields.next()? } else { name };
            if name.starts_with('-')
                || name.contains(['=', '"', '\''])
                || probe.locate(Path::new(name), true).is_some()
            {
                return None;
            }
            Path::new(name)
        } else {
            return None;
        };
        Some(c.fail(
            InstallationIssue::DependencyMissing,
            &format!(
                "the selected launcher's interpreter {} is unavailable in the probe environment",
                missing.display()
            ),
        ))
    }
}

pub(super) fn spawn_failure(c: &Candidate, error: Error, fallback: &str) -> InstallationSnapshot {
    if let Error::Io(io) = &error {
        #[cfg(target_os = "macos")]
        if io.raw_os_error() == Some(nix::libc::EBADARCH) {
            return c.fail(
                InstallationIssue::UnsupportedPlatform,
                "the selected executable was built for an unsupported processor architecture",
            );
        }
        if io.kind() == std::io::ErrorKind::PermissionDenied {
            return c.fail(
                InstallationIssue::UnsupportedHost,
                "the host denied execution of the selected launcher",
            );
        }
    }
    c.fail(InstallationIssue::ReadinessFailed, fallback)
}

pub(super) fn reported_failure(c: &Candidate, out: &Output) -> Option<InstallationSnapshot> {
    if out.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("ERR_MODULE_NOT_FOUND")
        || stderr.contains("MODULE_NOT_FOUND")
        || stderr.contains("ModuleNotFoundError: No module named")
        || stderr.contains("Library not loaded:")
    {
        let detail = stderr
            .lines()
            .find(|l| {
                l.contains("Cannot find module")
                    || l.contains("Cannot find package")
                    || l.contains("No module named")
                    || l.contains("Library not loaded:")
            })
            .map(|line| line.trim().chars().take(300).collect::<String>())
            .unwrap_or_else(|| "the launcher reported a missing module".into());
        return Some(c.fail(InstallationIssue::DependencyMissing, &detail));
    }
    let requires_node = regex::Regex::new(
        r"(?i)\b(?:requires?|needs?)\s+node(?:\.js)?(?:\s+version)?\s+(?:v|[>=^~]+\s*)?\d",
    )
    .expect("Node version diagnostic");
    if let Some(line) = stderr.lines().find(|line| requires_node.is_match(line)) {
        return Some(c.fail(
            InstallationIssue::UnsupportedNodeVersion,
            &line.trim().chars().take(300).collect::<String>(),
        ));
    }
    None
}
