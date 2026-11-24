use super::*;
use indexmap::IndexMap;
use std::collections::HashMap;

fn scope(source: Option<&str>) -> Option<SkillScope> {
    use SkillScope::*;
    Some(match source? {
        "built-in" => Builtin,
        "userSettings" => User,
        "projectSettings" | "localSettings" => Project,
        "policySettings" => Managed,
        "plugin" | "flagSettings" | "memoryStore" | "syncedSkills" => Other,
        _ => return None,
    })
}
fn label(description: &str) -> (&str, Option<SkillScope>) {
    for (suffix, scope) in [
        (" (project)", SkillScope::Project),
        (" (user)", SkillScope::User),
    ] {
        if let Some(text) = description.strip_suffix(suffix) {
            return (text, Some(scope));
        }
    }
    (description, None)
}
fn stem(path: &str) -> &str {
    let mut parts = path.rsplit(['/', '\\']);
    let base = parts.next().unwrap_or("");
    if base == "SKILL.md" {
        parts.next().unwrap_or("")
    } else {
        base.strip_suffix(".md").unwrap_or(base)
    }
}
async fn declaration(file: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let raw = crate::account::read_profile(&file, 16 * 1024 * 1024).ok()??;
        if raw.contains('\0') {
            return None;
        }
        let mut lines = raw.trim_start_matches('\u{feff}').lines();
        if lines.next()?.trim_end() != "---" {
            return None;
        }
        let mut header = String::new();
        for line in lines {
            if line.trim_end() == "---" {
                #[derive(Deserialize)]
                struct Declaration {
                    #[serde(default)]
                    description: Value,
                    #[serde(default, rename = "user-invocable")]
                    user_invocable: Value,
                }
                let front: Declaration = serde_saphyr::from_str(&header).ok()?;
                if front.user_invocable == false {
                    return None;
                }
                return front
                    .description
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
            header.push_str(line);
            header.push('\n');
        }
        None
    })
    .await
    .ok()
    .flatten()
}
async fn owns_command(skill: &SkillEntry, command: &Value, cwd: &Path) -> bool {
    let aliases = command["aliases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let Some(path) = &skill.path else {
        return aliases.is_empty();
    };
    let Some(desc) = command["description"].as_str() else {
        return false;
    };
    let (text, scope) = label(desc);
    if scope.is_some() && scope != skill.scope {
        return false;
    }
    let file = stem(path);
    let matches = if aliases.is_empty() {
        skill.name.rsplit(':').next() == Some(file)
    } else {
        aliases.contains(&file)
    };
    matches && declaration(cwd.join(path)).await.is_some_and(|d| d == text)
}
pub(super) async fn catalog(
    data: &Value,
    init: &Value,
    cwd: &Path,
) -> Result<(Vec<SkillEntry>, bool)> {
    let skills = &data["skills"];
    let partial = matches!((skills["includedSkills"].as_f64(),skills["totalSkills"].as_f64()),(Some(i),Some(t)) if i<t);
    let mut items = rows(&skills["skillFrontmatter"])?
        .iter()
        .map(|r| {
            let source = field(r, "source");
            Ok(SkillEntry {
                name: name(r)?,
                scope: scope(source.as_deref()),
                source,
                plugin: field(r, "pluginName"),
                path: field(r, "path"),
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(commands) = init["commands"].as_array() else {
        return Ok((items, partial));
    };
    let mut named = IndexMap::new();
    for r in commands {
        if let Some(name) = r["name"].as_str() {
            named
                .entry(name)
                .and_modify(|e| *e = None)
                .or_insert(Some(r));
        }
    }
    let mut known = HashMap::<String, usize>::new();
    for skill in &items {
        *known.entry(skill.name.clone()).or_default() += 1;
    }
    for skill in &mut items {
        if known[&skill.name] > 1 {
            continue;
        }
        match named.get(skill.name.as_str()) {
            Some(Some(cmd)) => {
                let own = if cmd["builtin"] == true {
                    skill.scope == Some(SkillScope::Builtin)
                } else {
                    owns_command(skill, cmd, cwd).await
                };
                if own {
                    skill.user_invocable = Some(true);
                    skill.description = cmd["description"]
                        .as_str()
                        .map(|s| {
                            let (text, scope) = label(s);
                            if scope == skill.scope { text } else { s }
                        })
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                }
            }
            None if matches!(
                skill.source.as_deref(),
                Some("userSettings" | "projectSettings")
            ) =>
            {
                skill.user_invocable = Some(false)
            }
            _ => {}
        }
    }
    for (name, cmd) in named {
        let Some(cmd) = cmd else { continue };
        if known.contains_key(name) || cmd["builtin"] == true {
            continue;
        }
        if let Some((text, Some(scope))) = cmd["description"].as_str().map(label) {
            items.push(SkillEntry {
                name: name.into(),
                scope: Some(scope),
                description: (!text.is_empty()).then(|| text.into()),
                user_invocable: Some(true),
                ..Default::default()
            });
        }
    }
    Ok((items, partial))
}
