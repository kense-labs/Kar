use super::*;
use std::collections::HashSet;
use tokio::fs;
mod scan;

const BRANDS: [&str; 5] = [".cursor", ".claude", ".codex", ".grok", ".agents"];

fn identity(path: &Path) -> String {
    let original = path.to_string_lossy().replace('\\', "/");
    let lower = original.to_ascii_lowercase();
    for brand in BRANDS {
        if let Some(at) = lower.find(&format!("/{brand}/")) {
            return format!("{}::{}", &original[..at], &original[at + brand.len() + 2..]);
        }
    }
    original
}
fn ignored(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    let parts: Vec<_> = text.split('/').collect();
    if let Some(at) = parts
        .windows(2)
        .position(|pair| BRANDS.contains(&pair[0]) && matches!(pair[1], "skills" | "skills-cursor"))
        && parts[at + 2..].iter().any(|p| p.starts_with('.'))
    {
        return true;
    }
    parts
        .iter()
        .rposition(|s| *s == ".codex")
        .is_some_and(|at| {
            parts.get(at + 1) == Some(&"skills")
                && parts.iter().rev().nth(1).is_some_and(|s| {
                    matches!(
                        *s,
                        "imagegen"
                            | "openai-docs"
                            | "opneai-docs"
                            | "plugin-creator"
                            | "skill-creator"
                            | "skill-installer"
                    )
                })
        })
}

fn frontmatter(input: &str) -> Result<Value> {
    let input = input.trim_start_matches('\u{feff}');
    let mut lines = input.lines();
    let lang = lines
        .next()
        .unwrap_or("")
        .strip_prefix("---")
        .ok_or_else(invalid)?
        .trim();
    if !matches!(lang, "" | "yaml" | "yml" | "json") {
        return Err(invalid());
    }
    let data = lines
        .take_while(|line| *line != "---")
        .collect::<Vec<_>>()
        .join("\n");
    if lang == "json" {
        return Ok(serde_json::from_str(&data)?);
    }
    let mut options = serde_saphyr::Options::default();
    options.strict_booleans = true;
    serde_saphyr::from_str_with_options(&data, options).map_err(|_| invalid())
}

fn project(path: &Path, raw: &str) -> Result<Option<SkillEntry>> {
    if raw.contains('\0') {
        return Err(invalid());
    }
    let data = frontmatter(raw)?;
    let surfaces = &data["metadata"]["surfaces"];
    let excluded = match surfaces {
        Value::Array(a) => !a.is_empty() && !a.iter().any(|s| s == "sdk"),
        Value::String(s) => {
            let mut surfaces = s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .peekable();
            surfaces.peek().is_some() && !surfaces.any(|s| s == "sdk")
        }
        _ => false,
    };
    if excluded || data["alwaysApply"] == true {
        return Ok(None);
    }
    let description = data["description"].as_str().unwrap_or("");
    let mut units = 0;
    let short: String = description
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 1536
        })
        .collect();
    let disabled = data["disable-model-invocation"] == true;
    if short.is_empty() && !disabled {
        return Err(invalid());
    }
    Ok(Some(SkillEntry {
        name: path
            .parent()
            .and_then(Path::file_name)
            .and_then(|v| v.to_str())
            .ok_or_else(invalid)?
            .to_owned(),
        path: Some(path.to_string_lossy().into_owned()),
        description: Some(short),
        disable_model_invocation: Some(disabled),
        ..Default::default()
    }))
}

pub(super) async fn skills(rt: &Runtime, probe: &Probe) -> Result<InventoryResult<SkillEntry>> {
    let home = crate::account::profile_home(probe).ok_or_else(invalid)?;
    let cwd = &probe.cwd;
    let mut partial = false;
    let mut roots = vec![cwd.join(".cursor/rules")];
    for base in [cwd, &home] {
        roots.extend(BRANDS.iter().map(|b| base.join(b).join("skills")));
    }
    let mut files = Vec::new();
    for root in roots {
        files.extend(scan::direct(root, &mut partial).await?);
    }
    files.extend(scan::nested(rt, probe, &home, &mut partial).await?);
    let canonical = async |path: PathBuf| fs::canonicalize(&path).await.unwrap_or(path);
    let cwd_real = canonical(cwd.clone()).await;
    let home_real = canonical(home.clone()).await;
    let managed = canonical(home.join(".cursor/skills-cursor")).await;
    let mut paths = HashSet::new();
    let mut keys = HashSet::new();
    let mut items = Vec::new();
    for file in files {
        if ignored(&file) {
            continue;
        }
        let real = canonical(file.clone()).await;
        if (!real.starts_with(&cwd_real) && !real.starts_with(&home_real))
            || real.starts_with(&managed)
            || !paths.insert(real.clone())
        {
            continue;
        }
        let key = identity(&file);
        if keys.contains(&key) {
            continue;
        }
        let raw = tokio::task::spawn_blocking(move || {
            crate::account::read_profile(&real, 16 * 1024 * 1024)
        })
        .await
        .map_err(|_| invalid())?;
        let entry = match raw
            .ok()
            .flatten()
            .ok_or_else(invalid)
            .and_then(|s| project(&file, &s))
        {
            Ok(None) => continue,
            Err(_) => {
                partial = true;
                None
            }
            Ok(v) => v,
        };
        keys.insert(key);
        if let Some(mut item) = entry {
            let user = BRANDS
                .iter()
                .any(|brand| file.starts_with(home.join(brand).join("skills")));
            item.source = Some(if user { "user" } else { "workspace" }.into());
            item.scope = Some(if user {
                SkillScope::User
            } else {
                SkillScope::Project
            });
            items.push(item);
        }
    }
    items.sort_by_key(|item| item.scope == Some(SkillScope::Project));
    Ok(InventoryResult::observed(
        cwd,
        items,
        partial,
        View::Discovered,
    ))
}
