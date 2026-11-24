//! Resolve known graphical editor launchers on the host and pass workspace paths as argv.
use super::{Workbench, http::ApiError};
use crate::{Error, Result};
use axum::{
    Json, Router,
    extract::{Path as Param, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Style {
    Goto,
    JetBrains,
    Zed,
    Finder,
}
struct Editor {
    id: &'static str,
    names: &'static [&'static str],
    commands: &'static [&'static str],
    style: Style,
}
const fn editor(
    id: &'static str,
    names: &'static [&'static str],
    commands: &'static [&'static str],
    style: Style,
) -> Editor {
    Editor {
        id,
        names,
        commands,
        style,
    }
}
const EDITORS: &[Editor] = &[
    editor("cursor", &["Cursor"], &["cursor"], Style::Goto),
    editor("trae", &["Trae"], &["trae"], Style::Goto),
    editor("kiro", &["Kiro"], &["kiro"], Style::Goto),
    editor("vscode", &["Visual Studio Code"], &["code"], Style::Goto),
    editor(
        "vscode-insiders",
        &["Visual Studio Code - Insiders"],
        &["code-insiders"],
        Style::Goto,
    ),
    editor("vscodium", &["VSCodium"], &["codium"], Style::Goto),
    editor("zed", &["Zed"], &["zed", "zeditor"], Style::Zed),
    editor(
        "antigravity",
        &["Antigravity IDE"],
        &["antigravity-ide", "agy-ide"],
        Style::Goto,
    ),
    editor(
        "idea",
        &[
            "IntelliJ IDEA",
            "IntelliJ IDEA CE",
            "IntelliJ IDEA Ultimate",
        ],
        &["idea"],
        Style::JetBrains,
    ),
    editor("aqua", &["Aqua"], &["aqua"], Style::JetBrains),
    editor("clion", &["CLion"], &["clion"], Style::JetBrains),
    editor("datagrip", &["DataGrip"], &["datagrip"], Style::JetBrains),
    editor(
        "dataspell",
        &["DataSpell"],
        &["dataspell"],
        Style::JetBrains,
    ),
    editor("goland", &["GoLand"], &["goland"], Style::JetBrains),
    editor("phpstorm", &["PhpStorm"], &["phpstorm"], Style::JetBrains),
    editor(
        "pycharm",
        &["PyCharm", "PyCharm CE"],
        &["pycharm"],
        Style::JetBrains,
    ),
    editor(
        "rider",
        &["Rider", "JetBrains Rider"],
        &["rider"],
        Style::JetBrains,
    ),
    editor("rubymine", &["RubyMine"], &["rubymine"], Style::JetBrains),
    editor(
        "rustrover",
        &["RustRover"],
        &["rustrover"],
        Style::JetBrains,
    ),
    editor("webstorm", &["WebStorm"], &["webstorm"], Style::JetBrains),
    editor("file-manager", &["Finder"], &["open"], Style::Finder),
];

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            m.is_file()
        }
    })
}
struct Installed {
    path: Vec<PathBuf>,
    bundles: Vec<PathBuf>,
    toolbox: Option<PathBuf>,
}
impl Installed {
    fn host() -> Self {
        let mut bundles = Vec::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let roots = home
            .iter()
            .map(|h| h.join("Applications"))
            .chain(std::iter::once(PathBuf::from("/Applications")));
        for root in roots {
            let mut group: Vec<_> = fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "app"))
                .collect();
            group.sort();
            bundles.extend(group);
        }
        Self {
            path: std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|p| p.is_absolute())
                .collect(),
            bundles,
            toolbox: home.map(|h| h.join("Library/Application Support/JetBrains/Toolbox/scripts")),
        }
    }
    fn candidates(&self, e: &Editor) -> Vec<PathBuf> {
        if e.style == Style::Finder {
            return vec!["/usr/bin/open".into()];
        }
        let commands: Vec<_> = e
            .commands
            .iter()
            .flat_map(|cmd| self.path.iter().map(move |p| p.join(cmd)))
            .collect();
        let mut paths = Vec::new();
        if e.style != Style::Zed {
            paths.extend_from_slice(&commands)
        }
        for bundle in &self.bundles {
            let Some(name) = bundle.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !e.names.iter().any(|base| {
                name == *base
                    || name.strip_prefix(&format!("{base} ")).is_some_and(|v| {
                        v.as_bytes().first().is_some_and(u8::is_ascii_digit)
                            && v.bytes().all(|b| b == b'.' || b.is_ascii_digit())
                    })
            }) {
                continue;
            }
            let contents = bundle.join("Contents");
            match e.style {
                Style::Zed => paths.push(contents.join("MacOS/cli")),
                Style::JetBrains => paths.push(contents.join("MacOS").join(e.commands[0])),
                _ => {
                    paths.push(contents.join("Resources/app/bin").join(e.commands[0]));
                    paths.push(contents.join("Resources/app/bin/code"))
                }
            }
        }
        if e.style == Style::Zed {
            paths.extend(commands);
        }
        if e.style == Style::JetBrains
            && let Some(dir) = &self.toolbox
        {
            let cmd = e.commands[0];
            paths.push(dir.join(cmd));
            let mut numbered: Vec<_> = fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_prefix(cmd))
                        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                })
                .collect();
            numbered.sort_by_key(|p| (p.as_os_str().len(), p.clone()));
            paths.extend(numbered);
        }
        paths
    }
    async fn resolve(&self, e: &Editor) -> Option<PathBuf> {
        for path in self.candidates(e).into_iter().filter(|p| executable(p)) {
            if e.style == Style::Zed
                && path.file_name().is_some_and(|name| name == "zed")
                && !is_zed(&path).await
            {
                continue;
            }
            return Some(path);
        }
        None
    }
}
// OpenZFS also installs a 'zed' executable. Bound the bytes and time of the version probe.
async fn is_zed(path: &Path) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut child) = Command::new(path)
        .arg("--version")
        .env_remove("KAR_WORKBENCH_TOKEN")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    tokio::time::timeout(Duration::from_secs(2), async move {
        let mut prefix = [0u8; 3];
        stdout.read_exact(&mut prefix).await.is_ok()
            && &prefix == b"Zed"
            && child.wait().await.is_ok_and(|s| s.success())
    })
    .await
    .unwrap_or(false)
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Position {
    line: Option<std::num::NonZeroU32>,
    column: Option<std::num::NonZeroU32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    editor: Option<String>,
    path: PathBuf,
    position: Option<Position>,
    #[serde(default)]
    reveal: bool,
}
#[derive(Serialize)]
struct EditorInfo {
    id: &'static str,
    label: &'static str,
}
fn suffix(path: &Path) -> Option<(PathBuf, u32, Option<u32>)> {
    let (prefix, last) = path.to_str()?.rsplit_once(':')?;
    let last = last.parse().ok()?;
    if let Some((base, line)) = prefix.rsplit_once(':')
        && !base.is_empty()
        && let Ok(line) = line.parse()
    {
        Some((PathBuf::from(base), line, Some(last)))
    } else if !prefix.is_empty() {
        Some((PathBuf::from(prefix), last, None))
    } else {
        None
    }
}
fn platform() -> Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err(Error::Unsupported(
            "Kar editor launch is available on macOS".into(),
        ))
    }
}
fn args(
    e: &Editor,
    root: &Path,
    path: &Path,
    line: Option<u32>,
    column: Option<u32>,
    literal: bool,
    reveal: bool,
    from_path: bool,
) -> Result<Vec<OsString>> {
    if reveal && e.style != Style::Finder {
        return Err(Error::Refused("Reveal requires Finder".into()));
    }
    let mut args = Vec::new();
    if e.id == "cursor" {
        args.push("--classic".into())
    }
    if e.id == "kiro" && from_path {
        args.push("ide".into())
    }
    if e.style == Style::Finder {
        if reveal {
            if !path.try_exists()? {
                return Err(Error::Refused("Reveal target does not exist".into()));
            }
            args.push("-R".into());
        } else {
            // The selected application is Finder, including when the item is an application bundle.
            args.extend(["-a".into(), "Finder".into()]);
        }
        args.push(path.into());
        return Ok(args);
    }
    if e.style == Style::Goto && path.is_dir() {
        args.push("--new-window".into());
        args.push(path.into());
        return Ok(args);
    }
    if let Some(line) = line {
        if e.style == Style::JetBrains {
            args.extend([root.into(), "--line".into(), line.to_string().into()]);
            if let Some(col) = column {
                args.extend(["--column".into(), col.saturating_sub(1).to_string().into()])
            }
        } else {
            let mut positioned = path.as_os_str().to_owned();
            positioned.push(format!(":{line}"));
            if let Some(col) = column {
                positioned.push(format!(":{col}"));
            }
            let ambiguous = path
                .to_string_lossy()
                .split(':')
                .skip(1)
                .any(|v| v.parse::<u32>().is_ok());
            if !literal
                || !(ambiguous || (e.style == Style::Zed && Path::new(&positioned).exists()))
            {
                if e.style == Style::Goto {
                    args.push("--goto".into())
                }
                args.push(positioned);
                return Ok(args);
            }
        }
    }
    args.push(path.into());
    Ok(args)
}
async fn list() -> std::result::Result<Json<Vec<EditorInfo>>, ApiError> {
    platform()?;
    let installed = Installed::host();
    let mut out = vec![];
    for e in EDITORS {
        if installed.resolve(e).await.is_some() {
            out.push(EditorInfo {
                id: e.id,
                label: e.names[0],
            })
        }
    }
    Ok(Json(out))
}
async fn open(
    State(app): State<Workbench>,
    Param(id): Param<Uuid>,
    Json(q): Json<Open>,
) -> std::result::Result<Json<EditorInfo>, ApiError> {
    platform()?;
    let project = app.project(id)?;
    let workspace = app.workspace(&project)?;
    let e = EDITORS
        .iter()
        .find(|e| e.id == q.editor.as_deref().unwrap_or("file-manager"))
        .ok_or_else(|| Error::Refused("Unknown editor".into()))?;
    let relative = q.path.strip_prefix(&workspace.root).unwrap_or(&q.path);
    let resolve = |relative: &Path| -> Result<PathBuf> {
        if relative == Path::new(".") || relative.as_os_str().is_empty() {
            return Ok(workspace.root.clone());
        }
        Ok(workspace.root.join(workspace.target(relative, true)?))
    };
    let mut path = resolve(relative)?;
    let literal = q.position.is_some();
    let mut line = None;
    let mut column = None;
    if let Some(p) = q.position {
        line = p.line.map(|v| v.get());
        column = p.column.map(|v| v.get())
    } else if e.style != Style::Finder
        && !(e.style == Style::Goto && path.is_dir())
        && let Some((base, l, c)) = suffix(relative)
    {
        path = resolve(&base)?;
        line = Some(l);
        column = c;
    }
    let installed = Installed::host();
    let cmd = installed
        .resolve(e)
        .await
        .ok_or_else(|| Error::Refused(format!("{} launcher not found", e.names[0])))?;
    let argv = args(
        e,
        &workspace.root,
        &path,
        line,
        column,
        literal,
        q.reveal,
        installed
            .path
            .iter()
            .any(|p| Some(p.as_path()) == cmd.parent()),
    )?;
    let mut child = Command::new(cmd)
        .args(argv)
        .current_dir(&workspace.root)
        .env_remove("KAR_WORKBENCH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::from)?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(Json(EditorInfo {
        id: e.id,
        label: e.names[0],
    }))
}
pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/editors", get(list))
        .route("/projects/{project}/editor/open", post(open))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positioned_editor_argv_uses_literal_paths_and_jetbrains_zero_based_columns() {
        let root = Path::new("/repo with spaces");
        let file = root.join("src/one.rs");
        let by_id = |id| EDITORS.iter().find(|e| e.id == id).unwrap();
        assert_eq!(
            suffix(Path::new("src/one.rs:8:3")),
            Some(("src/one.rs".into(), 8, Some(3)))
        );
        assert_eq!(
            args(
                by_id("idea"),
                root,
                &file,
                Some(8),
                Some(3),
                true,
                false,
                false
            )
            .unwrap(),
            [
                root.as_os_str(),
                "--line".as_ref(),
                "8".as_ref(),
                "--column".as_ref(),
                "2".as_ref(),
                file.as_os_str()
            ]
        );
        assert_eq!(
            args(
                by_id("cursor"),
                root,
                &file,
                Some(8),
                None,
                true,
                false,
                true
            )
            .unwrap(),
            ["--classic", "--goto", "/repo with spaces/src/one.rs:8"]
        );
        let collision = root.join("literal:2");
        assert_eq!(
            args(
                by_id("vscode"),
                root,
                &collision,
                Some(8),
                None,
                true,
                false,
                true
            )
            .unwrap(),
            [collision.as_os_str()]
        );
    }
}
