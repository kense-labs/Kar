use super::{
    Child, Launch, MAX_STEP, launches,
    profile::{self, Profile},
    valid_id,
};
use cap_std::fs::Dir;
use serde::Deserialize;
use serde_json::Value;
use std::{ffi::OsString, io, path::Path};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    sender: String,
    content: Option<String>,
    source_metadata: Option<Source>,
}
#[derive(Deserialize)]
struct Source {
    tool: Option<Tool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tool {
    conversation_id: Option<String>,
    tool_call: Option<Call>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Call {
    id: String,
    name: String,
    arguments_json: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    conversation_id: Option<String>,
    subagent_descriptor: Option<Role>,
}
#[derive(Deserialize)]
struct Role {
    role: Option<String>,
}
fn names(dir: &Dir, max: usize, accept: impl Fn(&str) -> bool) -> Option<Vec<OsString>> {
    let mut names = vec![];
    for entry in dir.entries().ok()? {
        let name = entry.ok()?.file_name();
        if name.to_str().is_some_and(&accept) {
            if names.len() == max {
                return None;
            }
            names.push(name);
        }
    }
    names.sort();
    Some(names)
}
fn json<T: serde::de::DeserializeOwned>(dir: &Dir, name: impl AsRef<Path>) -> Option<T> {
    serde_json::from_slice(&profile::bytes(profile::file(dir, name).ok()?, MAX_STEP as u64).ok()?)
        .ok()
}
pub(super) fn read(p: &Profile, root: &str, id: &str) -> Option<Vec<Child>> {
    let children = p
        .dir(&["brain", root, ".system_generated", "subagents"])
        .ok()?;
    let ids = names(&children, 64, |name| {
        name.strip_suffix(".json").is_some_and(valid_id)
    })?;
    let mut found = None;
    for descriptor in ids {
        let child = descriptor.to_str()?.strip_suffix(".json")?;
        let inbox = match p.dir(&["brain", child, ".system_generated", "messages"]) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let mut launch = None;
        for name in names(&inbox, 256, |n| n.ends_with(".json") && n != "read.json")? {
            let message: Message = json(&inbox, name)?;
            if message.sender != root {
                continue;
            }
            let Some(tool) = message.source_metadata.and_then(|s| s.tool) else {
                continue;
            };
            if tool.conversation_id.as_deref().is_some_and(|r| r != root) {
                continue;
            }
            let Some(call) = tool.tool_call else {
                continue;
            };
            if call.id == id && call.name == "invoke_subagent" && launch.is_none() {
                let args = call
                    .arguments_json
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .as_ref()
                    .and_then(launches)
                    .and_then(|ls| ls.into_iter().next());
                launch = Some((args, message.content));
            }
        }
        if let Some((args, content)) = launch {
            let meta: Descriptor = json(&children, &descriptor)?;
            if found.is_some() || meta.conversation_id.as_deref().is_some_and(|c| c != child) {
                return None;
            }
            let launch = args.unwrap_or_else(|| Launch {
                role: meta
                    .subagent_descriptor
                    .and_then(|d| d.role)
                    .unwrap_or_default(),
                prompt: content.unwrap_or_default(),
            });
            found = Some(Child {
                id: child.into(),
                uri: None,
                launch,
            });
        }
    }
    found.map(|c| vec![c])
}
