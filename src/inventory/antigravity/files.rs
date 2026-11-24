use super::{manifests::*, *};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use tokio::fs;

const BUILTINS: [&str; 6] = [
    "agy-customizations",
    "antigravity_guide",
    "automation",
    "migrate-agent-settings",
    "migrate-workflows",
    "run-workflow",
];

#[derive(Default, Deserialize)]
struct Header {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model: Option<Value>,
    #[serde(rename = "disable-slash-command")]
    hidden: Option<Value>,
}
impl Header {
    fn flag(value: Option<Value>) -> Option<bool> {
        match value {
            None => Some(false),
            Some(v) => v.as_bool(),
        }
    }
}
fn header(contents: &str) -> Option<Header> {
    let source = contents.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let mut lines = source.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut text = String::new();
    for line in lines {
        if line == "---" {
            return serde_saphyr::from_str::<Option<Header>>(&text)
                .ok()
                .map(Option::unwrap_or_default);
        }
        text.push_str(line);
        text.push('\n');
    }
    None
}
async fn workspace_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut ancestors = Vec::new();
    for dir in cwd.ancestors() {
        ancestors.push(dir.to_owned());
        let git = dir.join(".git");
        let root = fs::try_exists(git.join("HEAD")).await.unwrap_or(false)
            || if let Some(record) = read(&git, 64 * 1024).await
                && let Some(path) = record.trim_end().strip_prefix("gitdir: ")
            {
                !path.contains(['\r', '\n'])
                    && fs::try_exists(dir.join(path).join("HEAD"))
                        .await
                        .unwrap_or(false)
            } else {
                false
            };
        if root {
            ancestors.reverse();
            return ancestors;
        }
    }
    vec![cwd.to_owned()]
}

struct Scan {
    enabled: Value,
    seen: HashSet<[u8; 32]>,
    winners: HashMap<String, Option<SkillEntry>>,
}

impl Scan {
    async fn add(
        &mut self,
        dir: PathBuf,
        root: Option<&Path>,
        plugin: Option<(&str, &str)>,
        builtin: bool,
    ) {
        let path = dir.join("SKILL.md");
        if !allowed(&path, root).await {
            return;
        }
        let Some(contents) = read(&path, 16 * 1024 * 1024).await else {
            return;
        };
        let Some(meta) = header(&contents) else {
            return;
        };
        let Some(hidden) = Header::flag(meta.hidden) else {
            return;
        };
        let Some(disable) = Header::flag(meta.disable_model) else {
            return;
        };
        let Some(declared) = meta
            .name
            .as_deref()
            .map(|s| s.trim_end_matches('\n'))
            .filter(|s| !s.is_empty())
            .or_else(|| dir.file_name()?.to_str())
        else {
            return;
        };
        let name = match plugin {
            Some((id, _)) => format!("{id}:{declared}"),
            None => declared.to_owned(),
        };
        if !self.seen.insert(Sha256::digest(contents.as_bytes()).into()) {
            return;
        }
        self.winners.entry(name.clone()).or_insert_with(|| {
            (!hidden).then(|| SkillEntry {
                name,
                description: meta
                    .description
                    .map(|s| s.trim_end_matches('\n').to_string())
                    .filter(|s| !s.is_empty()),
                path: Some(path.to_string_lossy().into_owned()),
                scope: builtin.then_some(SkillScope::Builtin),
                plugin: plugin.map(|(_, label)| label.to_owned()),
                disable_model_invocation: Some(disable),
                user_invocable: Some(true),
                ..Default::default()
            })
        });
    }

    async fn plugin(&mut self, path: PathBuf, root: Option<&Path>) {
        let Some(id) = path.file_name().and_then(|s| s.to_str()) else {
            return;
        };
        let mut meta = None;
        for file in [
            "plugin.json",
            ".antigravity-plugin/plugin.json",
            ".gemini-plugin/plugin.json",
        ] {
            let candidate = path.join(file);
            if fs::try_exists(&candidate).await.unwrap_or(false) {
                if allowed(&candidate, root).await {
                    meta = json(&candidate).await;
                }
                break;
            }
        }
        let Some(meta) = meta else {
            return;
        };
        if ["name", "version", "description"]
            .iter()
            .any(|k| !meta[k].is_null() && !meta[k].is_string())
            || (!meta["disabled"].is_null() && !meta["disabled"].is_boolean())
        {
            return;
        }
        if !self.enabled[id]["enabled"]
            .as_bool()
            .unwrap_or_else(|| meta["disabled"] != true)
        {
            return;
        }
        let label = meta["name"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(id);
        for dir in dirs(&path.join("skills"), root).await {
            self.add(dir, root, Some((id, label)), false).await;
        }
    }

    async fn collect(&mut self, paths: Vec<PathBuf>, kind: &str, root: Option<&Path>) {
        for path in paths {
            if kind == "skills" {
                self.add(path, root, None, false).await;
            } else {
                self.plugin(path, root).await;
            }
        }
    }
    async fn scan_dir(&mut self, path: &Path, root: Option<&Path>) {
        for kind in ["skills", "plugins"] {
            self.collect(dirs(&path.join(kind), root).await, kind, root)
                .await;
        }
    }
}

pub(super) async fn discover(cwd: &Path, home: &Path) -> Result<Vec<SkillEntry>> {
    let cwd = fs::canonicalize(cwd).await?;
    let projects = workspace_dirs(&cwd).await;
    let root = Some(projects[0].as_path());
    let config = home.join(".gemini/config");
    let legacy = home.join(".gemini/antigravity-cli");
    let mut scan = Scan {
        enabled: json(&config.join("config.json")).await.unwrap_or_default()["plugins"].take(),
        seen: Default::default(),
        winners: Default::default(),
    };
    let locations: Vec<_> = projects
        .iter()
        .flat_map(|base| {
            [".agent", ".agents", "_agent", "_agents"]
                .into_iter()
                .map(move |alias| (base, base.join(alias)))
        })
        .collect();
    for (_, path) in &locations {
        scan.scan_dir(path, root).await;
    }
    for (base, path) in &locations {
        for kind in ["skills", "plugins"] {
            scan.collect(
                manifest(path.join(format!("{kind}.json")), base, root).await,
                kind,
                root,
            )
            .await;
        }
    }
    for path in [&legacy, &config, &home.join(".gemini")] {
        scan.scan_dir(path, None).await;
    }
    for kind in ["skills", "plugins"] {
        let name = format!("{kind}.json");
        let old = legacy.join(&name);
        let path = if fs::try_exists(&old).await.unwrap_or(false) {
            old
        } else {
            config.join(name)
        };
        scan.collect(manifest(path, &config, None).await, kind, None)
            .await;
    }
    for name in BUILTINS {
        scan.add(legacy.join("builtin/skills").join(name), None, None, true)
            .await;
    }
    let mut items: Vec<_> = scan.winners.into_values().flatten().collect();
    items.sort_by(|a, b| a.name.encode_utf16().cmp(b.name.encode_utf16()));
    Ok(items)
}
