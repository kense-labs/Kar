use super::*;
use crate::inventory::{InventoryOptions, InventoryResult, SkillScope, View};

#[derive(Debug, Default, Serialize)]
pub struct Customization {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<usize>,
}

fn count<T>(
    report: InventoryResult<T>,
    source: impl Fn(&T) -> Option<bool>,
) -> Result<Option<usize>> {
    match report {
        InventoryResult::Unavailable { .. } | InventoryResult::Ok { partial: true, .. } => {
            Err(Error::Protocol("incomplete customization inventory".into()))
        }
        InventoryResult::Ok {
            view: View::McpOnly | View::Context,
            ..
        }
        | InventoryResult::Unsupported { .. } => Ok(None),
        InventoryResult::Ok { items, .. } => Ok(items
            .iter()
            .try_fold(0, |n, item| Some(n + usize::from(source(item)?)))),
    }
}
fn user(source: Option<&str>) -> Option<bool> {
    match source? {
        "user" => Some(true),
        "builtin" | "project" | "local" => Some(false),
        _ => None,
    }
}

pub(super) async fn read(
    rt: &Runtime,
    config: &InstallationOptions,
    budget: &Budget<'_>,
) -> Probe<Customization> {
    let root = match tempfile::Builder::new().prefix("kar-orient-").tempdir() {
        Ok(root) => root,
        Err(_) => return Probe::failed(),
    };
    let Ok(cwd) = root.path().canonicalize() else {
        return Probe::failed();
    };
    let options = |timeout| InventoryOptions {
        installation: InstallationOptions {
            cwd: Some(cwd.clone()),
            env: config.env.clone(),
        },
        timeout: Some(timeout),
    };
    let (skills, commands, mcps, tools) = tokio::join!(
        budget.read(true, async |t| {
            count(rt.skills(&options(t)).await?, |r| {
                r.scope.as_ref().map(|s| s == &SkillScope::User)
            })
        }),
        budget.read(true, async |t| {
            count(rt.commands(&options(t)).await?, |r| {
                user(r.source.as_deref())
            })
        }),
        budget.read(true, async |t| {
            count(rt.mcp_servers(&options(t)).await?, |r| {
                user(r.source.as_deref())
            })
        }),
        budget.read(true, async |t| {
            count(rt.tools(&options(t)).await?, |r| user(r.source.as_deref()))
        }),
    );
    let results = [skills, commands, mcps, tools];
    if results.iter().any(|r| matches!(r, Probe::TimedOut { .. })) {
        return Probe::expired();
    }
    if results.iter().any(|r| matches!(r, Probe::Failed { .. })) {
        return Probe::Failed {
            reason: "A native inventory failed or was incomplete",
            observed_at: now(),
        };
    }
    let [skills, commands, mcp_servers, tools] = results.map(|r| match r {
        Probe::Ok { value, .. } => value,
        _ => None,
    });
    if [skills, commands, mcp_servers, tools]
        .iter()
        .all(Option::is_none)
    {
        return Probe::Unsupported;
    }
    Probe::ok(Customization {
        skills,
        commands,
        mcp_servers,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_complete_attributed_native_lists_can_publish_a_count() {
        let items = |view, partial, items| InventoryResult::Ok {
            scope: crate::inventory::InventoryScope::Workspace { cwd: ".".into() },
            observed_at: String::new(),
            view,
            items,
            partial,
        };
        for mixed in [vec![Some("plugin")], vec![Some("user"), None]] {
            assert_eq!(
                count(items(View::Discovered, false, mixed), |label| user(*label)).unwrap(),
                None
            );
        }
        for view in [View::Context, View::McpOnly] {
            assert_eq!(
                count(items(view, false, vec![Some("user")]), |v| user(*v)).unwrap(),
                None
            );
        }
        let known = vec![
            Some("user"),
            Some("builtin"),
            Some("project"),
            Some("local"),
        ];
        assert_eq!(
            count(items(View::Registered, false, known.clone()), |v| user(*v)).unwrap(),
            Some(1)
        );
        assert!(count(items(View::Registered, true, known), |v| user(*v)).is_err());
    }
}
