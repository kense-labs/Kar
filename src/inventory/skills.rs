use super::*;
use crate::{Catalog, Inventory, account::AccountWire as Wire, process::OwnedProcess};
use reqwest::Method;
use serde_json::{Value, json};
use std::process::Stdio;
type Listing = (Vec<SkillEntry>, bool);
fn basic(
    row: &Value,
    path: &str,
    source: &str,
    scopes: &[(&str, SkillScope)],
) -> Result<SkillEntry> {
    let source = field(row, source);
    let scope = scopes
        .iter()
        .find(|(s, _)| Some(*s) == source.as_deref())
        .map(|(_, s)| *s);
    Ok(SkillEntry {
        name: field(row, "name").ok_or_else(invalid)?,
        description: field(row, "description"),
        path: field(row, path),
        source,
        scope,
        ..Default::default()
    })
}

fn kimi_skill(row: &Value) -> Result<SkillEntry> {
    use SkillScope::*;
    let mut item = basic(
        row,
        "path",
        "source",
        &[
            ("builtin", Builtin),
            ("user", User),
            ("project", Project),
            ("extra", Other),
        ],
    )?;
    item.disable_model_invocation = row["disable_model_invocation"].as_bool();
    Ok(item)
}
async fn kimi(probe: &Probe, path: &Path, end: Instant) -> Result<Listing> {
    let home = probe
        .get("KIMI_CODE_HOME")
        .filter(|s| !s.is_empty())
        .map(|p| probe.cwd.join(p))
        .or_else(|| crate::account::profile_home(probe).map(|p| p.join(".kimi-code")))
        .ok_or_else(invalid)?;
    let mut web = crate::kimi::QueryWeb::for_query(
        probe,
        path,
        &home,
        end.saturating_duration_since(Instant::now()),
    )
    .await?;
    let result = async {
        let (id, owned) = web.workspace(&probe.cwd, None).await?;
        let resource = format!("/workspaces/{}", crate::kimi::workspace_segment(&id));
        let read = web
            .request(Method::GET, &format!("{resource}/skills"), None)
            .await;
        if owned {
            web.request(Method::DELETE, &resource, None).await?;
        }
        let items = rows(&read?["skills"])?
            .iter()
            .map(kimi_skill)
            .collect::<Result<_>>()?;
        Ok((items, false))
    }
    .await;
    let _ = web.stop().await;
    result
}

pub(super) async fn read(id: &str, probe: &Probe, path: &Path, end: Instant) -> Result<Listing> {
    use SkillScope::*;
    let mut cmd = probe.command(path);
    match id {
        "pi" => pi::skills(probe, path).await,
        "kimi" => kimi(probe, path, end).await,
        "claude" => claude::skills(probe, path).await,
        "grok" => grok::skills(probe, path).await,
        "factory" => {
            let r = crate::factory::inventory_command(
                cmd,
                Catalog::Inventory(Inventory::Skills),
                end.saturating_duration_since(Instant::now()),
            )
            .await?;
            let items = rows(&r["skills"])?
                .iter()
                .map(|r| {
                    let mut entry = basic(
                        r,
                        "filePath",
                        "location",
                        &[
                            ("personal", User),
                            ("project", Project),
                            ("builtin", Builtin),
                            ("automation", Other),
                        ],
                    )?;
                    entry.plugin = field(r, "pluginId");
                    entry.enabled = r["enabled"].as_bool();
                    entry.user_invocable = r["userInvocable"].as_bool();
                    Ok(entry)
                })
                .collect::<Result<_>>()?;
            if r.get("projectAvailable").is_some_and(|v| !v.is_boolean()) {
                return Err(invalid());
            }
            Ok((items, r["projectAvailable"] == false))
        }
        "codex" => {
            let mut wire = codex::connect(cmd).await?;
            let frame = wire
                .codex(2, "skills/list", json!({"cwds":[probe.cwd]}))
                .await;
            wire.finish().await;
            let result = rpc(frame?, "skills/list")?;
            let mut items = Vec::new();
            let mut partial = false;
            for batch in rows(&result["data"])? {
                partial |= batch["errors"].as_array().is_some_and(|r| !r.is_empty());
                for row in rows(&batch["skills"])? {
                    let mut entry = basic(
                        row,
                        "path",
                        "scope",
                        &[
                            ("system", Builtin),
                            ("user", User),
                            ("repo", Project),
                            ("admin", Managed),
                        ],
                    )?;
                    entry.plugin = field(row, "pluginId");
                    entry.enabled = row["enabled"].as_bool();
                    items.push(entry);
                }
            }
            Ok((items, partial))
        }
        "qwen" => {
            cmd.arg("--acp");
            let mut wire = Wire::open(cmd)?;
            wire.initialize(Wire::capabilities()).await?;
            let method = "qwen/status/workspace/skills";
            let result = rpc(wire.rpc(2, method, json!({})).await?, method)?;
            if result["v"] != 1
                || result["initialized"] != true
                || !result["workspaceCwd"].is_string()
            {
                return Err(invalid());
            }
            let partial = match result.get("errors") {
                Some(v) => {
                    let r = rows(v)?;
                    if !r.iter().all(Value::is_object) {
                        return Err(invalid());
                    }
                    !r.is_empty()
                }
                None => false,
            };
            let items = rows(&result["skills"])?
                .iter()
                .map(|r| {
                    let model = r["modelInvocable"].as_bool().ok_or_else(invalid)?;
                    if r["kind"] != "skill"
                        || !r["level"].is_string()
                        || !matches!(r["status"].as_str(), Some("ok" | "disabled"))
                    {
                        return Err(invalid());
                    }
                    let mut entry = basic(
                        r,
                        "installedPath",
                        "level",
                        &[
                            ("bundled", Builtin),
                            ("user", User),
                            ("project", Project),
                            ("extension", Other),
                        ],
                    )?;
                    entry.plugin = field(r, "extensionName");
                    entry.user_invocable = r["userInvocable"].as_bool();
                    entry.enabled = Some(r["status"] != "disabled");
                    entry.disable_model_invocation = Some(!model);
                    Ok(entry)
                })
                .collect::<Result<_>>()?;
            Ok((items, partial))
        }
        "devin" => {
            cmd.args(["skills", "list", "--json"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let out = OwnedProcess::spawn(cmd)?.output().await?;
            if !out.status.success() {
                return Err(invalid());
            }
            let data: Value = serde_json::from_slice(&out.stdout)?;
            let mut partial = false;
            let mut items = Vec::new();
            for r in rows(&data)? {
                let mut entry = basic(r, "base_dir", "provider", &[("Builtin", Builtin)])?;
                entry.path = entry.path.filter(|s| !s.is_empty());
                for key in ["warnings", "errors", "triggers"] {
                    if let Some(value) = r.get(key) {
                        let strings = rows(value)?;
                        if !strings.iter().all(Value::is_string) {
                            return Err(invalid());
                        }
                        if key == "errors" {
                            partial |= !strings.is_empty();
                        }
                        if key == "triggers" {
                            entry.disable_model_invocation =
                                Some(!strings.iter().any(|v| v == "model"));
                            entry.user_invocable = Some(strings.iter().any(|v| v == "user"));
                        }
                    }
                }
                items.push(entry);
            }
            Ok((items, partial))
        }
        _ => Err(Error::Unsupported("Independent skill discovery".into())),
    }
}
